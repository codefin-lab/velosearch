//! `GET /_prometheus/metrics` -- what this node is doing, in the format a
//! scraper reads.
//!
//! Everything here was already answered by `_nodes/stats` and
//! `_cluster/health`, and that is exactly the problem: a monitoring system
//! cannot read them. Prometheus scrapes a text endpoint; Grafana's OpenSearch
//! dashboards are built on the metric names the `prometheus-exporter` plugin
//! publishes. Without one, an operator watching this node has to write the
//! exporter themselves -- which means, in practice, that nobody is watching
//! it, and the ceilings a node now refuses at (`docs/limits.md`) are refusals
//! nobody sees until a client complains.
//!
//! So the numbers are not computed again here. The node's own statistics are
//! asked for the way any caller asks for them, and rendered: one source of
//! truth, in two formats. The names are the plugin's, `opensearch_` and all,
//! so a dashboard written for OpenSearch reads this node without being
//! rewritten -- the same reason every other answer here is spelled the way
//! OpenSearch spells it.
//!
//! What a scrape costs is what `_nodes/stats` costs, which is a reading of
//! the operating system and a walk of the open indices. `?indices=false`
//! leaves out the per-index series for a node holding so many that the
//! cardinality is the problem.

use super::*;

/// The exposition format's escaping: a label value may hold none of these.
fn escaped(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// One metric family, written out once.
struct Metrics {
    out: String,
    /// `cluster`, `node` and `nodeid`, which every series here carries
    common: String,
}

impl Metrics {
    fn new(cluster: &str, node: &str, node_id: &str) -> Metrics {
        Metrics {
            out: String::with_capacity(16 * 1024),
            common: format!(
                "cluster=\"{}\",node=\"{}\",nodeid=\"{}\"",
                escaped(cluster),
                escaped(node),
                escaped(node_id)
            ),
        }
    }

    /// Declare a family. A scraper wants the type and the help once, before
    /// the series.
    fn family(&mut self, name: &str, kind: &str, help: &str) {
        self.out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
    }

    /// One series of a family already declared.
    fn value(&mut self, name: &str, labels: &[(&str, &str)], v: f64) {
        let mut all = self.common.clone();
        for (k, val) in labels {
            all.push_str(&format!(",{k}=\"{}\"", escaped(val)));
        }
        // a whole number is written as one: `1` rather than `1.0`, which is
        // what every exporter emits and what a reader expects
        if v.fract() == 0.0 && v.abs() < 9e15 {
            self.out.push_str(&format!("{name}{{{all}}} {}\n", v as i64));
        } else {
            self.out.push_str(&format!("{name}{{{all}}} {v}\n"));
        }
    }

    /// A family of one series, which most of these are.
    fn one(&mut self, name: &str, kind: &str, help: &str, v: f64) {
        self.family(name, kind, help);
        self.value(name, &[], v);
    }
}

/// A number out of the statistics, whichever way it was written.
fn num(v: &Value, path: &str) -> f64 {
    v.pointer(path)
        .and_then(|n| n.as_f64().or_else(|| n.as_str().and_then(|s| s.parse().ok())))
        .unwrap_or(0.0)
}

/// `GET /_prometheus/metrics`
pub async fn metrics(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    let me = crate::cluster::identity();
    let per_index = p.get("indices").map(|v| v != "false").unwrap_or(true);

    // the node's own statistics, asked for as a caller asks for them
    let (_, stats) = crate::api::shared::error_parts_or_body(crate::api::nodes_stats_of(
        &store,
        &Params::new(),
        &[me.id.as_str().to_string()],
        None,
        None,
        if per_index { "indices" } else { "node" },
    ))
    .await;
    let node =
        stats.pointer(&format!("/nodes/{}", me.id.as_str())).cloned().unwrap_or_else(|| json!({}));

    // and the cluster's health, the same way
    let (_, health) = crate::api::shared::error_parts_or_body(
        crate::api::cluster_health(State(store.clone()), None, Query(Params::new())).await,
    )
    .await;

    let mut m = Metrics::new(&me.cluster_name, &me.name, me.id.as_str());

    m.family("velosearch_build_info", "gauge", "The build this node is running, as labels.");
    m.value(
        "velosearch_build_info",
        &[
            ("version", crate::VERSION),
            ("opensearch_version", crate::OPENSEARCH_VERSION),
            ("build_hash", crate::build_hash()),
        ],
        1.0,
    );

    // ------------------------------------------------------------ cluster
    let status = match health.get("status").and_then(|v| v.as_str()).unwrap_or("red") {
        "green" => 0.0,
        "yellow" => 1.0,
        _ => 2.0,
    };
    m.one(
        "opensearch_cluster_status",
        "gauge",
        "Cluster status: 0 green, 1 yellow, 2 red.",
        status,
    );
    for (name, path, help) in [
        ("opensearch_cluster_nodes_number", "/number_of_nodes", "Nodes in the cluster."),
        (
            "opensearch_cluster_datanodes_number",
            "/number_of_data_nodes",
            "Nodes of the cluster that hold data.",
        ),
        ("opensearch_cluster_shards_active_number", "/active_shards", "Shards that are active."),
        (
            "opensearch_cluster_shards_active_primary_number",
            "/active_primary_shards",
            "Primary shards that are active.",
        ),
        (
            "opensearch_cluster_shards_relocating_number",
            "/relocating_shards",
            "Shards being moved between nodes.",
        ),
        (
            "opensearch_cluster_shards_initializing_number",
            "/initializing_shards",
            "Shards being brought up.",
        ),
        (
            "opensearch_cluster_shards_unassigned_number",
            "/unassigned_shards",
            "Shards no node holds.",
        ),
        (
            "opensearch_cluster_pending_tasks_number",
            "/number_of_pending_tasks",
            "Cluster tasks waiting to be applied.",
        ),
    ] {
        m.one(name, "gauge", help, num(&health, path));
    }

    // ----------------------------------------------------------- the machine
    for (name, path, help) in [
        ("opensearch_os_cpu_percent", "/os/cpu/percent", "Processor use of the machine."),
        (
            "opensearch_os_load_average_one_minute",
            "/os/cpu/load_average/1m",
            "Load average over one minute.",
        ),
        (
            "opensearch_os_load_average_five_minutes",
            "/os/cpu/load_average/5m",
            "Load average over five minutes.",
        ),
        (
            "opensearch_os_load_average_fifteen_minutes",
            "/os/cpu/load_average/15m",
            "Load average over fifteen minutes.",
        ),
        ("opensearch_os_mem_total_bytes", "/os/mem/total_in_bytes", "Memory the machine has."),
        ("opensearch_os_mem_free_bytes", "/os/mem/free_in_bytes", "Memory the machine has free."),
        ("opensearch_os_mem_used_bytes", "/os/mem/used_in_bytes", "Memory the machine is using."),
        ("opensearch_os_swap_used_bytes", "/os/swap/used_in_bytes", "Swap the machine is using."),
        (
            "opensearch_process_cpu_percent",
            "/process/cpu/percent",
            "Processor use of this process.",
        ),
        (
            "opensearch_process_file_descriptors_open_number",
            "/process/open_file_descriptors",
            "File descriptors this process holds.",
        ),
        (
            "opensearch_process_file_descriptors_max_number",
            "/process/max_file_descriptors",
            "File descriptors this process may hold.",
        ),
        (
            "opensearch_process_mem_total_virtual_bytes",
            "/process/mem/total_virtual_in_bytes",
            "Virtual memory this process has mapped.",
        ),
    ] {
        m.one(name, "gauge", help, num(&node, path));
    }

    // There is no JVM. `jvm` here is what stands for it in `_nodes/stats`:
    // the memory the allocator holds for this node's own data, which is what
    // a heap gauge on a dashboard is watched for.
    for (name, path, help) in [
        (
            "opensearch_jvm_mem_heap_used_bytes",
            "/jvm/mem/heap_used_in_bytes",
            "Memory the allocator holds for this node's data; what stands for a heap here.",
        ),
        (
            "opensearch_jvm_mem_heap_max_bytes",
            "/jvm/mem/heap_max_in_bytes",
            "As far as that can grow: the machine's memory.",
        ),
        (
            "opensearch_jvm_mem_nonheap_used_bytes",
            "/jvm/mem/non_heap_used_in_bytes",
            "What the process holds besides: mapped index files, thread stacks.",
        ),
        ("opensearch_jvm_threads_number", "/jvm/threads/count", "Threads this process is running."),
    ] {
        m.one(name, "gauge", help, num(&node, path));
    }
    m.one(
        "opensearch_jvm_uptime_seconds",
        "gauge",
        "How long this node has been up.",
        num(&node, "/jvm/uptime_in_millis") / 1000.0,
    );

    for (name, path, help) in [
        ("opensearch_fs_total_total_bytes", "/fs/total/total_in_bytes", "Size of the data disk."),
        ("opensearch_fs_total_free_bytes", "/fs/total/free_in_bytes", "Free space on it."),
        (
            "opensearch_fs_total_available_bytes",
            "/fs/total/available_in_bytes",
            "Space on it this process may use.",
        ),
    ] {
        m.one(name, "gauge", help, num(&node, path));
    }

    // ------------------------------------------------------------- the pools
    //
    // What a node is holding back and what it has refused: the two numbers
    // that say a node is past what it can carry, and the reason this endpoint
    // is worth having at all.
    m.family("opensearch_threadpool_threads_number", "gauge", "Requests of this kind run at once.");
    for pool in crate::api::pools::POOLS.iter() {
        m.value(
            "opensearch_threadpool_threads_number",
            &[("name", pool.name)],
            pool.threads() as f64,
        );
    }
    m.family(
        "opensearch_threadpool_tasks_number",
        "gauge",
        "Requests of this kind running or waiting.",
    );
    for pool in crate::api::pools::POOLS.iter() {
        m.value(
            "opensearch_threadpool_tasks_number",
            &[("name", pool.name), ("type", "active")],
            pool.active() as f64,
        );
        m.value(
            "opensearch_threadpool_tasks_number",
            &[("name", pool.name), ("type", "queue")],
            pool.queue() as f64,
        );
    }
    m.family(
        "opensearch_threadpool_tasks_count",
        "counter",
        "Requests of this kind finished, and refused because the pool and its queue were full.",
    );
    for pool in crate::api::pools::POOLS.iter() {
        m.value(
            "opensearch_threadpool_tasks_count",
            &[("name", pool.name), ("type", "completed")],
            pool.completed() as f64,
        );
        m.value(
            "opensearch_threadpool_tasks_count",
            &[("name", pool.name), ("type", "rejected")],
            pool.rejected() as f64,
        );
    }

    // ---------------------------------------------------------- the breakers
    m.family("opensearch_circuitbreaker_estimated_bytes", "gauge", "What this breaker is holding.");
    m.family("opensearch_circuitbreaker_limit_bytes", "gauge", "What it may hold.");
    m.family(
        "opensearch_circuitbreaker_tripped_count",
        "counter",
        "Requests this breaker has refused since the node started.",
    );
    for b in crate::breaker::all() {
        let used = if b.name == "parent" {
            crate::breaker::parent_used(Some(&store)) as f64
        } else {
            b.used() as f64
        };
        m.value("opensearch_circuitbreaker_estimated_bytes", &[("name", b.name)], used);
        m.value(
            "opensearch_circuitbreaker_limit_bytes",
            &[("name", b.name)],
            b.limit(Some(&store)) as f64,
        );
        m.value("opensearch_circuitbreaker_tripped_count", &[("name", b.name)], b.tripped() as f64);
    }

    // ------------------------------------------------------------ the indices
    let totals = node.pointer("/indices").cloned().unwrap_or_else(|| json!({}));
    for (name, kind, path, help) in INDEX_SERIES {
        m.one(name, kind, help, num(&totals, path));
    }
    m.one(
        "opensearch_indices_indexing_index_time_seconds",
        "counter",
        "Time spent indexing.",
        num(&totals, "/indexing/index_time_in_millis") / 1000.0,
    );
    m.one(
        "opensearch_indices_search_query_time_seconds",
        "counter",
        "Time spent on the query phase.",
        num(&totals, "/search/query_time_in_millis") / 1000.0,
    );
    m.one(
        "opensearch_indices_search_fetch_time_seconds",
        "counter",
        "Time spent on the fetch phase.",
        num(&totals, "/search/fetch_time_in_millis") / 1000.0,
    );

    if per_index && let Some(each) = node.pointer("/indices/indices").and_then(|v| v.as_object()) {
        m.family("opensearch_index_doc_number", "gauge", "Documents in this index.");
        for (index, v) in each {
            m.value("opensearch_index_doc_number", &[("index", index)], num(v, "/docs/count"));
        }
        m.family("opensearch_index_store_size_bytes", "gauge", "What this index takes on disk.");
        for (index, v) in each {
            m.value(
                "opensearch_index_store_size_bytes",
                &[("index", index)],
                num(v, "/store/size_in_bytes"),
            );
        }
        m.family(
            "opensearch_index_search_query_count",
            "counter",
            "Queries answered out of this index.",
        );
        for (index, v) in each {
            m.value(
                "opensearch_index_search_query_count",
                &[("index", index)],
                num(v, "/search/query_total"),
            );
        }
        m.family(
            "opensearch_index_indexing_index_count",
            "counter",
            "Documents written into this index.",
        );
        for (index, v) in each {
            m.value(
                "opensearch_index_indexing_index_count",
                &[("index", index)],
                num(v, "/indexing/index_total"),
            );
        }
    }

    ([("content-type", "text/plain; version=0.0.4; charset=utf-8")], m.out).into_response()
}

/// The node-wide index series: the name, what it is, where it is in the
/// statistics, and what it means.
const INDEX_SERIES: &[(&str, &str, &str, &str)] = &[
    ("opensearch_indices_doc_number", "gauge", "/docs/count", "Documents this node holds."),
    (
        "opensearch_indices_doc_deleted_number",
        "gauge",
        "/docs/deleted",
        "Documents deleted but not yet merged away.",
    ),
    (
        "opensearch_indices_store_size_bytes",
        "gauge",
        "/store/size_in_bytes",
        "What this node's indices take on disk.",
    ),
    (
        "opensearch_indices_indexing_index_count",
        "counter",
        "/indexing/index_total",
        "Documents written.",
    ),
    (
        "opensearch_indices_indexing_index_current_number",
        "gauge",
        "/indexing/index_current",
        "Writes in flight.",
    ),
    (
        "opensearch_indices_indexing_delete_count",
        "counter",
        "/indexing/delete_total",
        "Documents deleted.",
    ),
    (
        "opensearch_indices_search_query_count",
        "counter",
        "/search/query_total",
        "Queries answered.",
    ),
    (
        "opensearch_indices_search_query_current_number",
        "gauge",
        "/search/query_current",
        "Queries in flight.",
    ),
    (
        "opensearch_indices_search_fetch_count",
        "counter",
        "/search/fetch_total",
        "Fetch phases run.",
    ),
    (
        "opensearch_indices_search_scroll_current_number",
        "gauge",
        "/search/scroll_current",
        "Scrolls open.",
    ),
    ("opensearch_indices_refresh_count", "counter", "/refresh/total", "Refreshes made."),
    ("opensearch_indices_flush_count", "counter", "/flush/total", "Flushes made."),
    ("opensearch_indices_merges_count", "counter", "/merges/total", "Merges finished."),
    ("opensearch_indices_segments_number", "gauge", "/segments/count", "Segments open."),
    (
        "opensearch_indices_translog_operations_number",
        "gauge",
        "/translog/operations",
        "Writes recorded but not yet committed.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_value_cannot_break_out_of_its_quotes() {
        assert_eq!(escaped(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escaped("a\nb"), "a\\nb");
        assert_eq!(escaped(r"a\b"), r"a\\b");
    }

    #[test]
    fn a_series_carries_what_says_which_node_it_is() {
        let mut m = Metrics::new("velo", "n1", "abc");
        m.one("a_metric", "gauge", "Help.", 3.0);
        assert!(m.out.contains("# TYPE a_metric gauge"));
        assert!(m.out.contains("a_metric{cluster=\"velo\",node=\"n1\",nodeid=\"abc\"} 3\n"));
    }

    #[test]
    fn a_whole_number_is_written_as_one() {
        let mut m = Metrics::new("c", "n", "i");
        m.one("whole", "gauge", "Help.", 12.0);
        m.one("fraction", "gauge", "Help.", 0.5);
        assert!(m.out.contains("} 12\n"));
        assert!(m.out.contains("} 0.5\n"));
    }

    #[test]
    fn a_number_that_is_not_there_is_zero_rather_than_missing() {
        let v = json!({"a": {"b": 2}});
        assert_eq!(num(&v, "/a/b"), 2.0);
        assert_eq!(num(&v, "/a/c"), 0.0);
    }
}
