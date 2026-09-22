//! `_plugins/_search_relevance` -- query sets, judgments and experiments.
//!
//! The plugin keeps each of these in an index of its own and its list APIs are
//! searches over those indices, which is what these are: where the index is
//! there, what it holds is what comes back, and where it is not the answer is
//! the empty result the plugin gives before anything has been written. Nothing
//! here runs an experiment, so the counters of what has been executed are zero.

use super::*;

const QUERY_SETS: &str = "search-relevance-queryset";
const SEARCH_CONFIGS: &str = "search-relevance-search-config";
const JUDGMENTS: &str = "search-relevance-judgment";
const EXPERIMENTS: &str = "search-relevance-experiment";

/// The answer when there is no index to search.
///
/// The reference creates each of these indices the first time its list is
/// read, so it answers from an index with nothing in it. Nothing is created
/// here on a read, so no shard was asked and none succeeded -- but the hits
/// are the same empty hits, scored the way a search with no hit scores them.
fn nothing_searched() -> Value {
    json!({
        "took": 0,
        "timed_out": false,
        "_shards": {"total": 0, "successful": 0, "skipped": 0, "failed": 0},
        "hits": {"total": {"value": 0, "relation": "eq"}, "max_score": Value::Null, "hits": []},
    })
}

/// One of the plugin's lists: everything in its index, as a search answers it.
fn listed(store: &Store, index: &str, p: &Params) -> Response {
    if store.get(index).is_none() {
        return respond(p, nothing_searched());
    }
    let size = p.get("size").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1_000);
    let body = json!({"size": size, "query": {"match_all": {}}});
    let inner = Params::new();
    match crate::search::run(store, index, &body, &inner) {
        Ok(out) => respond(p, crate::search::envelope(out, &body, &inner)),
        Err(refusal) => refusal,
    }
}

/// `GET _plugins/_search_relevance/query_sets`
pub async fn query_sets(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    listed(&store, QUERY_SETS, &p)
}

/// `GET _plugins/_search_relevance/search_configurations`
pub async fn search_configurations(
    State(store): State<Store>,
    Query(p): Query<Params>,
) -> Response {
    listed(&store, SEARCH_CONFIGS, &p)
}

/// `GET _plugins/_search_relevance/judgments`
pub async fn judgments(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    listed(&store, JUDGMENTS, &p)
}

/// `GET _plugins/_search_relevance/experiments`
pub async fn experiments(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    listed(&store, EXPERIMENTS, &p)
}

/// `GET _plugins/_search_relevance/experiments/schedule` -- the experiments
/// waiting to run. Nothing runs one here, so nothing is ever waiting, and the
/// plugin keeps them apart from the experiments themselves.
pub async fn scheduled_experiments(Query(p): Query<Params>) -> Response {
    respond(&p, nothing_searched())
}

/// The two groups of counters this plugin keeps, and the node that counts
/// them. Every one is zero: no judgment has been generated and no experiment
/// has been run.
fn counters(only: Option<&str>) -> Value {
    let groups: [(&str, &[&str]); 2] = [
        (
            "judgments",
            &[
                "import_judgment_rating_generations",
                "llm_judgment_rating_generations",
                "ubi_judgment_rating_generations",
            ],
        ),
        (
            "experiments",
            &[
                "experiment_executions",
                "experiment_pairwise_comparison_executions",
                "experiment_pointwise_evaluation_executions",
                "experiment_hybrid_optimizer_executions",
            ],
        ),
    ];
    let mut out = serde_json::Map::new();
    for (group, names) in groups {
        let kept: serde_json::Map<String, Value> = names
            .iter()
            .filter(|n| only.is_none_or(|want| want == **n))
            .map(|n| ((*n).to_string(), json!(0)))
            .collect();
        if !kept.is_empty() {
            out.insert(group.to_string(), Value::Object(kept));
        }
    }
    Value::Object(out)
}

/// `GET _plugins/_search_relevance/stats`, with a stat in the path where one
/// was named.
pub async fn stats(Query(p): Query<Params>, uri: Uri) -> Response {
    let want = stat_in_path(&uri);
    if let Some(stat) = &want
        && counters(Some(stat)).as_object().is_none_or(|o| o.is_empty())
    {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("request [{}] contains unrecognized stat: [{stat}]", uri.path()),
        );
    }
    let me = crate::cluster::identity();
    let counted = counters(want.as_deref());
    // the cluster's version travels with the whole answer and not with one
    // stat, which is how the plugin answers it
    let info = if want.is_some() {
        json!({})
    } else {
        json!({"cluster_version": crate::OPENSEARCH_VERSION})
    };
    respond(
        &p,
        json!({
            "_nodes": {"total": 1, "successful": 1, "failed": 0},
            "cluster_name": me.cluster_name,
            "info": info,
            "all_nodes": counted,
            "nodes": {me.id.as_str(): counted},
        }),
    )
}
