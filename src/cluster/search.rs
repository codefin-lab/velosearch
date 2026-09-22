//! A search over indices held on several nodes: the coordinator asks each
//! node for its page and its aggregation intermediates, merges the pages
//! by the request's sort, merges the intermediates, and finishes the
//! aggregations once. A node that does not answer is a set of failed
//! shards in `_shards`, and the answer is partial unless
//! `allow_partial_search_results=false` asked for the whole or nothing.
//!
//! A copy is a copy of the index, so a request that carries one of the
//! engine's own aggregations (the ones computed as searches of their own)
//! runs whole on one node holding every index it names when there is one;
//! there is not always one, and then the request is refused rather than
//! answered wrong.
//!
//! A scroll over such a search is driven from here: a point in time on
//! every node, and how far into each the scroll has read; each page asks
//! every node for its next `size` and takes the first `size` of the merge.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use super::runtime::DataFuture;
use super::state::{ClusterState, ShardState};
use super::transport::{Envelope, Kind, NodeId};
use crate::api::Params;
use crate::search::Outcome;
use crate::store::Store;

pub const SEARCH: &str = "indices:data/read/search";
pub const PIT: &str = "indices:data/read/point_in_time";

/// Where the indices of a request are: this node's own, and the rest by
/// the node that answers for them.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub local: Vec<String>,
    pub remote: BTreeMap<NodeId, Vec<String>>,
    /// indices the cluster knows but no node holds an active copy of
    pub unavailable: Vec<String>,
    /// every index, in the order the request named them
    pub all: Vec<String>,
}

impl Plan {
    pub fn spans_nodes(&self) -> bool {
        !self.remote.is_empty() || !self.unavailable.is_empty()
    }
}

/// The indices an expression names, as the cluster knows them.
fn resolve(state: &ClusterState, store: &Store, expr: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let expr = if expr.is_empty() { "_all" } else { expr };
    for part in expr.split(',').map(|p| p.trim()).filter(|p| !p.is_empty()) {
        let neg = part.starts_with('-');
        let p = part.trim_start_matches('-');
        let names: Vec<String> = if p == "_all" || p == "*" {
            state.indices.keys().cloned().collect()
        } else if p.contains('*') {
            state.indices.keys().filter(|n| crate::store::glob_match(p, n)).cloned().collect()
        } else if state.indices.contains_key(p) {
            vec![p.to_string()]
        } else {
            // an alias the cluster published, whose indices may be anywhere,
            // and only then what this node happens to hold under that name
            let by_alias: Vec<String> = state
                .indices
                .iter()
                .filter(|(_, m)| m.aliases.get(p).is_some())
                .map(|(n, _)| n.clone())
                .collect();
            if by_alias.is_empty() { store.resolve(p) } else { by_alias }
        };
        if neg {
            out.retain(|n| !names.contains(n));
        } else {
            for n in names {
                if !out.contains(&n) {
                    out.push(n);
                }
            }
        }
    }
    out
}

/// A copy is a copy of the index: a node holding any active copy of it
/// can answer for the whole of it.
fn active_here(state: &ClusterState, node: &NodeId, index: &str) -> bool {
    state.indices.contains_key(index)
        && state.routing.shards_of(index).any(|c| {
            c.node.as_ref() == Some(node)
                && matches!(c.state, ShardState::Started | ShardState::Relocating)
        })
}

/// Which node answers for each index this request names; `None` when
/// every one is here (or the cluster is one node), so the search runs as
/// it always did.
pub fn plan(store: &Store, expr: &str, preference: Option<&str>) -> Option<Plan> {
    let rt = super::runtime()?;
    let me = rt.local();
    super::with_state(|s| {
        if s.version == 0 || s.nodes.len() <= 1 {
            return None;
        }
        let all = resolve(s, store, expr);
        if all
            .iter()
            .all(|i| active_here(s, &me, i) || store.get(i).is_some() && !s.indices.contains_key(i))
        {
            return None;
        }
        let mut plan = Plan { all: all.clone(), ..Plan::default() };
        // a custom preference string picks the same copy every time
        let seed: u64 = preference
            .filter(|p| !p.starts_with('_'))
            .map(|p| {
                p.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
                    (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
                })
            })
            .unwrap_or(0);
        let only_nodes: Option<Vec<String>> = preference
            .and_then(|p| p.strip_prefix("_only_nodes:"))
            .map(|l| l.split(',').map(|x| x.trim().to_string()).collect());
        let prefer_local = preference.map(|p| p == "_local" || p == "_only_local").unwrap_or(true);
        for index in all {
            let holders: Vec<NodeId> = s
                .nodes
                .keys()
                .filter(|n| active_here(s, n, &index))
                .filter(|n| {
                    only_nodes
                        .as_ref()
                        .map(|o| {
                            o.iter().any(|x| {
                                x == n.as_str()
                                    || s.nodes.get(*n).map(|d| d.name == *x).unwrap_or(false)
                            })
                        })
                        .unwrap_or(true)
                })
                .cloned()
                .collect();
            if holders.is_empty() {
                if s.indices.contains_key(&index) {
                    // the cluster has it and no node can answer for it
                    plan.unavailable.push(index);
                } else {
                    // the local run reports it as it always did
                    plan.local.push(index);
                }
                continue;
            }
            let pick = if prefer_local && holders.contains(&me) {
                me.clone()
            } else if seed != 0 {
                holders[(seed as usize) % holders.len()].clone()
            } else {
                // the node holding the primary, then by id: the same choice
                // for the same routing, so a scroll's pages agree
                s.routing
                    .primary(&index, 0)
                    .and_then(|p| p.node.clone())
                    .filter(|p| holders.contains(p))
                    .unwrap_or_else(|| holders[0].clone())
            };
            if pick == me {
                plan.local.push(index);
            } else {
                plan.remote.entry(pick).or_default().push(index);
            }
        }
        Some(plan)
    })
}

