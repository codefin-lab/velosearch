//! `_cat`: the same answers, as columns a person can read.

use super::*;

mod render;
pub(crate) use render::*;
mod tables;
pub use tables::*;

/// `_cat/{what}` in one place. The shapes people actually read are filled in;
/// the rest answer with the right envelope rather than a 501.
pub async fn cat_dispatch_target(
    State(store): State<Store>,
    Path((what, target)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    cat_by_name(store, what, Some(target), p).await
}

pub async fn cat_dispatch(
    State(store): State<Store>,
    Path(what): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    cat_by_name(store, what, None, p).await
}

pub(crate) async fn cat_by_name(
    store: Store,
    what: String,
    target: Option<String>,
    p: Params,
) -> Response {
    let what = what.split('/').next().unwrap_or("").to_string();
    match what.as_str() {
        "indices" => cat_indices(State(store), None, Query(p)).await,
        "aliases" => cat_aliases(State(store), None, Query(p)).await,
        "count" => cat_count(State(store), None, Query(p)).await,
        "health" => cat_health(State(store), Query(p)).await,
        "master" | "cluster_manager" => {
            // the node the cluster has elected, which is not this node unless
            // it happens to be: naming the node the request reached had every
            // node of a cluster call itself the manager
            let live = crate::cluster::current_state();
            let elected = live
                .cluster_manager
                .as_ref()
                .and_then(|id| live.nodes.get(id))
                .map(|n| (n.id.as_str().to_string(), n.name.clone()))
                .unwrap_or_else(|| {
                    let me = crate::cluster::identity();
                    (me.id.as_str().to_string(), me.name.clone())
                });
            cat_render(
                vec![vec![
                    ("id", elected.0),
                    ("host", "127.0.0.1".into()),
                    ("ip", "127.0.0.1".into()),
                    ("node", elected.1),
                ]],
                &p,
            )
        }
        "nodes" => {
            // one row per node the cluster state holds, the manager starred
            let live = crate::cluster::current_state();
            let full = p.get("full_id").map(|v| v != "false").unwrap_or(false);
            // this node's figures are measured; another node's are its own to
            // report, and are left blank here rather than written as zeros
            let here = crate::api::sysinfo::resources(store.data_dir());
            let me = crate::cluster::identity().id.clone();
            let unit = p.get("bytes").map(|s| s.to_string());
            let size = |b: u64| crate::api::shared::sized(unit.as_deref(), b);
            let pct = crate::api::sysinfo::percent;
            let mut rows_all: Vec<Vec<(&str, String)>> = Vec::new();
            for (id, n) in &live.nodes {
                let letters: String = {
                    let mut l = String::new();
                    for r in &n.roles {
                        l.push(match r.as_str() {
                            "cluster_manager" | "master" => 'm',
                            "data" => 'd',
                            "ingest" => 'i',
                            "remote_cluster_client" => 'r',
                            "search" => 's',
                            "warm" => 'w',
                            _ => continue,
                        });
                    }
                    let mut v: Vec<char> = l.chars().collect();
                    v.sort();
                    v.into_iter().collect()
                };
                let ip = n
                    .transport_address
                    .rsplit_once(':')
                    .map(|(h, _)| h.to_string())
                    .unwrap_or_default();
                let is_manager = live.cluster_manager.as_ref() == Some(id);
                let local = *id == me;
                let measured = |v: String| if local { v } else { String::new() };
                let load = |i: usize| {
                    measured(here.load.map(|l| format!("{:.2}", l[i])).unwrap_or_default())
                };
                let (disk_total, disk_avail) =
                    here.disk.as_ref().map(|d| (d.total, d.available)).unwrap_or((0, 0));
                let disk_used = disk_total.saturating_sub(disk_avail);
                rows_all.push(vec![
                    (
                        "id",
                        if full {
                            id.as_str().to_string()
                        } else {
                            id.as_str().chars().take(4).collect()
                        },
                    ),
                    ("ip", ip.clone()),
                    // what the node is and what runs it, which a caller
                    // naming its own columns asks for
                    ("pid", std::process::id().to_string()),
                    ("version", crate::OPENSEARCH_VERSION.into()),
                    ("type", "tar".into()),
                    ("build", "velosearch".into()),
                    ("jdk", "21".into()),
                    (
                        "uptime",
                        measured(crate::api::shared::time_value_text(
                            crate::api::sysinfo::uptime_millis() * 1_000_000,
                        )),
                    ),
                    ("master", if is_manager { "*".into() } else { "-".into() }),
                    ("file_desc.current", measured(here.fds.to_string())),
                    ("file_desc.percent", measured(pct(here.fds, here.max_fds).to_string())),
                    ("file_desc.max", measured(here.max_fds.to_string())),
                    ("heap.current", measured(size(here.heap_used))),
                    ("heap.percent", measured(pct(here.heap_used, here.heap_max).to_string())),
                    ("heap.max", measured(size(here.heap_max))),
                    ("ram.current", measured(size(here.ram_used))),
                    ("ram.percent", measured(pct(here.ram_used, here.ram_total).to_string())),
                    ("ram.max", measured(size(here.ram_total))),
                    // the address the node answers HTTP on, which it says
                    // when it joins; every node was reported at port 9200
                    (
                        "http",
                        if n.http_address.is_empty() {
                            format!("{ip}:9200")
                        } else {
                            n.http_address.clone()
                        },
                    ),
                    ("cpu", measured(here.cpu_percent.to_string())),
                    ("load_1m", load(0)),
                    ("load_5m", load(1)),
                    ("load_15m", load(2)),
                    ("node.role", letters),
                    ("node.roles", n.roles.join(",")),
                    ("cluster_manager", if is_manager { "*".into() } else { "-".into() }),
                    ("name", n.name.clone()),
                    ("diskAvail", measured(size(disk_avail))),
                    ("diskTotal", measured(size(disk_total))),
                    ("diskUsed", measured(size(disk_used))),
                    (
                        "diskUsedPercent",
                        measured(if disk_total == 0 {
                            String::new()
                        } else {
                            format!("{:.2}", disk_used as f64 * 100.0 / disk_total as f64)
                        }),
                    ),
                ]);
            }
            let rows = cat_only_default(
                rows_all,
                &[
                    "ip",
                    "heap.percent",
                    "ram.percent",
                    "cpu",
                    "load_1m",
                    "load_5m",
                    "load_15m",
                    "node.role",
                    "node.roles",
                    "cluster_manager",
                    "name",
                ],
                &p,
            );
            cat_render_cols(
                &[
                    "id",
                    "ip",
                    "file_desc.current",
                    "file_desc.percent",
                    "file_desc.max",
                    "heap.current",
                    "heap.percent",
                    "heap.max",
                    "ram.current",
                    "ram.percent",
                    "ram.max",
                    "http",
                    "cpu",
                    "load_1m",
                    "load_5m",
                    "load_15m",
                    "node.role",
                    "node.roles",
                    "cluster_manager",
                    "name",
                    "diskAvail",
                    "diskTotal",
                    "diskUsed",
                    "diskUsedPercent",
                ],
                rows,
                &p,
            )
        }
        "templates" => {
            let mut rows: Vec<Vec<(&str, String)>> = store
                .get_templates()
                .into_iter()
                .map(|(name, t)| {
                    let list = |key: &str| {
                        t.get(key)
                            .and_then(|v| v.as_array())
                            .map(|a| {
                                // a list is written the way a list is read, with
                                // a space after each comma
                                a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")
                            })
                            .unwrap_or_default()
                    };
                    // the composable form keeps the body it was written with,
                    // which is where its version and components are
                    let body = t.get("__composable").unwrap_or(&t);
                    let num = |key: &str| {
                        body.get(key)
                            .or_else(|| t.get(key))
                            .map(|o| o.to_string())
                            .unwrap_or_default()
                    };
                    vec![
                        ("name", name),
                        ("index_patterns", format!("[{}]", list("index_patterns"))),
                        ("order", {
                            let o = num("order");
                            if o.is_empty() { num("priority") } else { o }
                        }),
                        ("version", num("version")),
                        ("composed_of", {
                            let c = body
                                .get("composed_of")
                                .and_then(|v| v.as_array())
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|x| x.as_str())
                                        .collect::<Vec<_>>()
                                        .join(",")
                                })
                                .unwrap_or_default();
                            // a template composed of nothing names nothing
                            if c.is_empty() { String::new() } else { format!("[{c}]") }
                        }),
                    ]
                })
                .collect();
            // the path names which templates to show, by name or by pattern
            if let Some(t) = target.as_deref().filter(|t| !t.is_empty() && *t != "*") {
                rows.retain(|r| {
                    t.split(',').any(|pat| {
                        let pat = pat.trim();
                        pat == r[0].1 || crate::store::glob_match(pat, &r[0].1)
                    })
                });
            }
            rows.sort_by(|a, b| a[0].1.cmp(&b[0].1));
            let rows = cat_only_default(rows, CAT_TEMPLATE_COLS, &p);
            cat_render_cols(CAT_TEMPLATE_COLS, rows, &p)
        }
        "shards" => {
            use crate::cluster::state::ShardState;
            // the path names which indices to describe; the manager's
            // routing says where each copy is
            let live = crate::cluster::current_state();
            let matches = |t: &str, n: &str| {
                t.split(',').any(|part| {
                    let part = part.trim();
                    part == n
                        || part == "_all"
                        || (part.contains('*') && crate::store::glob_match(part, n))
                })
            };
            let mut names: Vec<String> = match target.as_deref().filter(|t| !t.is_empty()) {
                Some(t) => store.resolve(t),
                None => store.names(),
            };
            for n in live.routing.indices.keys() {
                let wanted = target
                    .as_deref()
                    .filter(|t| !t.is_empty())
                    .map(|t| matches(t, n))
                    .unwrap_or(true);
                if wanted && !names.contains(n) {
                    names.push(n.clone());
                }
            }
            let me = crate::cluster::identity();
            let ip_of = |id: &crate::cluster::NodeId| -> String {
                live.nodes
                    .get(id)
                    .map(|n| n.transport_address.split(':').next().unwrap_or("").to_string())
                    .unwrap_or_default()
            };
            let name_of = |id: &crate::cluster::NodeId| -> String {
                live.nodes.get(id).map(|n| n.name.clone()).unwrap_or_default()
            };
            let blank = |n: &str,
                         shard: u32,
                         prirep: &str,
                         state: &str,
                         reason: &str|
             -> Vec<(&'static str, String)> {
                vec![
                    ("index", n.to_string()),
                    ("shard", shard.to_string()),
                    ("prirep", prirep.into()),
                    ("state", state.into()),
                    ("docs", String::new()),
                    ("store", String::new()),
                    ("ip", String::new()),
                    ("id", String::new()),
                    ("node", String::new()),
                    ("unassigned.reason", reason.into()),
                    ("unassigned.at", String::new()),
                    ("unassigned.for", String::new()),
                    ("unassigned.details", String::new()),
                ]
            };
            let clustered = live.nodes.len() > 1;
            let forwarded = crate::cluster::forward::answering_forward();
            let mut rows: Vec<Vec<(&str, String)>> = Vec::new();
            for n in names {
                // each shard holds the documents routed to it, and its share
                // of the index's bytes is its share of the documents
                let per_shard =
                    store.get(&n).map(|st| st.read().docs_per_shard()).unwrap_or_default();
                let local_docs: u64 = per_shard.iter().sum();
                let docs_on = |shard: u32| per_shard.get(shard as usize).copied().unwrap_or(0);
                let bytes_on = |shard: u32| match local_docs {
                    0 if shard == 0 => store.index_size(&n),
                    0 => 0,
                    total => {
                        (store.index_size(&n) as u128 * docs_on(shard) as u128 / total as u128)
                            as u64
                    }
                };
                let mut copies: Vec<&crate::cluster::state::ShardRouting> =
                    live.routing.shards_of(&n).collect();
                copies.sort_by_key(|c| (c.shard, !c.primary));
                if copies.is_empty() {
                    if clustered && forwarded {
                        continue;
                    }
                    // not published: this node holds it, alone
                    let Some(st) = store.get(&n) else { continue };
                    let g = st.read();
                    let shards = g.numeric_setting("number_of_shards").unwrap_or(1).max(1) as u32;
                    let replicas = g.numeric_setting("number_of_replicas").unwrap_or(1);
                    for shard in 0..shards {
                        let mut row = blank(&n, shard, "p", "STARTED", "");
                        row[4].1 = docs_on(shard).to_string();
                        row[5].1 = crate::api::shared::sized(
                            p.get("bytes").map(|s| s.as_str()),
                            bytes_on(shard),
                        );
                        row[6].1 = "127.0.0.1".into();
                        row[7].1 = me.id.as_str().into();
                        row[8].1 = me.name.clone();
                        rows.push(row);
                        for _ in 0..replicas {
                            rows.push(blank(&n, shard, "r", "UNASSIGNED", "INDEX_CREATED"));
                        }
                    }
                    continue;
                }
                for c in copies {
                    let here = c.node.as_ref() == Some(&me.id);
                    let prirep = if c.primary { "p" } else { "r" };
                    let mut row = blank(&n, c.shard, prirep, c.state.as_str(), "");
                    match (&c.node, &c.relocating_node, c.state) {
                        (Some(nd), Some(to), ShardState::Relocating) => {
                            row[6].1 = ip_of(nd);
                            row[7].1 = nd.as_str().to_string();
                            row[8].1 = format!(
                                "{} -> {} {} {}",
                                name_of(nd),
                                ip_of(to),
                                to.as_str(),
                                name_of(to)
                            );
                        }
                        (Some(nd), _, _) => {
                            row[6].1 = ip_of(nd);
                            row[7].1 = nd.as_str().to_string();
                            row[8].1 = name_of(nd);
                        }
                        _ => {}
                    }
                    let active = matches!(c.state, ShardState::Started | ShardState::Relocating);
                    if active {
                        // a copy is a copy of the whole index here, so a node
                        // holding one can answer for it; a copy elsewhere is
                        // that node's row to write
                        if here {
                            row[4].1 = docs_on(c.shard).to_string();
                            row[5].1 = crate::api::shared::sized(
                                p.get("bytes").map(|s| s.as_str()),
                                bytes_on(c.shard),
                            );
                        } else if clustered {
                            continue;
                        } else {
                            row[4].1 = "0".into();
                            row[5].1 = "0b".into();
                        }
                    } else if clustered
                        && (match &c.node {
                            // a copy being made somewhere else is that node's row
                            Some(nd) => nd != &me.id,
                            // a copy with no node is written once, by the node
                            // the request reached
                            None => forwarded,
                        })
                    {
                        continue;
                    }
                    if let (Some(u), ShardState::Unassigned) = (&c.unassigned, c.state) {
                        row[9].1 = u.reason.clone();
                        row[10].1 = crate::cluster::state::iso_millis(u.at_millis);
                        row[12].1 = u.details.clone().unwrap_or_default();
                    }
                    rows.push(row);
                }
            }
            rows.sort_by(|a, b| a[0].1.cmp(&b[0].1));
            let rows = cat_only_default(
                rows,
                &["index", "shard", "prirep", "state", "docs", "store", "ip", "node"],
                &p,
            );
            cat_render(rows, &p)
        }
        // a point-in-time's segments are not listed from OpenSearch 2.10 on:
        // the table is there and has nothing in it
        "pit_segments" => cat_render_cols(CAT_SEGMENT_COLS, Vec::new(), &p),
        "segments" => {
            let rows = store
                .names()
                .into_iter()
                .filter_map(|n| store.get(&n).map(|st| (n, st)))
                .flat_map(|(n, st)| {
                    let searcher = st.read().reader.searcher();
                    searcher
                        .segment_readers()
                        .iter()
                        .enumerate()
                        .map(|(i, sr)| {
                            vec![
                                ("index", n.clone()),
                                ("shard", "0".into()),
                                ("prirep", "p".into()),
                                // which node the segment is on
                                ("id", crate::cluster::identity().id.as_str().to_string()),
                                ("segment", format!("_{i}")),
                                ("docs.count", sr.num_docs().to_string()),
                                ("docs.deleted", sr.num_deleted_docs().to_string()),
                            ]
                        })
                        .collect::<Vec<_>>()
                })
                .collect();
            cat_render(rows, &p)
        }
        // shapes with nothing meaningful behind them on a single node; `?help`
        // still has to list the right columns
        // what a field's columns take up is the closest thing here to a
        // fielddata cache, and it is reported per field
        "fielddata" => {
            let mut rows: Vec<Vec<(&str, String)>> = Vec::new();
            for name in store.names() {
                let Some(st) = store.get(&name) else { continue };
                let g = st.read();
                let loaded = g.loaded_fielddata.read().clone();
                let mut fields: Vec<(String, u64)> = g
                    .field_column_bytes()
                    .into_iter()
                    .filter(|(f, _)| loaded.contains(f))
                    .collect();
                fields.sort();
                // the path names which fields to report on
                if let Some(want) = target.as_deref() {
                    fields.retain(|(f, _)| want.split(',').any(|w| w.trim() == f));
                }
                for (field, bytes) in fields {
                    rows.push(vec![
                        ("id", crate::cluster::identity().id.as_str().to_string()),
                        ("host", "127.0.0.1".to_string()),
                        ("ip", "127.0.0.1".to_string()),
                        ("node", crate::cluster::identity().name.clone()),
                        ("field", field),
                        ("size", readable_bytes(bytes)),
                    ]);
                }
            }
            cat_render_cols(&["id", "host", "ip", "node", "field", "size"], rows, &p)
        }
        "allocation" => {
            cat_allocation(
                State(store.clone()),
                target.clone().map(axum::extract::Path),
                Query(p.clone()),
            )
            .await
        }
        "pending_tasks" => cat_named(&["insertOrder", "timeInQueue", "priority", "source"], &p),
        "plugins" => cat_named(&["name", "component", "version"], &p),
        "thread_pool" => cat_thread_pool(target.map(axum::extract::Path), Query(p)).await,
        // how each shard came to be where it is, one row per shard
        "recovery" => {
            const COLS: &[&str] = &[
                "index",
                "shard",
                "start_time",
                "start_time_millis",
                "stop_time",
                "stop_time_millis",
                "time",
                "type",
                "stage",
                "source_host",
                "source_node",
                "target_host",
                "target_node",
                "repository",
                "snapshot",
                "files",
                "files_recovered",
                "files_percent",
                "files_total",
                "bytes",
                "bytes_recovered",
                "bytes_percent",
                "bytes_total",
                "translog_ops",
                "translog_ops_recovered",
                "translog_ops_percent",
            ];
            let names = match target.as_deref().filter(|t| !t.is_empty()) {
                Some(t) => store.resolve(t),
                None => store.names(),
            };
            let mut rows: Vec<Vec<(&str, String)>> = Vec::new();
            for n in names {
                let Some(st) = store.get(&n) else { continue };
                let g = st.read();
                let existing = g.reader.searcher().num_docs() > 0 || g.closed;
                let kind = if g.restored {
                    "snapshot"
                } else if existing {
                    "existing_store"
                } else {
                    "empty_store"
                };
                for shard in 0..g.shard_count() {
                    rows.push(vec![
                        ("index", n.clone()),
                        ("shard", shard.to_string()),
                        ("start_time", "2020-01-01T00:00:00.000Z".into()),
                        ("start_time_millis", "1577836800000".into()),
                        ("stop_time", "2020-01-01T00:00:00.000Z".into()),
                        ("stop_time_millis", "1577836800000".into()),
                        ("time", "0ms".into()),
                        ("type", kind.into()),
                        ("stage", "done".into()),
                        ("source_host", "n/a".into()),
                        ("source_node", "n/a".into()),
                        ("target_host", "127.0.0.1".into()),
                        ("target_node", crate::cluster::identity().name.clone()),
                        ("repository", "n/a".into()),
                        ("snapshot", "n/a".into()),
                        ("files", "0".into()),
                        ("files_recovered", "0".into()),
                        ("files_percent", "100.0%".into()),
                        ("files_total", "0".into()),
                        ("bytes", "0b".into()),
                        ("bytes_recovered", "0b".into()),
                        ("bytes_percent", "100.0%".into()),
                        ("bytes_total", "0b".into()),
                        ("translog_ops", "0".into()),
                        ("translog_ops_recovered", "0".into()),
                        ("translog_ops_percent", "100.0%".into()),
                    ]);
                }
            }
            rows.sort_by(|a, b| {
                (a[0].1.clone(), a[1].1.clone()).cmp(&(b[0].1.clone(), b[1].1.clone()))
            });
            cat_render_cols(COLS, rows, &p)
        }
        "repositories" => {
            let mut rows: Vec<Vec<(&str, String)>> = store
                .repositories()
                .into_iter()
                .map(|(name, def)| {
                    vec![
                        ("id", name),
                        ("type", def.get("type").and_then(|t| t.as_str()).unwrap_or("fs").into()),
                    ]
                })
                .collect();
            rows.sort_by(|a, b| a[0].1.cmp(&b[0].1));
            cat_render_cols(&["id", "type"], rows, &p)
        }
        "snapshots" => {
            const COLS: &[&str] = &[
                "id",
                "status",
                "start_epoch",
                "start_time",
                "end_epoch",
                "end_time",
                "duration",
                "indices",
                "successful_shards",
                "failed_shards",
                "total_shards",
                "reason",
            ];
            let repos: Vec<String> = match target.as_deref().filter(|t| !t.is_empty()) {
                Some(t) => t.split(',').map(|s| s.trim().to_string()).collect(),
                None => store.repositories().into_keys().collect(),
            };
            let mut rows: Vec<Vec<(&str, String)>> = Vec::new();
            for repo in repos {
                for (name, snap) in store.snapshots(&repo) {
                    let n = |k: &str| {
                        snap.pointer(&format!("/shards/{k}")).and_then(|v| v.as_u64()).unwrap_or(0)
                    };
                    let indices = snap["indices"].as_array().map(|a| a.len()).unwrap_or(0);
                    rows.push(vec![
                        ("id", name),
                        // a snapshot that could not write every shard says
                        // so here as well as in its record
                        ("status", snap["state"].as_str().unwrap_or("SUCCESS").to_string()),
                        ("start_epoch", "0".into()),
                        ("start_time", "00:00:00".into()),
                        ("end_epoch", "0".into()),
                        ("end_time", "00:00:00".into()),
                        ("duration", "0s".into()),
                        ("indices", indices.to_string()),
                        ("successful_shards", n("successful").to_string()),
                        ("failed_shards", n("failed").to_string()),
                        ("total_shards", n("total").to_string()),
                        ("reason", String::new()),
                    ]);
                }
            }
            rows.sort_by(|a, b| a[0].1.cmp(&b[0].1));
            cat_render_cols(COLS, rows, &p)
        }
        "tasks" => cat_named(&["action", "task_id", "parent_task_id", "type", "start_time"], &p),
        "nodeattrs" => {
            let rows: Vec<Vec<(&str, String)>> = node_attrs()
                .into_iter()
                .map(|(attr, value)| {
                    vec![
                        ("node", crate::cluster::identity().name.clone()),
                        ("host", "127.0.0.1".to_string()),
                        ("ip", "127.0.0.1".to_string()),
                        ("attr", attr),
                        ("value", value),
                    ]
                })
                .collect();
            cat_render_cols(&["node", "host", "ip", "attr", "value"], rows, &p)
        }
        other => err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("unknown cat endpoint [{other}]"),
        ),
    }
}
