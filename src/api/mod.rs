//! REST surface: index lifecycle and document CRUD.
//!
//! Response envelopes follow OpenSearch exactly -- `result`, `_version`,
//! `_shards`, `_seq_no` and friends are asserted on directly by the YAML suite.

use crate::store::{IdxState, Store, make_doc};
use anyhow::Result;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::collections::HashMap;
use velocore::TantivyDocument;
use velocore::schema::Value as _;

mod alias;
mod async_search;
pub use async_search::*;
pub mod json_position;
// The handlers are re-exported flat, so the routing table reads as one list
// and the module a handler lives in is a detail of where to find it.
pub use alias::*;
pub(crate) mod cat;
pub use cat::*;
mod cluster;
pub use cluster::*;
mod dangling;
pub use dangling::*;
mod datastream;
pub use datastream::*;
mod remote_store;
pub use remote_store::*;
pub(crate) mod doc;
pub use doc::*;
mod scripts;
pub use scripts::*;
pub(crate) mod mustache;
pub use mustache::*;
mod rank_eval;
pub use rank_eval::*;
mod indices;
pub use indices::*;
pub mod alerting;
pub mod flow_framework;
mod ingest;
pub mod insights;
pub mod ism;
pub mod job_scheduler;
pub mod knn;
pub mod ltr;
pub mod notifications;
pub mod perf_analyzer;
pub mod plugins;
pub mod replication;
pub mod rollup;
pub mod sm;
pub mod sql;
pub mod transform;
pub use ingest::*;
mod mapping;
pub use mapping::*;
mod nodes;
pub use nodes::*;
pub mod prometheus;
mod search_api;
pub use search_api::*;
mod settings;
pub use settings::*;
pub(crate) mod shared;
pub use shared::*;
mod snapshot;
pub use snapshot::*;
mod source;
pub use source::*;
mod hot_threads;
pub mod pools;
mod stats;
pub mod sysinfo;
pub use stats::*;
mod tasks;
pub use tasks::*;
mod template;
pub use template::*;

pub type Params = HashMap<String, String>;

// ---------------------------------------------------------------- index CRUD

// ------------------------------------------------------------------ mappings

// -------------------------------------------------------------- document CRUD

// ---------------------------------------------------------------------- bulk

// ------------------------------------------------------------ source filtering

// -------------------------------------------------------------------- scroll

// ------------------------------------------------------------- field mappings

// ----------------------------------------------------------------------- mget

// -------------------------------------------------------------------- update

// ------------------------------------------------------------- memory report

// --------------------------------------------------------------- force merge

const CAT_SEGMENT_COLS: &[&str] = &[
    "index",
    "shard",
    "prirep",
    "ip",
    "id",
    "segment",
    "generation",
    "docs.count",
    "docs.deleted",
    "size",
    "size.memory",
    "committed",
    "searchable",
    "version",
    "compound",
];

// --------------------------------------------------------------------- stats

/// `/_stats/{metric}` selects which sections to report; we always report all,
/// so the metric is only consumed to keep it off the index path.
pub const STATS_METRICS: &[&str] = &[
    "docs",
    "store",
    "indexing",
    "get",
    "search",
    "merges",
    "refresh",
    "flush",
    "warmer",
    "query_cache",
    "fielddata",
    "completion",
    "segments",
    "translog",
    "request_cache",
    "recovery",
    "_all",
    // the section is named `merges` but the metric may be asked for either way
    "merge",
];

// ------------------------------------------------------------------- explain

// ---------------------------------------------------------------- field_caps

// -------------------------------------------------------------------- alias

// -------------------------------------------------------------- cluster info

// -------------------------------------------------------------------- aliases

/// The keys an alias body may carry that are not part of the alias itself.
const ALIAS_ADDRESSING: &[&str] = &["index", "indices", "alias", "aliases"];
const ALIAS_OPTIONS: &[&str] = &[
    "filter",
    "routing",
    "index_routing",
    "search_routing",
    "is_write_index",
    "is_hidden",
    "must_exist",
];