/// A node's answer for its indices: the outcome, or why not.
///
/// An outcome is much larger than a reason, and boxing it would put an
/// allocation in front of every answer to make the rare refusal cheaper --
/// which is the wrong way round.
#[allow(clippy::large_enum_variant)]
enum Reply {
    Ok(Outcome),
    Failed(String),
}

/// The answer a node refused with, when it carried one.
fn refusal(why: &str) -> Option<axum::response::Response> {
    let v: Value = serde_json::from_str(why).ok()?;
    let status = v.get("status").and_then(|s| s.as_u64())? as u16;
    let body = v.get("body").and_then(|b| b.as_str())?.to_string();
    let mut r = axum::response::Response::new(axum::body::Body::from(body));
    *r.status_mut() =
        axum::http::StatusCode::from_u16(status).unwrap_or(axum::http::StatusCode::BAD_REQUEST);
    r.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    Some(r)
}

fn sort_keys_of(body: &Value) -> Vec<(bool, Option<bool>)> {
    // (desc, missing_last) per key, as `parse_sort` would read them
    let Some(sort) = body.get("sort") else { return Vec::new() };
    let items: Vec<Value> = match sort {
        Value::Array(a) => a.clone(),
        other => vec![other.clone()],
    };
    items
        .iter()
        .map(|it| match it {
            Value::String(s) => (s == "_score", None),
            Value::Object(o) => {
                let (field, spec) =
                    o.iter().next().map(|(k, v)| (k.clone(), v.clone())).unwrap_or_default();
                // `{"n": "desc"}` and `{"n": {"order": "desc"}}` say the same
                let order = spec
                    .as_str()
                    .map(|s| s.to_string())
                    .or_else(|| spec.get("order").and_then(|v| v.as_str()).map(|s| s.to_string()));
                let desc = match order.as_deref() {
                    Some("desc") => true,
                    Some("asc") => false,
                    _ => field == "_score",
                };
                let missing_last =
                    spec.get("missing").and_then(|m| m.as_str()).map(|m| m == "_last");
                (desc, missing_last)
            }
            _ => (false, None),
        })
        .collect()
}

fn cmp_json(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        (Value::Number(x), Value::Number(y)) => x
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&y.as_f64().unwrap_or(0.0))
            .unwrap_or(Ordering::Equal),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => a.to_string().cmp(&b.to_string()),
    }
}

/// Order two hits from different nodes as the local page cut would.
fn cmp_hits(
    a: &(usize, Value),
    b: &(usize, Value),
    keys: &[(bool, Option<bool>)],
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (na, ha) = a;
    let (nb, hb) = b;
    if keys.is_empty() {
        let sa = ha.get("_score").and_then(|s| s.as_f64()).unwrap_or(0.0);
        let sb = hb.get("_score").and_then(|s| s.as_f64()).unwrap_or(0.0);
        let o = sb.partial_cmp(&sa).unwrap_or(Ordering::Equal);
        if o != Ordering::Equal {
            return o;
        }
    } else {
        let va = ha.get("sort").and_then(|v| v.as_array());
        let vb = hb.get("sort").and_then(|v| v.as_array());
        for (i, (desc, missing_last)) in keys.iter().enumerate() {
            let x = va.and_then(|v| v.get(i)).unwrap_or(&Value::Null);
            let y = vb.and_then(|v| v.get(i)).unwrap_or(&Value::Null);
            let o = match (x.is_null(), y.is_null()) {
                (true, true) => Ordering::Equal,
                (true, false) => {
                    if missing_last.unwrap_or(true) {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (false, true) => {
                    if missing_last.unwrap_or(true) {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (false, false) => {
                    let o = cmp_json(x, y);
                    if *desc { o.reverse() } else { o }
                }
            };
            if o != Ordering::Equal {
                return o;
            }
        }
    }
    // ties: the order the request named the nodes, then the write order
    na.cmp(nb).then_with(|| {
        let sa = ha.get("_seq").and_then(|s| s.as_u64()).unwrap_or(0);
        let sb = hb.get("_seq").and_then(|s| s.as_u64()).unwrap_or(0);
        sa.cmp(&sb)
    })
}

/// Names of the aggregations this engine computes as searches of its own,
/// which a spanning request cannot carry.
fn own_aggregations(body: &Value) -> Vec<String> {
    const OWN: &[&str] = &[
        "filters",
        "filter",
        "missing",
        "median_absolute_deviation",
        "weighted_avg",
        "variable_width_histogram",
        "auto_date_histogram",
        "date_range",
        "ip_range",
        "adjacency_matrix",
        "geohash_grid",
        "geotile_grid",
        "geo_bounds",
        "geo_centroid",
        "matrix_stats",
        "scripted_metric",
        "global",
        "sampler",
        "diversified_sampler",
        "significant_terms",
        "significant_text",
        "rare_terms",
        "multi_terms",
        "children",
        "parent",
        "nested",
        "reverse_nested",
        "top_hits",
        "geo_distance",
        "range_field_histogram",
        // a composite walks the buckets in order and hands back an after key:
        // merging two nodes' pages would take the order with it
        "composite",
    ];
    fn walk(v: &Value, out: &mut Vec<String>) {
        let Some(o) = v.as_object() else { return };
        for (name, def) in o {
            if let Some(d) = def.as_object() {
                for k in d.keys() {
                    if OWN.contains(&k.as_str()) && !out.contains(name) {
                        out.push(name.clone());
                    }
                    if k == "aggs" || k == "aggregations" {
                        walk(&d[k], out);
                    }
                }
                // a script on any aggregation, or a terms order by a nested path
                if d.values().any(|x| x.get("script").is_some()) && !out.contains(name) {
                    out.push(name.clone());
                }
                // a `missing` value stands in for documents the field is not
                // in, and the engine works it out a bucket at a time from the
                // documents themselves rather than from an intermediate
                if d.values().any(|x| x.get("missing").is_some()) && !out.contains(name) {
                    out.push(name.clone());
                }
            }
        }
    }
    let mut out = Vec::new();
    if let Some(a) = body.get("aggs").or_else(|| body.get("aggregations")) {
        walk(a, &mut out);
    }
    if body.get("collapse").is_some()
        || body.get("rescore").is_some()
        || body.get("slice").is_some()
    {
        out.push("collapse/rescore/slice".into());
    }
    out
}

/// One node's share, asked of it and awaited.
async fn ask(
    rt: Arc<super::runtime::Runtime>,
    node: NodeId,
    expr: String,
    body: Value,
    p: Params,
    caller: crate::security::Caller,
) -> (NodeId, Reply) {
    // A walk the server runs for itself -- a geo aggregation, a by-query --
    // is not paging for anybody, and is exempt from the result window. The
    // marker is a thread-local, so it has to be carried across the node
    // boundary or the remote leg refuses the walk halfway through. It rides
    // on the transport rather than in the body, where a caller could write
    // it: the transport is the trust boundary.
    let ask = json!({
        "expr": expr,
        "body": body,
        "params": p,
        "caller": caller,
        "as_the_server": crate::search::is_the_server(),
    });
    let answer = rt
        .call(
            &node,
            SEARCH,
            serde_json::to_vec(&ask).unwrap_or_default(),
            std::time::Duration::from_secs(120),
        )
        .await;
    let reply = match answer {
        None => Reply::Failed("no answer from the node".into()),
        Some(e) if e.kind == Kind::Error => {
            Reply::Failed(String::from_utf8_lossy(&e.body).into_owned())
        }
        Some(e) => match serde_json::from_slice::<Outcome>(&e.body) {
            Ok(o) => Reply::Ok(o),
            Err(err) => Reply::Failed(format!("the node's answer could not be read: {err}")),
        },
    };
    (node, reply)
}

/// A search whose indices span nodes.
pub fn run_spanning(
    store: &Store,
    expr: &str,
    body: &Value,
    p: &Params,
    plan: Plan,
) -> std::result::Result<Outcome, axum::response::Response> {
    use axum::http::StatusCode;
    let Some(rt) = super::runtime() else {
        return crate::search::run(store, expr, body, p);
    };
    // a closed index cannot be searched, wherever its copies are: the answer
    // says so rather than reporting every copy as a failed shard
    let lenient = p.get("ignore_unavailable").map(|v| v == "true").unwrap_or(false);
    if !lenient {
        let closed: Option<String> = super::with_state(|s| {
            plan.all
                .iter()
                // the state published for this index, not for an earlier one
                // of the same name: a restore over a closed index made a new
                // one, and it read as closed until the next publish
                .find(|n| {
                    s.indices
                        .get(*n)
                        .map(|m| {
                            m.state == "close"
                                && store
                                    .get(n)
                                    .map(|st| m.uuid.is_empty() || st.read().uuid == m.uuid)
                                    .unwrap_or(true)
                        })
                        .unwrap_or(false)
                })
                .cloned()
        });
        if let Some(name) = closed {
            return Err(crate::api::err(
                StatusCode::BAD_REQUEST,
                "index_closed_exception",
                format!("closed index [{name}]"),
            ));
        }
    }
    let own = own_aggregations(body);
    // a terms lookup reads a document of another index; when one node holds
    // both, the search runs there and the lookup is a local read
    let lookups = lookup_indices(body);
    // under a point in time the node holding every part is the one that can
    // run the whole request, whatever copies the routing lists now
    let holder = if body.get("pit").is_some() {
        match (plan.local.is_empty(), plan.remote.len()) {
            (true, 1) => plan.remote.keys().next().cloned(),
            (false, 0) => Some(rt.local()),
            _ => None,
        }
    } else {
        super::with_state(|s| {
            s.nodes
                .keys()
                .find(|n| plan.all.iter().chain(lookups.iter()).all(|i| active_here(s, n, i)))
                .cloned()
        })
    };
    if !own.is_empty() || (!lookups.is_empty() && holder.is_some()) {
        return match holder {
            Some(n) => {
                let me = rt.local();
                if n == me {
                    let mut p2 = p.clone();
                    p2.insert("_local_only".into(), "1".into());
                    crate::search::run(store, expr, body, &p2)
                } else {
                    let caller = crate::security::layer::current_caller().unwrap_or_default();
                    let mut p2 = p.clone();
                    p2.insert("_local_only".into(), "1".into());
                    let (_, reply) = tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(ask(
                            rt.clone(),
                            n,
                            expr.to_string(),
                            body.clone(),
                            p2,
                            caller,
                        ))
                    });
                    match reply {
                        Reply::Ok(o) => Ok(o),
                        Reply::Failed(why) => Err(refusal(&why).unwrap_or_else(|| {
                            crate::api::err(
                                StatusCode::SERVICE_UNAVAILABLE,
                                "search_phase_execution_exception",
                                why,
                            )
                        })),
                    }
                }
            }
            // no node holds them all: each node answers for the indices it
            // holds and the buckets are added together, which is what a
            // reduce over final results is. Only for a request that asks for
            // no documents -- with hits to merge as well, the order of the
            // page would have to come from the intermediates.
            None if want_no_hits(body, p) => reduce_over_holders(store, body, p, &plan, rt),
            None => Err(crate::api::err(
                StatusCode::BAD_REQUEST,
                "search_phase_execution_exception",
                format!(
                    "aggregation [{}] runs as a search of its own and needs one node holding every index named, and none does: [{}]",
                    own.join(", "),
                    plan.all.join(", ")
                ),
            )),
        };
    }
    let from = body
        .get("from")
        .and_then(|v| v.as_u64())
        .or_else(|| p.get("from").and_then(|v| v.parse().ok()))
        .unwrap_or(0) as usize;
    let size = body
        .get("size")
        .and_then(|v| v.as_u64())
        .or_else(|| p.get("size").and_then(|v| v.parse().ok()))
        .unwrap_or(10) as usize;
    let mut ask_body = body.clone();
    ask_body["from"] = json!(0);
    ask_body["size"] = json!(from + size);
    let mut ask_p = p.clone();
    ask_p.remove("from");
    ask_p.remove("size");
    ask_p.insert("_native_only".into(), "1".into());
    let caller = crate::security::layer::current_caller().unwrap_or_default();
    let started = std::time::Instant::now();
    // the remote shares, all at once
    let mut waits = Vec::new();
    for (node, indices) in &plan.remote {
        waits.push(tokio::spawn(ask(
            rt.clone(),
            node.clone(),
            indices.join(","),
            ask_body.clone(),
            ask_p.clone(),
            caller.clone(),
        )));
    }
    // this node's share, meanwhile
    let local: Option<(NodeId, Reply)> = if plan.local.is_empty() {
        None
    } else {
        let expr_l = plan.local.join(",");
        Some((rt.local(), {
            let o = crate::search::run(store, &expr_l, &ask_body, &ask_p)?;
            Reply::Ok(o)
        }))
    };
    let mut replies: Vec<(NodeId, Reply)> = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let mut out = Vec::new();
            for w in waits {
                if let Ok(r) = w.await {
                    out.push(r);
                }
            }
            out
        })
    });
    if let Some(l) = local {
        replies.insert(0, l);
    }
    // merge
    let keys = sort_keys_of(body);
    let mut hits: Vec<(usize, Value)> = Vec::new();
    let mut total = 0u64;
    let mut max_score: Option<f32> = None;
    let mut shards = 0u64;
    let mut skipped = 0u64;
    let mut failures: Vec<Value> = Vec::new();
    let mut fruits: Vec<
        velocore::aggregation::intermediate_agg_result::IntermediateAggregationResults,
    > = Vec::new();
    let mut native: Option<crate::search::NativeParts> = None;
    let mut suggest: Option<Value> = None;
    let mut profiles: Vec<Value> = Vec::new();
    let mut took = 0u64;
    let mut filtered = false;
    let mut timed_out = false;
    let mut failed_shards = 0u64;
    let allow_partial = p.get("allow_partial_search_results").map(|v| v != "false").unwrap_or(true);
    for (order, (node, reply)) in replies.into_iter().enumerate() {
        let indices: Vec<String> = if node == rt.local() {
            plan.local.clone()
        } else {
            plan.remote.get(&node).cloned().unwrap_or_default()
        };
        match reply {
            Reply::Ok(o) => {
                for h in o.hits {
                    hits.push((order, h));
                }
                total += o.total;
                if let Some(m) = o.max_score {
                    max_score = Some(max_score.map_or(m, |x: f32| x.max(m)));
                }
                shards += o.shards;
                skipped += o.skipped;
                failures.extend(o.failures);
                took = took.max(o.took_ms);
                filtered |= o.filtered;
                timed_out |= o.timed_out;
                if let Some(pr) = o
                    .profile
                    .and_then(|v| v.get("shards").cloned())
                    .and_then(|v| v.as_array().cloned())
                {
                    profiles.extend(pr);
                }
                if suggest.is_none() {
                    suggest = o.suggest;
                }
                if let Some(n) = o.native {
                    if let Some(bytes) = &n.agg_acc
                        && let Ok(acc) = postcard::from_bytes(bytes)
                    {
                        fruits.push(acc);
                    }
                    if native.is_none()
                        || native.as_ref().map(|x| x.agg_req.is_none()).unwrap_or(false)
                    {
                        native = Some(n);
                    }
                }
            }
            Reply::Failed(why) => {
                // the node refused rather than failed: it is answering for
                // the whole request, so its answer is the request's
                if let Some(r) = refusal(&why) {
                    return Err(r);
                }
                // every shard of every index that node answered for
                let per_index: Vec<(String, u32)> = super::with_state(|s| {
                    indices
                        .iter()
                        .map(|i| {
                            (i.clone(), s.indices.get(i).map(|m| m.number_of_shards).unwrap_or(1))
                        })
                        .collect()
                });
                for (index, n) in per_index {
                    for shard in 0..n {
                        failed_shards += 1;
                        failures.push(json!({
                            "shard": shard, "index": index, "node": node.as_str(),
                            "reason": {"type": "node_not_connected_exception", "reason": why},
                        }));
                    }
                }
            }
        }
    }
    // an index nobody holds an active copy of: every shard of it failed
    for index in &plan.unavailable {
        let n =
            super::with_state(|s| s.indices.get(index).map(|m| m.number_of_shards).unwrap_or(1));
        for shard in 0..n {
            failed_shards += 1;
            failures.push(json!({
                "shard": shard, "index": index, "node": null,
                "reason": {"type": "no_shard_available_action_exception", "reason": null},
            }));
        }
    }
    if failed_shards > 0 && !allow_partial {
        return Err(crate::api::err(
            StatusCode::SERVICE_UNAVAILABLE,
            "search_phase_execution_exception",
            format!("Partial shards failure ({failed_shards} shards unavailable)"),
        ));
    }
    if failed_shards > 0 && hits.is_empty() && total == 0 && shards == 0 {
        return Err(crate::api::err(
            StatusCode::SERVICE_UNAVAILABLE,
            "search_phase_execution_exception",
            "all shards failed",
        ));
    }
    hits.sort_by(|a, b| cmp_hits(a, b, &keys));
    let mut page: Vec<Value> = hits.into_iter().skip(from).take(size).map(|(_, h)| h).collect();
    for h in page.iter_mut() {
        if let Some(o) = h.as_object_mut() {
            o.remove("_seq");
        }
    }
    // the aggregations, intermediate from every node, finished once here
    let agg_acc = fruits.into_iter().reduce(|mut a, b| {
        let _ = a.merge_fruits(b);
        a
    });
    let n = native.unwrap_or_default();
    let agg_req: Option<velocore::aggregation::agg_req::Aggregations> =
        n.agg_req.and_then(|v| serde_json::from_value(v).ok());
    let agg_json = body.get("aggs").or_else(|| body.get("aggregations")).cloned();
    let partitions: Vec<(String, i64, i64, usize)> =
        agg_json.clone().map(|mut a| crate::search::extract_partitions(&mut a)).unwrap_or_default();
    let views = crate::security::view::views_for(store, &plan.all);
    let query_json = body.get("query").cloned();
    let out = crate::search::finish_search(
        store,
        &plan.all,
        body,
        p,
        crate::search::Finish {
            started,
            // the coordinator holds no budget of its own: what it merges was
            // already collected under each node's
            budget: crate::search::Budget::unbounded(),
            timed_out,
            page,
            total,
            max_score,
            shards: shards + failed_shards,
            empty_shards: n.empty_shards,
            failures,
            suggest,
            agg_acc,
            agg_req,
            agg_json,
            bucket_orders: n.bucket_orders,
            partitions,
            agg_meta: n.agg_meta,
            weighted: n.weighted,
            filters_aggs: Vec::new(),
            bucket_pipelines: Vec::new(),
            pipeline_aggs: Vec::new(),
            shard_profiles: profiles,
            query_json,
            views,
            dls_applied: filtered,
            extras: Default::default(),
            named: Default::default(),
            size,
            join_inner_hits: Vec::new(),
        },
    )?;
    let mut out = out;
    out.took_ms = out.took_ms.max(took);
    out.skipped = skipped;
    // an aggregation the request named and the merge could not produce -- one
    // the engine works out from the documents rather than from an
    // intermediate: every holder is asked for its own answer instead, and the
    // buckets are added together
    let asked_for: Vec<String> = body
        .get("aggs")
        .or_else(|| body.get("aggregations"))
        .and_then(|a| a.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    let missing =
        asked_for.iter().any(|name| out.aggs.as_ref().and_then(|a| a.get(name)).is_none());
    if missing {
        return reduce_over_holders(store, body, p, &plan, rt);
    }
    Ok(out)
}