// ------------------------------------------------------------------ templates

// ------------------------------------------------------- index open/close/get

// ---------------------------------------------------------------- cat helpers

pub const CAT_INDEX_COLS: &[&str] = &[
    "health",
    "status",
    "index",
    "uuid",
    "pri",
    "rep",
    "docs.count",
    "docs.deleted",
    "store.size",
    "pri.store.size",
    "creation.date",
    "creation.date.string",
    "system",
    "system.description",
];

/// What a system index is for, by the pattern of the descriptor that claims
/// it -- the descriptors the reference registers in its core. A dot-prefixed
/// name no descriptor matches is not a system index.
pub(crate) fn system_index_description(name: &str) -> Option<&'static str> {
    const DESCRIPTORS: &[(&str, &str)] = &[(".tasks", "Task Result Index")];
    DESCRIPTORS.iter().find(|(prefix, _)| name.starts_with(prefix)).map(|(_, d)| *d)
}

pub const CAT_TEMPLATE_COLS: &[&str] =
    &["name", "index_patterns", "order", "version", "composed_of"];

pub const CAT_ALLOCATION_COLS: &[&str] = &[
    "shards",
    "disk.indices",
    "disk.used",
    "disk.avail",
    "disk.total",
    "disk.percent",
    "host",
    "ip",
    "node",
];

pub const CAT_NODEATTRS_COLS: &[&str] =
    &["node", "id", "pid", "host", "ip", "port", "attr", "value"];

pub const CAT_PLUGINS_COLS: &[&str] = &["id", "name", "component", "version", "description"];

pub const CAT_THREAD_POOL_COLS: &[&str] = &[
    "node_name",
    "node_id",
    "id",
    "pid",
    "host",
    "ip",
    "port",
    "ephemeral_node_id",
    "name",
    "type",
    "active",
    "pool_size",
    "size",
    "queue",
    "queue_size",
    "rejected",
    "largest",
    "completed",
    "core",
    "min",
    "max",
    "keep_alive",
    "total_wait_time",
    "twt",
];

pub const CAT_TASKS_COLS: &[&str] = &[
    "action",
    "task_id",
    "parent_task_id",
    "type",
    "start_time",
    "timestamp",
    "running_time",
    "ip",
    "node",
    "description",
    "x_opaque_id",
];

pub const CAT_ALIAS_COLS: &[&str] =
    &["alias", "index", "filter", "routing.index", "routing.search", "is_write_index"];

pub const CAT_HEALTH_COLS: &[&str] = &[
    "epoch",
    "timestamp",
    "cluster",
    "status",
    "node.total",
    "node.data",
    "discovered_cluster_manager",
    "shards",
    "pri",
    "relo",
    "init",
    "unassign",
    "pending_tasks",
    "max_task_wait_time",
    "active_shards_percent",
];

// ------------------------------------------------------------ generic cat API

// -------------------------------------------------- composable index templates

// ---------------------------------------------------------------- snapshots

// ---------------------------------------------------------------- pipelines

// ------------------------------------------------------------- data streams

// --------------------------------------------------------------- nodes & misc

/// Write one audit record into the audit index, making the index when it
/// is not there; the caller is the audit sink's own thread.
pub fn index_audit_document(
    store: &crate::store::Store,
    index: &str,
    doc: &serde_json::Value,
) -> anyhow::Result<()> {
    if store.get(index).is_none() {
        store.create(
            index,
            &serde_json::json!({"settings": {"number_of_shards": 1, "number_of_replicas": 0}}),
        )?;
    }
    let Some(st) = store.get(index) else { return Ok(()) };
    let mut g = st.write();
    let id = g.next_auto_id();
    let _ = crate::api::doc::write_doc_internal(&mut g, &id, doc.clone(), "index", None, None)
        .map_err(|_| anyhow::anyhow!("audit record refused"))?;
    // a record is for reading as soon as it is written
    let _ = g.refresh();
    Ok(())
}