/// The node's side: run its share and hand back the outcome, as the caller.
pub fn install(store: Store) {
    let Some(rt) = super::runtime() else { return };
    let me = rt.local();
    let s = store.clone();
    let from = me.clone();
    rt.register(
        SEARCH,
        Arc::new(move |e: Envelope| -> DataFuture {
            let store = s.clone();
            let from = from.clone();
            Box::pin(async move {
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                let expr = v.get("expr").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let body = v.get("body").cloned().unwrap_or(json!({}));
                let p: Params = v
                    .get("params")
                    .and_then(|x| serde_json::from_value(x.clone()).ok())
                    .unwrap_or_default();
                let caller: crate::security::Caller = v
                    .get("caller")
                    .and_then(|c| serde_json::from_value(c.clone()).ok())
                    .unwrap_or_default();
                // the coordinator says whether this is a walk it is running
                // for itself; a client cannot say so, because a client does
                // not speak this action
                let for_the_server =
                    v.get("as_the_server").and_then(|x| x.as_bool()).unwrap_or(false);
                let result = tokio::task::spawn_blocking(move || {
                    crate::security::layer::CALLER.sync_scope(caller, || match for_the_server {
                        true => crate::search::as_the_server(|| {
                            crate::search::run(&store, &expr, &body, &p)
                        }),
                        false => crate::search::run(&store, &expr, &body, &p),
                    })
                })
                .await;
                match result {
                    Ok(Ok(out)) => e.response(from, serde_json::to_vec(&out).unwrap_or_default()),
                    Ok(Err(resp)) => {
                        // the answer the node would have given the client,
                        // status and body, so the coordinator can hand the
                        // same one back rather than turning every refusal
                        // into "the shards would not answer"
                        let status = resp.status().as_u16();
                        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                            .await
                            .map(|b| String::from_utf8_lossy(&b).into_owned())
                            .unwrap_or_default();
                        e.error(from, &json!({"status": status, "body": body}).to_string())
                    }
                    Err(err) => e.error(from, &format!("search panicked: {err}")),
                }
            })
        }),
    );
    let s = store.clone();
    let from = me.clone();
    rt.register(
        PIT,
        Arc::new(move |e: Envelope| -> DataFuture {
            let store = s.clone();
            let from = from.clone();
            Box::pin(async move {
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                // a caller is carried only where the asking node knew one: a
                // context opened with nobody named belongs to nobody, and
                // standing a nameless caller in would make it somebody's
                let caller: Option<crate::security::Caller> =
                    v.get("caller").and_then(|c| serde_json::from_value(c.clone()).ok());
                let out = match caller {
                    Some(c) => {
                        crate::security::layer::CALLER.sync_scope(c, || pit_here(&store, &v))
                    }
                    None => pit_here(&store, &v),
                };
                e.response(from, serde_json::to_vec(&out).unwrap_or_default())
            })
        }),
    );
}

/// This node's side of a point in time: open a part, let go of one or all of
/// them, or list them, as the caller that asked.
fn pit_here(store: &Store, v: &Value) -> Value {
    match v.get("op").and_then(|o| o.as_str()).unwrap_or("") {
        "open" => {
            let id = v.get("id").and_then(|i| i.as_str()).and_then(crate::store::PitId::decode);
            let indices: Vec<String> = v
                .get("indices")
                .and_then(|i| serde_json::from_value(i.clone()).ok())
                .unwrap_or_default();
            let keep = v.get("keep_alive_ms").and_then(|x| x.as_u64()).unwrap_or(0);
            let created = v.get("created_ms").and_then(|x| x.as_u64()).unwrap_or(0);
            let for_scroll = v.get("for_scroll").and_then(|x| x.as_bool()).unwrap_or(false);
            match id {
                Some(id) => match store.open_pit_part(&id, &indices, keep, created, for_scroll) {
                    Ok(()) => json!({"opened": true}),
                    Err(missing) => json!({"opened": false, "missing": missing}),
                },
                None => json!({"opened": false}),
            }
        }
        "close" => {
            let token = v.get("token").and_then(|t| t.as_str()).unwrap_or("");
            json!({"closed": store.close_pit(token)})
        }
        "close_all" => json!({"closed": store.close_visible_pits()}),
        _ => json!({"pits": store.visible_pits().iter().map(pit_listing).collect::<Vec<_>>()}),
    }
}

/// A point in time as `_search/point_in_time/_all` lists it.
pub fn pit_listing(p: &crate::store::PitState) -> Value {
    json!({"pit_id": p.id, "creation_time": p.created_ms, "keep_alive": p.keep_alive_ms})
}

/// A point in time, opened: its id, and how many shards it covers.
pub struct OpenedPit {
    pub id: String,
    pub shards: u64,
    pub created_ms: u64,
}

/// The name this node goes by in a point in time's id.
fn own_part_name() -> String {
    super::runtime().map(|rt| rt.local().as_str().to_string()).unwrap_or_default()
}

/// Ask every node named of one point-in-time operation, this one included,
/// as the caller; each node's answer.
fn ask_pit_nodes(nodes: &[NodeId], op: Value) -> Vec<(NodeId, Option<Value>)> {
    let Some(rt) = super::runtime() else { return Vec::new() };
    let mut ask = op;
    ask["caller"] = json!(crate::security::layer::current_caller());
    let bytes = serde_json::to_vec(&ask).unwrap_or_default();
    let run = async {
        let mut waits = Vec::new();
        for node in nodes {
            let rt = rt.clone();
            let node = node.clone();
            let bytes = bytes.clone();
            waits.push(tokio::spawn(async move {
                let answer = rt.call(&node, PIT, bytes, std::time::Duration::from_secs(30)).await;
                let value = answer
                    .filter(|e| e.kind != Kind::Error)
                    .and_then(|e| serde_json::from_slice::<Value>(&e.body).ok());
                (node, value)
            }));
        }
        let mut out = Vec::new();
        for w in waits {
            if let Ok(r) = w.await {
                out.push(r);
            }
        }
        out
    };
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(run))
}

/// Every node of the cluster, where there is one to ask.
fn every_node() -> Vec<NodeId> {
    match super::runtime() {
        Some(_) => super::with_state(|s| s.nodes.keys().cloned().collect()),
        None => Vec::new(),
    }
}

/// Open a point in time over what an expression names: a part on every node
/// that holds one of the indices, each part reading its indices as they stand
/// now. The id says which node holds which, so it can be searched from any.
pub fn open_pit(
    store: &Store,
    expr: &str,
    keep_alive_ms: u64,
    for_scroll: bool,
) -> std::result::Result<OpenedPit, axum::response::Response> {
    use axum::http::StatusCode;
    let created_ms = crate::store::now_millis() as u64;
    let token = crate::cluster::NodeId::random().as_str().to_string();
    let spanning = plan(store, expr, None).filter(|p| p.spans_nodes());
    let Some(plan) = spanning else {
        let names = store.resolve(expr);
        if names.is_empty() && !expr.is_empty() {
            return Err(crate::api::shared::no_such_index(expr));
        }
        let mut parts = BTreeMap::new();
        parts.insert(own_part_name(), names.clone());
        let id = crate::store::PitId { token, parts };
        store
            .open_pit_part(&id, &names, keep_alive_ms, created_ms, for_scroll)
            .map_err(|missing| crate::api::shared::no_such_index(&missing))?;
        let shards = crate::api::shards_over(store, &names)["total"].as_u64().unwrap_or(0);
        return Ok(OpenedPit { id: id.encode(), shards, created_ms });
    };
    if let Some(index) = plan.unavailable.first() {
        return Err(crate::api::err(
            StatusCode::SERVICE_UNAVAILABLE,
            "search_phase_execution_exception",
            format!("no active copy of [{index}] to open a point in time over"),
        ));
    }
    let mut parts: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if !plan.local.is_empty() {
        parts.insert(own_part_name(), plan.local.clone());
    }
    for (node, indices) in &plan.remote {
        parts.insert(node.as_str().to_string(), indices.clone());
    }
    let id = crate::store::PitId { token: token.clone(), parts };
    let encoded = id.encode();
    let mut failed: Option<String> = None;
    if !plan.local.is_empty()
        && let Err(missing) =
            store.open_pit_part(&id, &plan.local, keep_alive_ms, created_ms, for_scroll)
    {
        failed = Some(format!("no such index [{missing}]"));
    }
    let mut opened_on: Vec<NodeId> = Vec::new();
    if failed.is_none() {
        for (node, indices) in &plan.remote {
            let answer = ask_pit_nodes(
                std::slice::from_ref(node),
                json!({"op": "open", "id": encoded, "indices": indices,
                       "keep_alive_ms": keep_alive_ms, "created_ms": created_ms,
                       "for_scroll": for_scroll}),
            );
            let opened = answer
                .first()
                .and_then(|(_, v)| v.as_ref())
                .and_then(|v| v.get("opened"))
                .and_then(|o| o.as_bool())
                .unwrap_or(false);
            if opened {
                opened_on.push(node.clone());
            } else {
                failed = Some(format!("[{}] could not open its part", node.as_str()));
                break;
            }
        }
    }
    if let Some(why) = failed {
        // a point in time missing a part would answer without those indices,
        // so what was opened is let go of again
        store.close_pit(&token);
        ask_pit_nodes(&opened_on, json!({"op": "close", "token": token}));
        return Err(crate::api::err(
            StatusCode::SERVICE_UNAVAILABLE,
            "search_phase_execution_exception",
            why,
        ));
    }
    let shards = super::with_state(|s| {
        plan.all
            .iter()
            .map(|i| s.indices.get(i).map(|m| m.number_of_shards as u64).unwrap_or(1))
            .sum()
    });
    Ok(OpenedPit { id: encoded, shards, created_ms })
}

/// The nodes a point in time asks, where any of them is not this one.
pub fn pit_plan(id: &crate::store::PitId) -> Option<Plan> {
    let rt = super::runtime()?;
    let me = rt.local();
    if id.parts.keys().all(|n| n.is_empty() || n == me.as_str()) {
        return None;
    }
    let mut plan = Plan { all: id.indices(), ..Plan::default() };
    for (node, indices) in &id.parts {
        if node.is_empty() || node == me.as_str() {
            plan.local.extend(indices.iter().cloned());
        } else {
            plan.remote.entry(NodeId(node.clone())).or_default().extend(indices.iter().cloned());
        }
    }
    Some(plan)
}

/// Let go of a point in time on every node holding a part of it; whether any
/// part was let go of.
pub fn close_pit(store: &Store, id: &crate::store::PitId) -> bool {
    let mut closed = false;
    let mut others = Vec::new();
    let me = own_part_name();
    for node in id.parts.keys() {
        if node.is_empty() || *node == me {
            closed |= store.close_pit(&id.token);
        } else {
            others.push(NodeId(node.clone()));
        }
    }
    for (_, answer) in ask_pit_nodes(&others, json!({"op": "close", "token": id.token})) {
        closed |= answer.and_then(|v| v.get("closed").and_then(|c| c.as_bool())).unwrap_or(false);
    }
    closed
}

/// Every point in time the caller may see, from every node, each once.
pub fn list_pits(store: &Store) -> Vec<Value> {
    let mut all: Vec<Value> = store.visible_pits().iter().map(pit_listing).collect();
    let me = own_part_name();
    let others: Vec<NodeId> = every_node().into_iter().filter(|n| n.as_str() != me).collect();
    for (_, answer) in ask_pit_nodes(&others, json!({"op": "list"})) {
        if let Some(pits) = answer.and_then(|v| v.get("pits").and_then(|p| p.as_array()).cloned()) {
            all.extend(pits);
        }
    }
    let mut seen = std::collections::HashSet::new();
    all.retain(|p| seen.insert(p.get("pit_id").and_then(|i| i.as_str()).unwrap_or("").to_string()));
    all
}

/// Let go of every point in time the caller may, on every node; the ids.
pub fn close_all_pits(store: &Store) -> Vec<String> {
    let mut ids = store.close_visible_pits();
    let me = own_part_name();
    let others: Vec<NodeId> = every_node().into_iter().filter(|n| n.as_str() != me).collect();
    for (_, answer) in ask_pit_nodes(&others, json!({"op": "close_all"})) {
        if let Some(closed) = answer.and_then(|v| v.get("closed").cloned())
            && let Ok(list) = serde_json::from_value::<Vec<String>>(closed)
        {
            ids.extend(list);
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

/// The indices a terms lookup in this body reads its terms from.
fn lookup_indices(body: &Value) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(o) => {
                if let Some(Value::Object(terms)) = o.get("terms") {
                    for def in terms.values() {
                        if let Some(i) = def.get("index").and_then(|i| i.as_str())
                            && !out.iter().any(|x| x == i)
                        {
                            out.push(i.to_string());
                        }
                    }
                }
                for inner in o.values() {
                    walk(inner, out);
                }
            }
            Value::Array(a) => {
                for inner in a {
                    walk(inner, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(body, &mut out);
    out
}

/// Does this request ask for no documents, only aggregations?
fn want_no_hits(body: &Value, p: &Params) -> bool {
    let size = body
        .get("size")
        .and_then(|v| v.as_u64())
        .or_else(|| p.get("size").and_then(|v| v.parse().ok()));
    size == Some(0)
}

/// Ask every node holding part of what was named, and add the answers up.
///
/// Each node runs the whole request over its own indices -- aggregations and
/// all -- and the buckets are merged by key, the way a reduce phase merges
/// what the shards send back.
fn reduce_over_holders(
    store: &Store,
    body: &Value,
    p: &Params,
    plan: &Plan,
    rt: Arc<super::runtime::Runtime>,
) -> std::result::Result<Outcome, axum::response::Response> {
    use axum::http::StatusCode;
    let me = rt.local();
    let caller = crate::security::layer::current_caller().unwrap_or_default();
    let mut parts: Vec<Outcome> = Vec::new();
    let mut asks: Vec<(NodeId, Vec<String>)> = Vec::new();
    if !plan.local.is_empty() {
        asks.push((me.clone(), plan.local.clone()));
    }
    for (node, names) in &plan.remote {
        asks.push((node.clone(), names.clone()));
    }
    for (node, names) in asks {
        let expr = names.join(",");
        let mut p2 = p.clone();
        p2.insert("_local_only".into(), "1".into());
        if node == me {
            {
                let o = crate::search::run(store, &expr, body, &p2)?;
                parts.push(o)
            }
        } else {
            let (_, reply) = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(ask(
                    rt.clone(),
                    node,
                    expr,
                    body.clone(),
                    p2,
                    caller.clone(),
                ))
            });
            match reply {
                Reply::Ok(o) => parts.push(o),
                Reply::Failed(why) => {
                    return Err(refusal(&why).unwrap_or_else(|| {
                        crate::api::err(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "search_phase_execution_exception",
                            why,
                        )
                    }));
                }
            }
        }
    }
    let mut out = parts.pop().ok_or_else(|| {
        crate::api::err(
            StatusCode::SERVICE_UNAVAILABLE,
            "search_phase_execution_exception",
            "no node answered",
        )
    })?;
    let keys = sort_keys_of(body);
    let size = body.get("size").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
    for other in parts {
        out.total += other.total;
        out.shards += other.shards;
        out.skipped += other.skipped;
        out.failures.extend(other.failures);
        out.took_ms = out.took_ms.max(other.took_ms);
        out.hits.extend(other.hits);
        out.max_score = match (out.max_score, other.max_score) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        match (out.aggs.as_mut(), other.aggs) {
            (Some(a), Some(b)) => merge_agg_json(a, &b),
            (None, b) => out.aggs = b,
            _ => {}
        }
    }
    // the documents of every node, in the order the request asked for
    let mut hits: Vec<(usize, Value)> = out.hits.drain(..).map(|h| (0, h)).collect();
    hits.sort_by(|a, b| cmp_hits(a, b, &keys));
    out.hits = hits.into_iter().take(size).map(|(_, h)| h).collect();
    Ok(out)
}

/// Add one node's aggregation answer into another: buckets are matched by
/// their key and their counts added, and anything else keeps what the first
/// node said.
fn merge_agg_json(a: &mut Value, b: &Value) {
    match (a, b) {
        (Value::Object(ao), Value::Object(bo)) => {
            for (k, bv) in bo {
                match ao.get_mut(k) {
                    Some(av) => {
                        if k == "doc_count"
                            || k == "sum_other_doc_count"
                            || k == "doc_count_error_upper_bound"
                        {
                            let sum = av.as_f64().unwrap_or(0.0) + bv.as_f64().unwrap_or(0.0);
                            *av = json!(sum as i64);
                        } else {
                            merge_agg_json(av, bv);
                        }
                    }
                    None => {
                        ao.insert(k.clone(), bv.clone());
                    }
                }
            }
        }
        (Value::Array(aa), Value::Array(ba)) => {
            // buckets: by key, if they carry one
            for bv in ba {
                let key = bv.get("key");
                match key.and_then(|k| {
                    aa.iter_mut().find(|x| x.get("key").map(|xk| xk == k).unwrap_or(false))
                }) {
                    Some(av) => merge_agg_json(av, bv),
                    None => aa.push(bv.clone()),
                }
            }
            aa.sort_by(|x, y| {
                let (dx, dy) = (
                    x.get("doc_count").and_then(|v| v.as_i64()).unwrap_or(0),
                    y.get("doc_count").and_then(|v| v.as_i64()).unwrap_or(0),
                );
                dy.cmp(&dx)
            });
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_merge_by_sort_then_score_then_order() {
        let keys = sort_keys_of(&json!({"sort": [{"a": "desc"}, "_score"]}));
        assert!(keys[0].0);
        let keys2 = sort_keys_of(&json!({"sort": [{"a": {"order": "desc"}}, "_score"]}));
        assert_eq!(keys, keys2);
        let a = (0usize, json!({"sort": [5, 1.0], "_seq": 1}));
        let b = (1usize, json!({"sort": [7, 1.0], "_seq": 0}));
        let c = (1usize, json!({"sort": [5, 1.0], "_seq": 0}));
        let mut v = [a.clone(), b.clone(), c.clone()];
        v.sort_by(|x, y| cmp_hits(x, y, &keys));
        assert_eq!(v[0].1["sort"][0], 7);
        // equal sort values: the node named first, then the write order
        assert_eq!(v[1].0, 0);
        assert_eq!(v[2].0, 1);
        let keys = sort_keys_of(&json!({}));
        let x = (0usize, json!({"_score": 1.5}));
        let y = (0usize, json!({"_score": 2.5}));
        let mut v = [x, y];
        v.sort_by(|a, b| cmp_hits(a, b, &keys));
        assert_eq!(v[0].1["_score"], 2.5);
        // a missing sort value goes last whatever the direction
        let keys = sort_keys_of(&json!({"sort": [{"n": {"order": "desc"}}]}));
        let m = (0usize, json!({"sort": [null]}));
        let n = (0usize, json!({"sort": [1]}));
        let mut v = [m, n];
        v.sort_by(|a, b| cmp_hits(a, b, &keys));
        assert_eq!(v[0].1["sort"][0], 1);
    }

    #[test]
    fn the_engines_own_aggregations_are_recognised() {
        assert!(own_aggregations(&json!({"aggs": {"t": {"terms": {"field": "x"}}}})).is_empty());
        assert_eq!(
            own_aggregations(&json!({"aggs": {"f": {"filters": {"filters": {}}}}})),
            vec!["f"]
        );
        assert_eq!(
            own_aggregations(
                &json!({"aggs": {"t": {"terms": {"field": "x"}, "aggs": {"m": {"missing": {"field": "y"}}}}}})
            ),
            vec!["m"]
        );
        assert_eq!(
            own_aggregations(&json!({"collapse": {"field": "x"}})),
            vec!["collapse/rescore/slice"]
        );
    }
}
