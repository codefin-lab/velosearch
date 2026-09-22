//! Where every copy of every shard goes: OpenSearch's deciders and its
//! balancer, as one pure function from a routing table to the next.
//!
//! `reroute` takes what the cluster manager knows -- the nodes, the
//! indices and their settings, the cluster settings, the table as it was
//! and the time -- and gives the table as it should be: copies on nodes
//! that left become unassigned (a replica waits out
//! `index.unassigned.node_left.delayed_timeout` first; a primary is
//! replaced by an in-sync replica), unassigned copies are placed on the
//! node the deciders allow and the balancer weighs lightest, and once
//! every copy is active the balancer moves copies from heavy nodes to
//! light ones, one at a time. `explain` asks the same deciders the
//! questions `_cluster/allocation/explain` answers, in its words.
//!
//! The deciders are the plugin's: `same_shard`, `filter`, `enable`,
//! `replica_after_primary_active`, `throttling`, `shards_limit`,
//! `awareness`, `max_retry`, `rebalance_only_when_active`,
//! `cluster_rebalance`, `concurrent_rebalance`, `node_version` and
//! `disk_threshold` (the last two always yes, there being one version and
//! no disk accounting yet). Nothing in here does I/O and nothing reads a
//! clock: the time comes in as an argument, as ADR 0002 asks.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::clock::Millis;
use super::state::{
    ClusterState, DiscoveryNode, IndexMetadata, RoutingTable, ShardRouting, ShardState,
    UnassignedInfo,
};
use super::transport::NodeId;

/// The version this node reports, as `_nodes` has it.
const VERSION: &str = crate::OPENSEARCH_VERSION;

/// What a decider says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Decision {
    Yes,
    Throttle,
    No,
}

impl Decision {
    pub fn label(self) -> &'static str {
        match self {
            Decision::Yes => "YES",
            Decision::Throttle => "THROTTLE",
            Decision::No => "NO",
        }
    }
}

/// One decider's verdict, with the sentence `_cluster/allocation/explain` shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub decider: &'static str,
    pub decision: Decision,
    pub explanation: String,
}

fn verdict(decider: &'static str, decision: Decision, explanation: impl Into<String>) -> Verdict {
    Verdict { decider, decision, explanation: explanation.into() }
}

/// The worst of a set of verdicts.
pub fn overall(vs: &[Verdict]) -> Decision {
    vs.iter().map(|v| v.decision).max().unwrap_or(Decision::Yes)
}

/// `cluster.routing.allocation.enable` and the index setting of the same name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Enable {
    All,
    Primaries,
    NewPrimaries,
    None,
}

impl Enable {
    fn parse(s: &str) -> Enable {
        match s.to_ascii_lowercase().as_str() {
            "primaries" => Enable::Primaries,
            "new_primaries" => Enable::NewPrimaries,
            "none" => Enable::None,
            _ => Enable::All,
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Enable::All => "all",
            Enable::Primaries => "primaries",
            Enable::NewPrimaries => "new_primaries",
            Enable::None => "none",
        }
    }
}

/// `cluster.routing.rebalance.enable` and the index setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebalanceEnable {
    All,
    Primaries,
    Replicas,
    None,
}

impl RebalanceEnable {
    fn parse(s: &str) -> RebalanceEnable {
        match s.to_ascii_lowercase().as_str() {
            "primaries" => RebalanceEnable::Primaries,
            "replicas" => RebalanceEnable::Replicas,
            "none" => RebalanceEnable::None,
            _ => RebalanceEnable::All,
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            RebalanceEnable::All => "all",
            RebalanceEnable::Primaries => "primaries",
            RebalanceEnable::Replicas => "replicas",
            RebalanceEnable::None => "none",
        }
    }
}

/// `cluster.routing.allocation.allow_rebalance`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllowRebalance {
    Always,
    IndicesPrimariesActive,
    IndicesAllActive,
}

impl AllowRebalance {
    fn parse(s: &str) -> AllowRebalance {
        match s.to_ascii_lowercase().as_str() {
            "always" => AllowRebalance::Always,
            "indices_primaries_active" => AllowRebalance::IndicesPrimariesActive,
            _ => AllowRebalance::IndicesAllActive,
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            AllowRebalance::Always => "always",
            AllowRebalance::IndicesPrimariesActive => "indices_primaries_active",
            AllowRebalance::IndicesAllActive => "indices_all_active",
        }
    }
}

/// `include`, `exclude` and `require` filters: attribute -> accepted values.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filters {
    pub include: BTreeMap<String, Vec<String>>,
    pub exclude: BTreeMap<String, Vec<String>>,
    pub require: BTreeMap<String, Vec<String>>,
}

impl Filters {
    /// The node's value for an attribute: the built-in `_name`, `_id`,
    /// `_ip`, `_host` and `_publish_ip`, or `node.attr.*`.
    fn node_value(node: &DiscoveryNode, attr: &str) -> Option<String> {
        match attr {
            "_name" => Some(node.name.clone()),
            "_id" => Some(node.id.as_str().to_string()),
            "_ip" | "_host_ip" | "_publish_ip" | "_host" => {
                Some(node.transport_address.split(':').next().unwrap_or("").to_string())
            }
            _ => node.attributes.get(attr).cloned(),
        }
    }

    fn matches_any(values: &[String], have: Option<&String>) -> bool {
        match have {
            Some(h) => {
                values.iter().any(|v| v == h || (v.contains('*') && crate::store::glob_match(v, h)))
            }
            None => false,
        }
    }

    /// Whether the node passes, and if not which filter it failed.
    fn check(
        &self,
        node: &DiscoveryNode,
    ) -> Option<(&'static str, &BTreeMap<String, Vec<String>>)> {
        // require: every attribute must match
        for (attr, values) in &self.require {
            if !Self::matches_any(values, Self::node_value(node, attr).as_ref()) {
                return Some(("require", &self.require));
            }
        }
        // include: at least one attribute must match
        if !self.include.is_empty() {
            let any = self.include.iter().any(|(attr, values)| {
                Self::matches_any(values, Self::node_value(node, attr).as_ref())
            });
            if !any {
                return Some(("include", &self.include));
            }
        }
        // exclude: no attribute may match
        for (attr, values) in &self.exclude {
            if Self::matches_any(values, Self::node_value(node, attr).as_ref()) {
                return Some(("exclude", &self.exclude));
            }
        }
        None
    }
}

fn filters_text(f: &BTreeMap<String, Vec<String>>) -> String {
    f.iter().map(|(k, v)| format!("{k}:\"{}\"", v.join(","))).collect::<Vec<_>>().join(",")
}

/// A dotted setting from a settings document, whichever way it was
/// written: fully nested, flat, or nested part of the way
/// (`{"index": {"unassigned.node_left.delayed_timeout": "5s"}}`).
pub fn setting<'a>(settings: &'a Value, key: &str) -> Option<&'a Value> {
    fn walk<'a>(v: &'a Value, parts: &[&str]) -> Option<&'a Value> {
        if parts.is_empty() {
            return if v.is_null() { None } else { Some(v) };
        }
        let o = v.as_object()?;
        // the longest key first, so `a.b.c` beats `a` + `b.c`
        for n in (1..=parts.len()).rev() {
            let head = parts[..n].join(".");
            if let Some(next) = o.get(&head)
                && let Some(found) = walk(next, &parts[n..])
            {
                return Some(found);
            }
        }
        None
    }
    let parts: Vec<&str> = key.split('.').collect();
    walk(settings, &parts)
}

fn setting_str(settings: &Value, key: &str) -> Option<String> {
    match setting(settings, key)? {
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

fn setting_u64(settings: &Value, key: &str) -> Option<u64> {
    match setting(settings, key)? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn setting_f64(settings: &Value, key: &str) -> Option<f64> {
    match setting(settings, key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn filters_from(settings: &Value, prefix: &str) -> Filters {
    let mut f = Filters::default();
    for (kind, into) in
        [("include", &mut f.include), ("exclude", &mut f.exclude), ("require", &mut f.require)]
    {
        let values_of = |v: &Value| -> Vec<String> {
            match v {
                Value::String(s) => {
                    s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
                }
                Value::Array(a) => {
                    a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()
                }
                Value::Null => Vec::new(),
                other => vec![other.to_string()],
            }
        };
        if let Some(Value::Object(o)) = setting(settings, &format!("{prefix}.{kind}")) {
            for (attr, v) in o {
                let values = values_of(v);
                if !values.is_empty() {
                    into.insert(attr.clone(), values);
                }
            }
        }
        // the same written flat: `cluster.routing.allocation.exclude._name`
        let head = format!("{prefix}.{kind}.");
        if let Value::Object(o) = settings {
            for (k, v) in o {
                if let Some(attr) = k.strip_prefix(&head) {
                    let values = values_of(v);
                    if !values.is_empty() {
                        into.insert(attr.to_string(), values);
                    }
                }
            }
        }
    }
    f
}

/// A time value in milliseconds (`1m`, `30s`, `500ms`, `0`).
pub fn time_ms(s: &str) -> Option<Millis> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let n: f64 = s[..split].trim().parse().ok()?;
    let mult = match &s[split..] {
        "" | "ms" => 1.0,
        "s" => 1_000.0,
        "m" => 60_000.0,
        "h" => 3_600_000.0,
        "d" => 86_400_000.0,
        _ => return None,
    };
    Some((n * mult) as Millis)
}

/// The cluster-level settings allocation reads, `persistent` under `transient`.
#[derive(Clone, Debug)]
pub struct ClusterSettings {
    pub enable: Enable,
    pub rebalance_enable: RebalanceEnable,
    pub allow_rebalance: AllowRebalance,
    pub cluster_concurrent_rebalance: u64,
    pub node_concurrent_incoming: u64,
    pub node_concurrent_outgoing: u64,
    pub node_initial_primaries: u64,
    pub node_initial_replicas: u64,
    pub total_shards_per_node: i64,
    pub awareness_attributes: Vec<String>,
    pub awareness_forced: BTreeMap<String, Vec<String>>,
    pub filters: Filters,
    pub balance_shard: f64,
    pub balance_index: f64,
    pub balance_threshold: f64,
}

impl Default for ClusterSettings {
    fn default() -> ClusterSettings {
        ClusterSettings {
            enable: Enable::All,
            rebalance_enable: RebalanceEnable::All,
            allow_rebalance: AllowRebalance::IndicesAllActive,
            cluster_concurrent_rebalance: 2,
            node_concurrent_incoming: 2,
            node_concurrent_outgoing: 2,
            node_initial_primaries: 4,
            node_initial_replicas: 4,
            total_shards_per_node: -1,
            awareness_attributes: Vec::new(),
            awareness_forced: BTreeMap::new(),
            filters: Filters::default(),
            balance_shard: 0.45,
            balance_index: 0.55,
            balance_threshold: 1.0,
        }
    }
}

impl ClusterSettings {
    /// From `_cluster/settings` as the store keeps it: `{"persistent": {...},
    /// "transient": {...}}`, flat or nested keys.
    pub fn from_value(v: &Value) -> ClusterSettings {
        let mut merged = json!({});
        for section in ["persistent", "transient"] {
            if let Some(Value::Object(o)) = v.get(section) {
                for (k, val) in o {
                    merged[k] = val.clone();
                }
            }
        }
        let mut s = ClusterSettings::default();
        let g = |k: &str| setting_str(&merged, k);
        if let Some(e) = g("cluster.routing.allocation.enable") {
            s.enable = Enable::parse(&e);
        }
        if let Some(e) = g("cluster.routing.rebalance.enable") {
            s.rebalance_enable = RebalanceEnable::parse(&e);
        }
        if let Some(e) = g("cluster.routing.allocation.allow_rebalance") {
            s.allow_rebalance = AllowRebalance::parse(&e);
        }
        if let Some(n) =
            setting_u64(&merged, "cluster.routing.allocation.cluster_concurrent_rebalance")
        {
            s.cluster_concurrent_rebalance = n;
        }
        if let Some(n) =
            setting_u64(&merged, "cluster.routing.allocation.node_concurrent_recoveries")
        {
            s.node_concurrent_incoming = n;
            s.node_concurrent_outgoing = n;
        }
        if let Some(n) =
            setting_u64(&merged, "cluster.routing.allocation.node_concurrent_incoming_recoveries")
        {
            s.node_concurrent_incoming = n;
        }
        if let Some(n) =
            setting_u64(&merged, "cluster.routing.allocation.node_concurrent_outgoing_recoveries")
        {
            s.node_concurrent_outgoing = n;
        }
        if let Some(n) =
            setting_u64(&merged, "cluster.routing.allocation.node_initial_primaries_recoveries")
        {
            s.node_initial_primaries = n;
        }
        if let Some(n) =
            setting_u64(&merged, "cluster.routing.allocation.node_initial_replicas_recoveries")
        {
            s.node_initial_replicas = n;
        }
        if let Some(t) = g("cluster.routing.allocation.total_shards_per_node") {
            s.total_shards_per_node = t.parse().unwrap_or(-1);
        }
        if let Some(a) = g("cluster.routing.allocation.awareness.attributes") {
            s.awareness_attributes =
                a.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
        }
        if let Some(Value::Object(force)) =
            setting(&merged, "cluster.routing.allocation.awareness.force")
        {
            for (attr, v) in force {
                if let Some(vals) = setting_str(v, "values") {
                    s.awareness_forced.insert(
                        attr.clone(),
                        vals.split(',')
                            .map(|x| x.trim().to_string())
                            .filter(|x| !x.is_empty())
                            .collect(),
                    );
                }
            }
        }
        s.filters = filters_from(&merged, "cluster.routing.allocation");
        if let Some(f) = setting_f64(&merged, "cluster.routing.allocation.balance.shard") {
            s.balance_shard = f;
        }
        if let Some(f) = setting_f64(&merged, "cluster.routing.allocation.balance.index") {
            s.balance_index = f;
        }
        if let Some(f) = setting_f64(&merged, "cluster.routing.allocation.balance.threshold") {
            s.balance_threshold = f;
        }
        s
    }
}

/// The index-level settings allocation reads.
#[derive(Clone, Debug)]
pub struct IndexSettings {
    pub enable: Option<Enable>,
    pub rebalance_enable: Option<RebalanceEnable>,
    pub total_shards_per_node: i64,
    pub filters: Filters,
    pub max_retries: u64,
    pub delayed_timeout: Millis,
}

impl IndexSettings {
    pub fn from_metadata(m: &IndexMetadata) -> IndexSettings {
        let s = &m.settings;
        IndexSettings {
            enable: setting_str(s, "index.routing.allocation.enable").map(|e| Enable::parse(&e)),
            rebalance_enable: setting_str(s, "index.routing.rebalance.enable")
                .map(|e| RebalanceEnable::parse(&e)),
            total_shards_per_node: setting_str(s, "index.routing.allocation.total_shards_per_node")
                .and_then(|t| t.parse().ok())
                .unwrap_or(-1),
            filters: filters_from(s, "index.routing.allocation"),
            max_retries: setting_u64(s, "index.allocation.max_retries").unwrap_or(5),
            delayed_timeout: setting_str(s, "index.unassigned.node_left.delayed_timeout")
                .and_then(|t| time_ms(&t))
                .unwrap_or(60_000),
        }
    }
}

/// Everything `reroute` and `explain` look at.
pub struct Context<'a> {
    pub nodes: &'a BTreeMap<NodeId, DiscoveryNode>,
    pub indices: &'a BTreeMap<String, IndexMetadata>,
    pub cluster: &'a ClusterSettings,
    /// where an index's primary data lives, for indices the manager holds
    pub primary_home: &'a BTreeMap<String, NodeId>,
    /// the indices among those whose home already holds documents: a primary
    /// that may not stay at home is moved from there rather than made again
    /// empty somewhere else, which would lose them
    pub home_holds_documents: &'a BTreeSet<String>,
    /// the index copies each node holds on disk: name, uuid and the
    /// allocation id the copy was given
    pub held: &'a BTreeMap<NodeId, Vec<(String, String, String)>>,
    pub now: Millis,
}

impl Context<'_> {
    fn data_nodes(&self) -> Vec<&DiscoveryNode> {
        self.nodes.values().filter(|n| n.is_data()).collect()
    }

    fn index_settings(&self, index: &str) -> IndexSettings {
        self.indices.get(index).map(IndexSettings::from_metadata).unwrap_or_else(|| IndexSettings {
            enable: None,
            rebalance_enable: None,
            total_shards_per_node: -1,
            filters: Filters::default(),
            max_retries: 5,
            delayed_timeout: 60_000,
        })
    }
}

/// How a copy of a shard describes itself in the plugin's messages:
/// `[[index][0], node[x], [P], s[STARTED], a[id=...]]`.
fn short_summary(c: &ShardRouting) -> String {
    let node = match &c.node {
        Some(n) => format!("node[{}]", n.as_str()),
        None => "node[null]".into(),
    };
    let reloc = match &c.relocating_node {
        Some(r) => format!(", relocating [{}]", r.as_str()),
        None => String::new(),
    };
    let kind = if c.primary { "[P]" } else { "[R]" };
    let source = if matches!(c.state, ShardState::Unassigned | ShardState::Initializing) {
        let kind = if c.relocating_node.is_some() || !c.primary {
            "peer recovery"
        } else if c.unassigned.as_ref().map(|u| u.reason == "INDEX_CREATED").unwrap_or(true) {
            "new shard recovery"
        } else {
            "existing store recovery"
        };
        format!(", recovery_source[{kind}]")
    } else {
        String::new()
    };
    let alloc = match &c.allocation_id {
        Some(a) => format!(", a[id={a}]"),
        None => String::new(),
    };
    let unassigned = match &c.unassigned {
        Some(u) if c.state == ShardState::Unassigned => format!(
            ", unassigned_info[[reason={}], at[{}], delayed={}, allocation_status[{}]]",
            u.reason,
            super::state::iso_millis(u.at_millis),
            u.delayed,
            u.allocation_status
        ),
        _ => String::new(),
    };
    format!(
        "[[{}][{}], {node}{reloc}, {kind}{source}, s[{}]{alloc}{unassigned}]",
        c.index,
        c.shard,
        c.state.as_str()
    )
}

// ---- the deciders --------------------------------------------------------------

/// Can this copy be allocated to this node? The deciders in the order
/// OpenSearch lists them, in its words.
pub fn can_allocate(
    ctx: &Context,
    table: &RoutingTable,
    copy: &ShardRouting,
    node: &DiscoveryNode,
) -> Vec<Verdict> {
    let mut out = Vec::new();
    let is = ctx.index_settings(&copy.index);
    if let Some(v) = data_role_verdict(node) {
        out.push(v);
    }
    out.push(max_retry_verdict(&is, copy));
    if copy.primary {
        out.push(verdict(
            "replica_after_primary_active",
            Decision::Yes,
            "shard is primary and can be allocated",
        ));
    } else {
        let primary_active = table
            .primary(&copy.index, copy.shard)
            .map(|p| matches!(p.state, ShardState::Started | ShardState::Relocating))
            .unwrap_or(false);
        if primary_active {
            out.push(verdict(
                "replica_after_primary_active",
                Decision::Yes,
                "primary shard for this replica is already active",
            ));
        } else {
            out.push(verdict(
                "replica_after_primary_active",
                Decision::No,
                "primary shard for this replica is not yet active",
            ));
        }
    }
    out.push(verdict(
        "cluster_concurrent_recoveries",
        Decision::Yes,
        "undefined cluster concurrent recoveries",
    ));
    out.push(enable_verdict(ctx, &is, copy));
    if copy.primary {
        out.push(verdict(
            "node_version",
            Decision::Yes,
            "the primary shard is new or already existed on the node",
        ));
    } else {
        out.push(verdict(
            "node_version",
            Decision::Yes,
            format!("can allocate replica shard to a node with version [{VERSION}] since this is equal-or-newer than the primary version [{VERSION}]"),
        ));
    }
    out.push(verdict("snapshot_in_progress", Decision::Yes, "the shard is not being snapshotted"));
    out.push(verdict(
        "restore_in_progress",
        Decision::Yes,
        "ignored as shard is not being recovered from a snapshot",
    ));
    // filter: cluster and index include/exclude/require
    let mut filter_ok = true;
    if let Some((kind, f)) = ctx.cluster.filters.check(node) {
        filter_ok = false;
        out.push(verdict(
            "filter",
            Decision::No,
            format!("node does not match cluster setting [cluster.routing.allocation.{kind}] filters [{}]", filters_text(f)),
        ));
    }
    if filter_ok && let Some((kind, f)) = is.filters.check(node) {
        filter_ok = false;
        out.push(verdict(
            "filter",
            Decision::No,
            format!(
                "node does not match index setting [index.routing.allocation.{kind}] filters [{}]",
                filters_text(f)
            ),
        ));
    }
    if filter_ok {
        out.push(verdict("filter", Decision::Yes, "node passes include/exclude/require filters"));
    }
    out.push(verdict(
        "search_replica_allocation",
        Decision::Yes,
        format!(
            "node and shard are compatible. node: [{}], is search node: [false], shard: {}",
            node.id.as_str(),
            short_summary(copy)
        ),
    ));
    // same_shard: no two copies of a shard on one node
    let twin = table.shards_of(&copy.index).find(|c| {
        c.shard == copy.shard
            && c.state != ShardState::Unassigned
            && c.node.as_ref() == Some(&node.id)
            && c.allocation_id != copy.allocation_id
    });
    // the copy itself counts too: a move to the node it is already on made a
    // relocation from a node to itself, which no node could ever finish
    let itself = copy.state != ShardState::Unassigned && copy.node.as_ref() == Some(&node.id);
    match twin {
        _ if itself => out.push(verdict(
            "same_shard",
            Decision::No,
            format!(
                "the shard cannot be allocated to the same node on which it already exists {}",
                short_summary(copy)
            ),
        )),
        Some(t) => out.push(verdict(
            "same_shard",
            Decision::No,
            format!("a copy of this shard is already allocated to this node {}", short_summary(t)),
        )),
        None => out.push(verdict(
            "same_shard",
            Decision::Yes,
            "this node does not hold a copy of this shard",
        )),
    }
    if ctx.data_nodes().len() <= 1 {
        out.push(verdict(
            "disk_threshold",
            Decision::Yes,
            "there is only a single data node present",
        ));
    } else {
        out.push(verdict(
            "disk_threshold",
            Decision::Yes,
            "enough disk for shard on node, free: [1gb], shard size: [0b], free after allocating shard: [1gb]",
        ));
    }
    out.push(throttling_verdict(ctx, table, copy, node));
    out.push(shards_limit_verdict(ctx, &is, table, copy, node));
    out.push(awareness_verdict(ctx, table, copy, node));
    out.push(verdict(
        "load_awareness",
        Decision::Yes,
        "overload awareness allocation is not enabled, set cluster setting [cluster.routing.allocation.load_awareness.skew_factor] and cluster setting [cluster.routing.allocation.load_awareness.provisioned_capacity] to enable it",
    ));
    out.push(verdict(
        "target_pool",
        Decision::Yes,
        "Routing pools are compatible. Shard pool: [LOCAL_ONLY], node pool: [LOCAL_ONLY]",
    ));
    out.push(verdict(
        "remote_store_migration",
        Decision::Yes,
        format!(
            "[none migration_direction]: {} shard copy can be allocated to a non-remote node for strict compatibility mode",
            if copy.primary { "primary" } else { "replica" }
        ),
    ));
    out
}

/// A node without the data role holds no copy of anything. OpenSearch leaves
/// such nodes out of the routing nodes altogether, so none of its deciders
/// is ever asked about them; here every node is in the state, and the
/// refusal has to be said.
fn data_role_verdict(node: &DiscoveryNode) -> Option<Verdict> {
    (!node.is_data()).then(|| {
        verdict(
            "data_role",
            Decision::No,
            format!("node [{}] does not have the data role and cannot hold shard data", node.name),
        )
    })
}

/// Recoveries in flight on the node and on the primary's node, against the
/// limits: a new index's copies get `node_initial_*_recoveries`, the rest
/// `node_concurrent_*_recoveries`.
fn throttling_verdict(
    ctx: &Context,
    table: &RoutingTable,
    copy: &ShardRouting,
    node: &DiscoveryNode,
) -> Verdict {
    let initial = copy.unassigned.as_ref().map(|u| u.reason == "INDEX_CREATED").unwrap_or(false);
    if copy.primary {
        let ongoing = table
            .on_node(&node.id)
            .filter(|c| {
                c.state == ShardState::Initializing && c.primary && c.relocating_node.is_none()
            })
            .count() as u64;
        if ongoing >= ctx.cluster.node_initial_primaries {
            return verdict(
                "throttling",
                Decision::Throttle,
                format!(
                    "reached the limit of ongoing initial primary recoveries [{ongoing}], cluster setting [cluster.routing.allocation.node_initial_primaries_recoveries={}]",
                    ctx.cluster.node_initial_primaries
                ),
            );
        }
        return verdict(
            "throttling",
            Decision::Yes,
            format!("below primary recovery limit of [{}]", ctx.cluster.node_initial_primaries),
        );
    }
    let (in_limit, out_limit) = if initial {
        (ctx.cluster.node_initial_replicas, ctx.cluster.node_initial_replicas)
    } else {
        (ctx.cluster.node_concurrent_incoming, ctx.cluster.node_concurrent_outgoing)
    };
    let incoming =
        table.on_node(&node.id).filter(|c| c.state == ShardState::Initializing).count() as u64;
    if incoming >= in_limit {
        return verdict(
            "throttling",
            Decision::Throttle,
            format!(
                "reached the limit of incoming shard recoveries [{incoming}], cluster setting [cluster.routing.allocation.node_concurrent_incoming_recoveries={in_limit}] (can also be set via [cluster.routing.allocation.node_concurrent_recoveries])"
            ),
        );
    }
    let source = table.primary(&copy.index, copy.shard).and_then(|p| p.node.clone());
    let outgoing = match &source {
        Some(src) => table
            .all()
            .filter(|c| {
                c.state == ShardState::Initializing
                    && !c.primary
                    && table.primary(&c.index, c.shard).and_then(|p| p.node.clone()).as_ref()
                        == Some(src)
            })
            .count() as u64,
        None => 0,
    };
    if let Some(src) = &source
        && outgoing >= out_limit
    {
        return verdict(
            "throttling",
            Decision::Throttle,
            format!(
                "reached the limit of outgoing shard recoveries [{outgoing}] on the node [{}] which holds the primary, cluster setting [cluster.routing.allocation.node_concurrent_outgoing_recoveries={out_limit}] (can also be set via [cluster.routing.allocation.node_concurrent_recoveries])",
                src.as_str()
            ),
        );
    }
    verdict(
        "throttling",
        Decision::Yes,
        format!(
            "below shard recovery limit of outgoing: [{outgoing} < {out_limit}] incoming: [{incoming} < {in_limit}]"
        ),
    )
}

fn enable_verdict(ctx: &Context, is: &IndexSettings, copy: &ShardRouting) -> Verdict {
    let (enable, from) = match is.enable {
        Some(e) => (e, "index setting [index.routing.allocation.enable"),
        None => (ctx.cluster.enable, "cluster setting [cluster.routing.allocation.enable"),
    };
    let new_primary = copy.primary
        && copy.unassigned.as_ref().map(|u| u.reason == "INDEX_CREATED").unwrap_or(false);
    let ok = match enable {
        Enable::All => true,
        Enable::None => false,
        Enable::Primaries => copy.primary,
        Enable::NewPrimaries => new_primary,
    };
    if ok {
        verdict("enable", Decision::Yes, "all allocations are allowed")
    } else {
        let what = match enable {
            Enable::None => "all allocations are forbidden".to_string(),
            Enable::Primaries => "replica allocations are forbidden".to_string(),
            Enable::NewPrimaries => "non-new primary allocations are forbidden".to_string(),
            Enable::All => unreachable!(),
        };
        verdict("enable", Decision::No, format!("{what} due to {from}={}]", enable.as_str()))
    }
}

fn shards_limit_verdict(
    ctx: &Context,
    is: &IndexSettings,
    table: &RoutingTable,
    copy: &ShardRouting,
    node: &DiscoveryNode,
) -> Verdict {
    let on_node = table.on_node(&node.id).filter(|c| c.allocation_id != copy.allocation_id);
    let (index_count, total) = on_node
        .fold((0i64, 0i64), |(i, t), c| (if c.index == copy.index { i + 1 } else { i }, t + 1));
    if is.total_shards_per_node >= 0 && index_count >= is.total_shards_per_node {
        return verdict(
            "shards_limit",
            Decision::No,
            format!(
                "too many shards [{index_count}] allocated to this node for index [{}], index setting [index.routing.allocation.total_shards_per_node={}]",
                copy.index, is.total_shards_per_node
            ),
        );
    }
    if ctx.cluster.total_shards_per_node >= 0 && total >= ctx.cluster.total_shards_per_node {
        return verdict(
            "shards_limit",
            Decision::No,
            format!(
                "too many shards [{total}] allocated to this node, cluster setting [cluster.routing.allocation.total_shards_per_node={}]",
                ctx.cluster.total_shards_per_node
            ),
        );
    }
    if is.total_shards_per_node < 0 && ctx.cluster.total_shards_per_node < 0 {
        return verdict(
            "shards_limit",
            Decision::Yes,
            "total shard limits are disabled: [index: -1, index primary: -1, cluster: -1, cluster primary: -1] <= 0",
        );
    }
    verdict(
        "shards_limit",
        Decision::Yes,
        format!(
            "the shard count [{index_count}] for this node is under the index limit [{}] and cluster level node limit [{}]",
            is.total_shards_per_node, ctx.cluster.total_shards_per_node
        ),
    )
}

fn awareness_verdict(
    ctx: &Context,
    table: &RoutingTable,
    copy: &ShardRouting,
    node: &DiscoveryNode,
) -> Verdict {
    if ctx.cluster.awareness_attributes.is_empty() {
        return verdict(
            "awareness",
            Decision::Yes,
            "allocation awareness is not enabled, set cluster setting [cluster.routing.allocation.awareness.attributes] to enable it",
        );
    }
    let copies: Vec<&ShardRouting> =
        table.shards_of(&copy.index).filter(|c| c.shard == copy.shard).collect();
    let total = copies.len();
    for attr in &ctx.cluster.awareness_attributes {
        let Some(mine) = node.attributes.get(attr) else {
            return verdict(
                "awareness",
                Decision::No,
                format!(
                    "node does not contain the awareness attribute [{attr}]; required attributes cluster setting [cluster.routing.allocation.awareness.attributes={}]",
                    ctx.cluster.awareness_attributes.join(",")
                ),
            );
        };
        let mut values: BTreeSet<String> =
            ctx.data_nodes().into_iter().filter_map(|n| n.attributes.get(attr).cloned()).collect();
        let forced = ctx.cluster.awareness_forced.get(attr);
        if let Some(f) = forced {
            values.extend(f.iter().cloned());
        }
        let n_values = values.len().max(1);
        // copies already on nodes with this value, this copy excluded
        let same: usize = copies
            .iter()
            .filter(|c| c.allocation_id != copy.allocation_id && c.state != ShardState::Unassigned)
            .filter(|c| {
                let target = c
                    .relocating_node
                    .as_ref()
                    .filter(|_| c.state == ShardState::Relocating)
                    .or(c.node.as_ref());
                target.and_then(|id| ctx.nodes.get(id)).and_then(|n| n.attributes.get(attr))
                    == Some(mine)
            })
            .count();
        let max_per_value = total.div_ceil(n_values);
        if same + 1 > max_per_value {
            let listed: Vec<&str> = values.iter().map(|s| s.as_str()).collect();
            let forced_text = match forced {
                Some(f) => format!("forced awareness values [{}]", f.join(", ")),
                None => "no forced awareness".to_string(),
            };
            return verdict(
                "awareness",
                Decision::No,
                format!(
                    "there are [{total}] copies of this shard and [{n_values}] values for attribute [{attr}] ([{}] from nodes in the cluster and {forced_text}) so there may be at most [{max_per_value}] copies of this shard allocated to nodes with each value, but (including this copy) there would be [{}] copies allocated to nodes with [node.attr.{attr}: {mine}]",
                    listed.join(", "),
                    same + 1
                ),
            );
        }
    }
    verdict("awareness", Decision::Yes, "node meets all awareness attribute requirements")
}

fn max_retry_verdict(is: &IndexSettings, copy: &ShardRouting) -> Verdict {
    let failed = copy.unassigned.as_ref().map(|u| u.failed_allocations).unwrap_or(0);
    if failed >= is.max_retries {
        return verdict(
            "max_retry",
            Decision::No,
            format!(
                "shard has exceeded the maximum number of retries [{}] on failed allocation attempts - manually call [/_cluster/reroute?retry_failed=true] to retry, [{}]",
                is.max_retries,
                copy.unassigned.as_ref().map(|u| format!("unassigned_info[[reason={}], at[{}], failed_attempts[{}], delayed={}, allocation_status[{}]]", u.reason, super::state::iso_millis(u.at_millis), u.failed_allocations, u.delayed, u.allocation_status)).unwrap_or_default()
            ),
        );
    }
    if failed == 0 {
        verdict("max_retry", Decision::Yes, "shard has no previous failures")
    } else {
        verdict(
            "max_retry",
            Decision::Yes,
            format!(
                "shard has failed allocating [{failed}] times but [{}] retries are allowed",
                is.max_retries
            ),
        )
    }
}

/// May a started copy be moved at all? The deciders in OpenSearch's order.
pub fn can_rebalance(ctx: &Context, table: &RoutingTable, copy: &ShardRouting) -> Vec<Verdict> {
    let mut out = Vec::new();
    let is = ctx.index_settings(&copy.index);
    // rebalance_only_when_active: this shard's copies all active
    let all_active = table
        .shards_of(&copy.index)
        .filter(|c| c.shard == copy.shard)
        .all(|c| matches!(c.state, ShardState::Started | ShardState::Relocating));
    if all_active {
        out.push(verdict(
            "rebalance_only_when_active",
            Decision::Yes,
            "rebalancing is allowed as all replicas are active in the cluster",
        ));
    } else {
        out.push(verdict(
            "rebalance_only_when_active",
            Decision::No,
            "rebalancing is not allowed until all replicas in the cluster are active",
        ));
    }
    out.push(cluster_rebalance_verdict(ctx, table));
    let relocating = table.all().filter(|c| c.state == ShardState::Relocating).count() as u64;
    if relocating >= ctx.cluster.cluster_concurrent_rebalance {
        out.push(verdict(
            "concurrent_rebalance",
            Decision::Throttle,
            format!("reached the limit of concurrently rebalancing shards [{relocating}], cluster setting [cluster.routing.allocation.cluster_concurrent_rebalance={}]", ctx.cluster.cluster_concurrent_rebalance),
        ));
    } else {
        out.push(verdict(
            "concurrent_rebalance",
            Decision::Yes,
            format!("below threshold [{}] for concurrent rebalances, current rebalance shard count [{relocating}]", ctx.cluster.cluster_concurrent_rebalance),
        ));
    }
    let (enable, from) = match is.rebalance_enable {
        Some(e) => (e, "index setting [index.routing.rebalance.enable"),
        None => (ctx.cluster.rebalance_enable, "cluster setting [cluster.routing.rebalance.enable"),
    };
    let ok = match enable {
        RebalanceEnable::All => true,
        RebalanceEnable::None => false,
        RebalanceEnable::Primaries => copy.primary,
        RebalanceEnable::Replicas => !copy.primary,
    };
    if ok {
        out.push(verdict("enable", Decision::Yes, "all rebalancing is allowed"));
    } else {
        let what = match enable {
            RebalanceEnable::None => "all rebalancing is forbidden",
            RebalanceEnable::Primaries => "replica rebalancing is forbidden",
            RebalanceEnable::Replicas => "primary rebalancing is forbidden",
            RebalanceEnable::All => unreachable!(),
        };
        out.push(verdict(
            "enable",
            Decision::No,
            format!("{what} due to {from}={}]", enable.as_str()),
        ));
    }
    out.push(verdict("snapshot_in_progress", Decision::Yes, "no snapshots are currently running"));
    out
}

fn cluster_rebalance_verdict(ctx: &Context, table: &RoutingTable) -> Verdict {
    let unassigned = table.all().filter(|c| c.state == ShardState::Unassigned).count();
    let primaries_inactive = table
        .all()
        .any(|c| c.primary && !matches!(c.state, ShardState::Started | ShardState::Relocating));
    let any_inactive =
        table.all().any(|c| !matches!(c.state, ShardState::Started | ShardState::Relocating));
    let setting = ctx.cluster.allow_rebalance.as_str();
    match ctx.cluster.allow_rebalance {
        AllowRebalance::Always => {
            verdict("cluster_rebalance", Decision::Yes, "all shards are active")
        }
        AllowRebalance::IndicesPrimariesActive => {
            if primaries_inactive {
                verdict(
                    "cluster_rebalance",
                    Decision::No,
                    format!(
                        "the cluster has inactive primary shards and cluster setting [cluster.routing.allocation.allow_rebalance] is set to [{setting}]"
                    ),
                )
            } else {
                verdict("cluster_rebalance", Decision::Yes, "all primary shards are active")
            }
        }
        AllowRebalance::IndicesAllActive => {
            if unassigned > 0 {
                verdict(
                    "cluster_rebalance",
                    Decision::No,
                    format!(
                        "the cluster has unassigned shards and cluster setting [cluster.routing.allocation.allow_rebalance] is set to [{setting}]"
                    ),
                )
            } else if any_inactive {
                verdict(
                    "cluster_rebalance",
                    Decision::No,
                    format!(
                        "the cluster has inactive shards and cluster setting [cluster.routing.allocation.allow_rebalance] is set to [{setting}]"
                    ),
                )
            } else {
                verdict("cluster_rebalance", Decision::Yes, "all shards are active")
            }
        }
    }
}

/// May a started copy stay where it is?
pub fn can_remain(
    ctx: &Context,
    table: &RoutingTable,
    copy: &ShardRouting,
    node: &DiscoveryNode,
) -> Vec<Verdict> {
    let mut out = Vec::new();
    let is = ctx.index_settings(&copy.index);
    if let Some(v) = data_role_verdict(node) {
        out.push(v);
    }
    let mut filter_ok = true;
    if let Some((kind, f)) = ctx.cluster.filters.check(node) {
        filter_ok = false;
        out.push(verdict("filter", Decision::No, format!("node does not match cluster setting [cluster.routing.allocation.{kind}] filters [{}]", filters_text(f))));
    }
    if filter_ok && let Some((kind, f)) = is.filters.check(node) {
        filter_ok = false;
        out.push(verdict(
            "filter",
            Decision::No,
            format!(
                "node does not match index setting [index.routing.allocation.{kind}] filters [{}]",
                filters_text(f)
            ),
        ));
    }
    if filter_ok {
        out.push(verdict("filter", Decision::Yes, "node passes include/exclude/require filters"));
    }
    // shards_limit counts the other copies on the node
    let mut sl = shards_limit_verdict(ctx, &is, table, copy, node);
    if sl.decision == Decision::No {
        sl.decision = Decision::No;
    }
    out.push(sl);
    out.push(awareness_verdict(ctx, table, copy, node));
    out.push(verdict("disk_threshold", Decision::Yes, "enough disk for shard on node, free: [1gb], shard size: [0b], free after allocating shard: [1gb]"));
    out
}

// ---- the balancer ---------------------------------------------------------------

/// The balancer's weight of a node for an index: how far above average
/// it is in shards overall and in shards of that index.
pub fn weight(ctx: &Context, table: &RoutingTable, index: &str, node: &NodeId) -> f64 {
    let data = ctx.data_nodes();
    let n = data.len().max(1) as f64;
    // an index is held whole here (ADR 0003), so the weight counts copies of
    // indices rather than shards: counting shards would make a five-shard
    // index look five times the load of a one-shard one on the same node, and
    // the balancer would shuffle whole indices back and forth chasing it
    let counts = |id: &NodeId| -> (f64, f64) {
        let mut seen: std::collections::BTreeSet<(&str, bool)> = Default::default();
        let mut of_index = 0.0;
        for c in table.on_node(id) {
            if c.state == ShardState::Unassigned {
                continue;
            }
            if seen.insert((c.index.as_str(), c.primary)) && c.index == index {
                of_index += 1.0;
            }
        }
        (seen.len() as f64, of_index)
    };
    let total_all: f64 = data.iter().map(|d| counts(&d.id).0).sum();
    let total_index: f64 = data.iter().map(|d| counts(&d.id).1).sum();
    let (all, of_index) = counts(node);
    let theta0 =
        ctx.cluster.balance_shard / (ctx.cluster.balance_shard + ctx.cluster.balance_index);
    let theta1 =
        ctx.cluster.balance_index / (ctx.cluster.balance_shard + ctx.cluster.balance_index);
    theta0 * (all - total_all / n) + theta1 * (of_index - total_index / n)
}

/// Nodes ranked by weight for an index, lightest first, ties by id.
fn ranked<'a>(
    ctx: &'a Context,
    table: &RoutingTable,
    index: &str,
) -> Vec<(&'a DiscoveryNode, f64)> {
    let mut v: Vec<(&DiscoveryNode, f64)> =
        ctx.data_nodes().into_iter().map(|d| (d, weight(ctx, table, index, &d.id))).collect();
    v.sort_by(|a, b| {
        a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.id.cmp(&b.0.id))
    });
    v
}

fn new_allocation_id() -> String {
    NodeId::random().0
}

/// What one pass of `reroute` did, for the notes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// copies that are no longer in sync: their node left, so the writes
    /// taken since never reached them
    pub retired: Vec<(String, u32, String)>,
    pub notes: Vec<String>,
    pub assigned: Vec<(String, u32, bool, NodeId)>,
    pub unassigned: Vec<(String, u32, bool, String)>,
    pub promoted: Vec<(String, u32, NodeId)>,
    pub relocating: Vec<(String, u32, bool, NodeId, NodeId)>,
    /// the soonest a delayed replica may be allocated, if any waits
    pub next_delay_at: Option<Millis>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.assigned.is_empty()
            && self.unassigned.is_empty()
            && self.promoted.is_empty()
            && self.relocating.is_empty()
    }
}

pub(crate) fn unassigned_info(
    reason: &str,
    at: Millis,
    status: &str,
    failed: u64,
) -> UnassignedInfo {
    UnassignedInfo {
        reason: reason.into(),
        at_millis: at,
        delayed: false,
        allocation_status: status.into(),
        failed_allocations: failed,
        details: None,
    }
}

/// The routing table as it should be, from the table as it was.
pub fn reroute(ctx: &Context, table: &RoutingTable) -> (RoutingTable, Changes) {
    let mut t = table.clone();
    let mut changes = Changes::default();

    // 1. indices that came and went; shards that came and went
    let names: Vec<String> = t.indices.keys().cloned().collect();
    for name in names {
        if !ctx.indices.contains_key(&name) {
            t.indices.remove(&name);
        }
    }
    for (name, m) in ctx.indices {
        let shards = t.indices.entry(name.clone()).or_default();
        for shard in 0..m.number_of_shards {
            let fresh = !shards.contains_key(&shard);
            let copies = shards.entry(shard).or_default();
            if copies.iter().all(|c| !c.primary) {
                // a new primary: at home if the index has one and the deciders
                // let it be there, else placed like any unassigned copy. A home
                // they refuse -- a node without the data role, one a filter
                // excludes -- keeps the primary only when documents are already
                // written there, and the move below takes it away at once.
                let fresh_primary = ShardRouting {
                    index: name.clone(),
                    shard,
                    primary: true,
                    state: ShardState::Unassigned,
                    node: None,
                    relocating_node: None,
                    allocation_id: None,
                    unassigned: Some(unassigned_info(
                        "INDEX_CREATED",
                        m.creation_date.max(ctx.now),
                        "no_attempt",
                        0,
                    )),
                };
                let at_home =
                    ctx.primary_home.get(name).and_then(|home| ctx.nodes.get(home)).filter(
                        |node| {
                            ctx.home_holds_documents.contains(name)
                                || overall(&can_allocate(ctx, table, &fresh_primary, node))
                                    != Decision::No
                        },
                    );
                match at_home.map(|n| n.id.clone()) {
                    Some(home) => copies.insert(
                        0,
                        ShardRouting {
                            index: name.clone(),
                            shard,
                            primary: true,
                            state: ShardState::Started,
                            node: Some(home.clone()),
                            relocating_node: None,
                            allocation_id: Some(new_allocation_id()),
                            unassigned: None,
                        },
                    ),
                    None => copies.insert(0, fresh_primary),
                }
            }
            // the replica count follows the setting
            let have = copies.iter().filter(|c| !c.primary).count() as u32;
            for _ in have..m.number_of_replicas {
                copies.push(ShardRouting {
                    index: name.clone(),
                    shard,
                    primary: false,
                    state: ShardState::Unassigned,
                    node: None,
                    relocating_node: None,
                    allocation_id: None,
                    unassigned: Some(unassigned_info(
                        if fresh { "INDEX_CREATED" } else { "REPLICA_ADDED" },
                        if fresh { m.creation_date.max(ctx.now) } else { ctx.now },
                        "no_attempt",
                        0,
                    )),
                });
            }
            if have > m.number_of_replicas {
                // drop unassigned replicas first, then the rest from the end
                let mut extra = (have - m.number_of_replicas) as usize;
                let mut i = copies.len();
                while extra > 0 && i > 0 {
                    i -= 1;
                    if !copies[i].primary && copies[i].state == ShardState::Unassigned {
                        copies.remove(i);
                        extra -= 1;
                    }
                }
                while extra > 0 {
                    if let Some(pos) = copies.iter().rposition(|c| !c.primary) {
                        copies.remove(pos);
                    }
                    extra -= 1;
                }
            }
        }
        shards.retain(|s, _| *s < m.number_of_shards);
    }

    // 2. copies on nodes that left
    for (name, shards) in t.indices.iter_mut() {
        let is = ctx.index_settings(name);
        for (shard, copies) in shards.iter_mut() {
            // a relocation whose target left goes back to just the source
            for c in copies.iter_mut() {
                if c.state == ShardState::Relocating
                    && c.relocating_node
                        .as_ref()
                        .map(|r| !ctx.nodes.contains_key(r))
                        .unwrap_or(false)
                {
                    c.state = ShardState::Started;
                    c.relocating_node = None;
                }
            }
            copies.retain(|c| {
                !(c.state == ShardState::Initializing
                    && c.relocating_node.is_some()
                    && (c.node.as_ref().map(|n| !ctx.nodes.contains_key(n)).unwrap_or(true)
                        || c.relocating_node
                            .as_ref()
                            .map(|r| !ctx.nodes.contains_key(r))
                            .unwrap_or(true)))
            });
            let gone: Vec<usize> = copies
                .iter()
                .enumerate()
                .filter(|(_, c)| {
                    c.state != ShardState::Unassigned
                        && c.node.as_ref().map(|n| !ctx.nodes.contains_key(n)).unwrap_or(false)
                })
                .map(|(i, _)| i)
                .collect();
            // the primary that was on the node that left, if there was one:
            // its place in the in-sync set is kept while nothing has taken
            // over from it, and given up the moment something does
            let mut deposed: Option<String> = None;
            for i in gone {
                let c = &mut copies[i];
                let was_primary = c.primary;
                // a copy whose node is gone is no longer in sync: the writes
                // the primary takes while it is away never reach it, and when
                // the node comes back the copy has to be filled again. The
                // primary's own copy keeps its place in the set, since it is
                // the one holding what everybody else is missing.
                if !was_primary && let Some(a) = &c.allocation_id {
                    changes.retired.push((name.clone(), *shard, a.clone()));
                }
                if was_primary {
                    deposed = c.allocation_id.clone();
                }
                c.state = ShardState::Unassigned;
                c.node = None;
                c.relocating_node = None;
                c.allocation_id = None;
                let mut info = unassigned_info("NODE_LEFT", ctx.now, "no_attempt", 0);
                info.delayed = !was_primary && is.delayed_timeout > 0;
                c.unassigned = Some(info);
                changes.unassigned.push((name.clone(), *shard, was_primary, "NODE_LEFT".into()));
            }
            // a primary lost: an active replica that is in sync takes over.
            // A copy that missed an acknowledged write was retired from the
            // set when it missed it, and promoting it would throw that write
            // away: the shard stays without a primary until a copy that has
            // everything comes back, which is what OpenSearch does too.
            // an empty set is not "any copy will do": it is every copy having
            // missed something. No set at all is no information, and there the
            // shard is better up than down.
            let in_sync: Option<Vec<String>> =
                ctx.indices.get(name).and_then(|m| m.in_sync_allocations.get(shard).cloned());
            if let Some(pi) =
                copies.iter().position(|c| c.primary && c.state == ShardState::Unassigned)
            {
                if let Some(ri) = copies.iter().position(|c| {
                    !c.primary
                        && matches!(c.state, ShardState::Started | ShardState::Relocating)
                        && c.allocation_id
                            .as_ref()
                            .map(|a| match &in_sync {
                                None => true,
                                Some(set) => set.iter().any(|i| i == a),
                            })
                            .unwrap_or(false)
                }) {
                    let node = copies[ri].node.clone().unwrap();
                    copies[ri].primary = true;
                    copies[ri].relocating_node = None;
                    copies[ri].state = ShardState::Started;
                    copies[pi].primary = false;
                    copies.swap(0, ri);
                    // the copy that was the primary is behind the one that
                    // has taken over: every write acknowledged from here on
                    // is one it does not have. Leaving it in the set let it
                    // be handed the primary back when it returned, and the
                    // writes taken in between went with it.
                    if let Some(a) = deposed.take() {
                        changes.retired.push((name.clone(), *shard, a));
                    }
                    changes.promoted.push((name.clone(), *shard, node));
                } else if copies[pi]
                    .unassigned
                    .as_ref()
                    .map(|u| u.reason == "NODE_LEFT")
                    .unwrap_or(false)
                {
                    if let Some(u) = copies[pi].unassigned.as_mut() {
                        u.allocation_status = "no_valid_shard_copy".into();
                    }
                    if copies.iter().any(|c| {
                        !c.primary
                            && matches!(c.state, ShardState::Started | ShardState::Relocating)
                    }) {
                        changes.notes.push(format!(
                            "no copy of [{name}][{shard}] may take the primary: the copies here are not in sync (in_sync={in_sync:?})"
                        ));
                    }
                }
            }
        }
    }

    // 3. allocate what is unassigned: primaries first, then replicas
    for pass in [true, false] {
        let mut pending: Vec<(String, u32, usize)> = Vec::new();
        for (name, shards) in &t.indices {
            for (shard, copies) in shards {
                for (i, c) in copies.iter().enumerate() {
                    if c.primary == pass && c.state == ShardState::Unassigned {
                        pending.push((name.clone(), *shard, i));
                    }
                }
            }
        }
        for (name, shard, i) in pending {
            let copy = t.indices[&name][&shard][i].clone();
            let is = ctx.index_settings(&name);
            // a primary whose data is lost is not made again out of nothing:
            // it waits, and the index is red, until `allocate_empty_primary`
            // with `accept_data_loss` asks for exactly that
            if copy.primary {
                let reason = copy.unassigned.as_ref().map(|u| u.reason.clone()).unwrap_or_default();
                if reason != "INDEX_CREATED" && reason != "FORCED_EMPTY_PRIMARY" {
                    // a node that still holds this index's data (the same uuid)
                    // gets the primary back: an existing-store recovery
                    let uuid = ctx.indices.get(&name).map(|m| m.uuid.clone()).unwrap_or_default();
                    // ... and only a copy that was in sync: one that missed an
                    // acknowledged write cannot become the primary on its own
                    let in_sync: Vec<String> = ctx
                        .indices
                        .get(&name)
                        .and_then(|m| m.in_sync_allocations.get(&shard).cloned())
                        .unwrap_or_default();
                    let keeper = ctx
                        .data_nodes()
                        .into_iter()
                        .find(|n| {
                            ctx.held
                                .get(&n.id)
                                .map(|h| {
                                    h.iter().any(|(hn, hu, ha)| {
                                        *hn == name
                                            && *hu == uuid
                                            && in_sync.iter().any(|i| i == ha)
                                    })
                                })
                                .unwrap_or(false)
                                && overall(&can_allocate(ctx, &t, &copy, n)) == Decision::Yes
                        })
                        .map(|n| n.id.clone());
                    let copies = t.indices.get_mut(&name).unwrap().get_mut(&shard).unwrap();
                    match keeper {
                        Some(node) => {
                            let c = &mut copies[i];
                            c.state = ShardState::Initializing;
                            c.node = Some(node.clone());
                            c.relocating_node = None;
                            c.allocation_id = Some(new_allocation_id());
                            if let Some(u) = c.unassigned.as_mut() {
                                u.delayed = false;
                            }
                            changes.notes.push(format!("primary of [{name}] placed back on {node}, which holds its in-sync copy"));
                            changes.assigned.push((name.clone(), shard, true, node));
                        }
                        None => {
                            if let Some(u) = copies[i].unassigned.as_mut() {
                                u.allocation_status = "no_valid_shard_copy".into();
                            }
                            changes.notes.push(format!(
                                "no in-sync copy of [{name}] to place the primary on: in_sync={in_sync:?} held={:?}",
                                ctx.held
                            ));
                        }
                    }
                    continue;
                }
            }
            // a replica whose node left waits for it to come back
            if let Some(u) = &copy.unassigned
                && u.delayed
            {
                let ready_at = u.at_millis + is.delayed_timeout;
                if ctx.now < ready_at {
                    changes.next_delay_at =
                        Some(changes.next_delay_at.map_or(ready_at, |d| d.min(ready_at)));
                    continue;
                }
            }
            let mut best: Option<(&DiscoveryNode, Decision)> = None;
            let mut throttled = false;
            for (node, _w) in ranked(ctx, &t, &name) {
                let d = overall(&can_allocate(ctx, &t, &copy, node));
                match d {
                    Decision::Yes => {
                        best = Some((node, d));
                        break;
                    }
                    Decision::Throttle => throttled = true,
                    Decision::No => {}
                }
            }
            let copies = t.indices.get_mut(&name).unwrap().get_mut(&shard).unwrap();
            match best {
                Some((node, _)) => {
                    let c = &mut copies[i];
                    c.state = ShardState::Initializing;
                    c.node = Some(node.id.clone());
                    c.relocating_node = None;
                    c.allocation_id = Some(new_allocation_id());
                    if let Some(u) = c.unassigned.as_mut() {
                        u.delayed = false;
                    }
                    changes.assigned.push((name.clone(), shard, copy.primary, node.id.clone()));
                }
                None => {
                    if let Some(u) = copies[i].unassigned.as_mut() {
                        u.delayed = false;
                        // a replica every node refused reads `no_attempt`, as
                        // the plugin leaves it; a primary reads why
                        u.allocation_status = if throttled {
                            "throttled".into()
                        } else if ctx.data_nodes().is_empty() || !copy.primary {
                            "no_attempt".into()
                        } else if u.allocation_status == "no_valid_shard_copy" {
                            "no_valid_shard_copy".into()
                        } else {
                            "deciders_no".into()
                        };
                    }
                }
            }
        }
    }

    // 4. move what may not stay where it is: a copy on a node without the
    // data role, or on a node a filter now excludes, goes to the lightest
    // node the deciders allow, as OpenSearch's `moveShards` does before it
    // balances. Only those two reasons move a copy: the others `can_remain`
    // reports never moved one here, and a copy is a copy of the whole index,
    // so the first shard is what moves and the rest follow it below.
    let names: Vec<String> = t.indices.keys().cloned().collect();
    for name in names {
        let stuck: Vec<ShardRouting> = t
            .shards_of(&name)
            .filter(|c| c.shard == 0 && c.state == ShardState::Started)
            .filter(|c| {
                c.node.as_ref().and_then(|n| ctx.nodes.get(n)).is_some_and(|node| {
                    can_remain(ctx, &t, c, node).iter().any(|v| {
                        v.decision == Decision::No && matches!(v.decider, "data_role" | "filter")
                    })
                })
            })
            .cloned()
            .collect();
        for copy in stuck {
            let Some(from) = copy.node.clone() else { continue };
            let target = ranked(ctx, &t, &name)
                .into_iter()
                .map(|(n, _)| n)
                .find(|n| {
                    n.id != from && overall(&can_allocate(ctx, &t, &copy, n)) == Decision::Yes
                })
                .map(|n| n.id.clone());
            let Some(to) = target else {
                changes.notes.push(format!(
                    "[{name}][{}] may not remain on {from} and no node will take it",
                    copy.shard
                ));
                continue;
            };
            let copies = t.indices.get_mut(&name).unwrap().get_mut(&copy.shard).unwrap();
            let Some(pos) = copies.iter().position(|c| c.allocation_id == copy.allocation_id)
            else {
                continue;
            };
            copies[pos].state = ShardState::Relocating;
            copies[pos].relocating_node = Some(to.clone());
            copies.push(ShardRouting {
                index: name.clone(),
                shard: copy.shard,
                primary: copy.primary,
                state: ShardState::Initializing,
                node: Some(to.clone()),
                relocating_node: Some(from.clone()),
                allocation_id: Some(new_allocation_id()),
                unassigned: None,
            });
            changes.relocating.push((name.clone(), copy.shard, copy.primary, from, to));
        }
    }

    // 5. rebalance: a copy of an index from a heavy node to the lightest,
    // heaviest source first, while the difference is above the threshold;
    // one move per pass
    let names: Vec<String> = t.indices.keys().cloned().collect();
    'outer: for name in names {
        let r = ranked(ctx, &t, &name);
        if r.len() < 2 {
            continue;
        }
        let (light, lw) = (r[0].0, r[0].1);
        for (heavy, hw) in r.iter().rev() {
            if heavy.id == light.id || hw - lw <= ctx.cluster.balance_threshold {
                break;
            }
            let candidates: Vec<ShardRouting> = t
                .on_node(&heavy.id)
                .filter(|c| c.index == name && c.state == ShardState::Started)
                .cloned()
                .collect();
            for copy in candidates {
                if overall(&can_rebalance(ctx, &t, &copy)) != Decision::Yes {
                    continue;
                }
                if overall(&can_allocate(ctx, &t, &copy, light)) != Decision::Yes {
                    continue;
                }
                // the move must make things better, not merely different
                let after_light = weight(ctx, &t, &name, &light.id) + 1.0;
                let after_heavy = weight(ctx, &t, &name, &heavy.id) - 1.0;
                if (after_heavy - after_light).abs() >= hw - lw {
                    continue;
                }
                let copies = t.indices.get_mut(&name).unwrap().get_mut(&copy.shard).unwrap();
                let pos =
                    copies.iter().position(|c| c.allocation_id == copy.allocation_id).unwrap();
                copies[pos].state = ShardState::Relocating;
                copies[pos].relocating_node = Some(light.id.clone());
                copies.push(ShardRouting {
                    index: name.clone(),
                    shard: copy.shard,
                    primary: copy.primary,
                    state: ShardState::Initializing,
                    node: Some(light.id.clone()),
                    relocating_node: Some(heavy.id.clone()),
                    allocation_id: Some(new_allocation_id()),
                    unassigned: None,
                });
                changes.relocating.push((
                    name.clone(),
                    copy.shard,
                    copy.primary,
                    heavy.id.clone(),
                    light.id.clone(),
                ));
                continue 'outer;
            }
        }
    }

    // 6. every shard of an index sits where its first shard sits.
    //
    // A copy here is a copy of the whole index -- the store holds one index,
    // not a shard of one (ADR 0003) -- so the routing has to say the same:
    // a write routed to shard three has to land on the node that answers for
    // the index, and a copy filled from the primary has to be the primary of
    // every shard, not of one.
    for (name, shards) in t.indices.iter_mut() {
        let Some(first) = shards.get(&0).cloned() else { continue };
        let keys: Vec<u32> = shards.keys().copied().filter(|s| *s != 0).collect();
        for shard in keys {
            let copies = shards.get_mut(&shard).unwrap();
            for (i, c) in copies.iter_mut().enumerate() {
                let Some(model) = first.get(i) else { continue };
                if c.node == model.node && c.state == model.state && c.primary == model.primary {
                    continue;
                }
                let was = c.node.clone();
                c.primary = model.primary;
                c.state = model.state;
                c.node = model.node.clone();
                c.relocating_node = model.relocating_node.clone();
                c.unassigned = model.unassigned.clone();
                if c.allocation_id.is_none() && model.allocation_id.is_some() {
                    c.allocation_id = Some(new_allocation_id());
                }
                if model.node.is_none() {
                    c.allocation_id = None;
                }
                if was != model.node
                    && let Some(n) = model.node.clone()
                {
                    changes.assigned.push((name.clone(), shard, c.primary, n));
                }
            }
            // a shard with fewer copies than the first one gains them
            while copies.len() < first.len() {
                let model = &first[copies.len()];
                copies.push(ShardRouting {
                    index: name.clone(),
                    shard,
                    primary: model.primary,
                    state: model.state,
                    node: model.node.clone(),
                    relocating_node: model.relocating_node.clone(),
                    allocation_id: model.allocation_id.as_ref().map(|_| new_allocation_id()),
                    unassigned: model.unassigned.clone(),
                });
            }
            copies.truncate(first.len());
        }
    }
    (t, changes)
}

/// A data node reports a copy started: the copy becomes STARTED; the
/// source of a relocation goes away.
pub fn shard_started(
    table: &mut RoutingTable,
    index: &str,
    shard: u32,
    allocation_id: &str,
) -> Option<Started> {
    let copies = table.indices.get_mut(index).and_then(|s| s.get_mut(&shard))?;
    let pos = copies.iter().position(|c| c.allocation_id.as_deref() == Some(allocation_id))?;
    if copies[pos].state != ShardState::Initializing {
        return None;
    }
    let source = copies[pos].relocating_node.clone();
    let forced = copies[pos].primary
        && copies[pos]
            .unassigned
            .as_ref()
            .map(|u| u.reason == "FORCED_EMPTY_PRIMARY")
            .unwrap_or(false);
    copies[pos].state = ShardState::Started;
    copies[pos].relocating_node = None;
    copies[pos].unassigned = None;
    let mut retired = None;
    if let Some(src) = source {
        // the relocated copy is here now; the one on the source node ends
        if let Some(sp) = copies
            .iter()
            .position(|c| c.state == ShardState::Relocating && c.node.as_ref() == Some(&src))
        {
            retired = copies[sp].allocation_id.clone();
            copies.remove(sp);
        }
    }
    // primaries first, as OpenSearch lists them
    copies.sort_by_key(|c| !c.primary);
    Some(Started { retired, forced_primary: forced })
}

/// What starting a copy changed besides its state.
#[derive(Clone, Debug, Default)]
pub struct Started {
    /// the allocation id of a relocation's source, which is no longer in sync
    pub retired: Option<String>,
    /// a primary made empty on request: it alone is in sync now
    pub forced_primary: bool,
}

/// A data node reports a copy failed: unassigned again, one more failure on it.
pub fn shard_failed(
    table: &mut RoutingTable,
    index: &str,
    shard: u32,
    allocation_id: &str,
    now: Millis,
    message: &str,
) -> bool {
    let Some(copies) = table.indices.get_mut(index).and_then(|s| s.get_mut(&shard)) else {
        return false;
    };
    let Some(pos) = copies.iter().position(|c| c.allocation_id.as_deref() == Some(allocation_id))
    else {
        return false;
    };
    let c = &mut copies[pos];
    if c.state == ShardState::Initializing && c.relocating_node.is_some() {
        // a failed relocation target: the source stays where it is
        let src = c.relocating_node.clone();
        copies.remove(pos);
        if let Some(sp) =
            copies.iter().position(|c| c.state == ShardState::Relocating && c.node == src)
        {
            copies[sp].state = ShardState::Started;
            copies[sp].relocating_node = None;
        }
        return true;
    }
    let failed = c.unassigned.as_ref().map(|u| u.failed_allocations).unwrap_or(0) + 1;
    c.state = ShardState::Unassigned;
    c.node = None;
    c.relocating_node = None;
    c.allocation_id = None;
    let mut u = unassigned_info("ALLOCATION_FAILED", now, "no_attempt", failed);
    u.details = Some(format!("failed shard on node: {message}"));
    c.unassigned = Some(u);
    true
}

/// A copy that did not take an acknowledged write is no copy of the index
/// any more: it is unassigned, and the allocator fills it again from the
/// primary. Not a failure -- nothing went wrong with the node -- so it does
/// not count against `index.allocation.max_retries`, which is how OpenSearch
/// treats a replica it takes out of the in-sync set.
pub fn shard_stale(
    table: &mut RoutingTable,
    index: &str,
    shard: u32,
    allocation_id: &str,
    now: Millis,
) -> bool {
    let Some(copies) = table.indices.get_mut(index).and_then(|s| s.get_mut(&shard)) else {
        return false;
    };
    let Some(pos) =
        copies.iter().position(|c| c.allocation_id.as_deref() == Some(allocation_id) && !c.primary)
    else {
        return false;
    };
    let c = &mut copies[pos];
    c.state = ShardState::Unassigned;
    c.node = None;
    c.relocating_node = None;
    c.allocation_id = None;
    let mut u = unassigned_info("REALLOCATED_REPLICA", now, "no_attempt", 0);
    u.details = Some("the copy did not take an acknowledged write".into());
    c.unassigned = Some(u);
    true
}

/// `retry_failed=true`: failures forgotten, so the deciders look again.
pub fn retry_failed(table: &mut RoutingTable) -> usize {
    let mut n = 0;
    for c in table.indices.values_mut().flat_map(|s| s.values_mut()).flatten() {
        if let Some(u) = c.unassigned.as_mut()
            && u.failed_allocations > 0
        {
            u.failed_allocations = 0;
            n += 1;
        }
    }
    n
}

// ---- `_cluster/reroute` commands ----------------------------------------------

fn resolve_node<'a>(ctx: &'a Context, name: &str) -> Result<&'a DiscoveryNode, String> {
    ctx.nodes
        .values()
        .find(|n| n.id.as_str() == name || n.name == name || n.transport_address == name)
        .ok_or_else(|| format!("failed to resolve [{name}], no matching nodes"))
}

fn node_text(n: &DiscoveryNode) -> String {
    format!(
        "{{{}}}{{{}}}{{{}}}{{{}}}",
        n.name,
        n.id.as_str(),
        n.ephemeral_id.as_str(),
        n.transport_address
    )
}

fn decisions_text(vs: &[Verdict]) -> String {
    vs.iter()
        .map(|v| format!("[{}({})]", v.decision.label(), v.explanation))
        .collect::<Vec<_>>()
        .join("")
}

/// Apply `_cluster/reroute` commands to a table. With `explain`, a command
/// that is refused becomes an explanation instead of an error.
pub fn apply_commands(
    ctx: &Context,
    table: &RoutingTable,
    commands: &[Value],
    explain: bool,
) -> Result<(RoutingTable, Vec<Value>), String> {
    let mut t = table.clone();
    let mut explanations = Vec::new();
    for cmd in commands {
        let Some((name, args)) = cmd.as_object().and_then(|o| o.iter().next()) else {
            return Err("commands must be objects with one command each".into());
        };
        let index = args.get("index").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let shard = args.get("shard").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let refuse = |explanations: &mut Vec<Value>,
                      msg: String,
                      params: Value|
         -> Result<(), String> {
            if explain {
                explanations.push(json!({
                    "command": name,
                    "parameters": params,
                    "decisions": [{"decider": format!("{name} (allocation command)"), "decision": "NO", "explanation": msg}],
                }));
                Ok(())
            } else {
                Err(msg)
            }
        };
        let accept = |explanations: &mut Vec<Value>, params: Value, vs: &[Verdict]| {
            if explain {
                let mut ds: Vec<Value> = vs
                    .iter()
                    .map(|v| json!({"decider": v.decider, "decision": v.decision.label(), "explanation": v.explanation}))
                    .collect();
                if ds.is_empty() {
                    ds.push(json!({"decider": format!("{name} (allocation command)"), "decision": "YES", "explanation": "explanation not available"}));
                }
                explanations.push(json!({"command": name, "parameters": params, "decisions": ds}));
            }
        };
        match name.as_str() {
            "move" => {
                let from = args.get("from_node").and_then(|v| v.as_str()).unwrap_or("");
                let to = args.get("to_node").and_then(|v| v.as_str()).unwrap_or("");
                let params =
                    json!({"index": index, "shard": shard, "from_node": from, "to_node": to});
                let (from_n, to_n) = match (resolve_node(ctx, from), resolve_node(ctx, to)) {
                    (Ok(f), Ok(t)) => (f, t),
                    (Err(e), _) | (_, Err(e)) => {
                        refuse(&mut explanations, e, params)?;
                        continue;
                    }
                };
                let copy = t
                    .shards_of(&index)
                    .find(|c| c.shard == shard && c.node.as_ref() == Some(&from_n.id))
                    .cloned();
                // The shards of an index move together (below), so a second
                // command for another shard of the same index is asking for a
                // move that is already under way: that is done, not refused.
                // `_cluster/reroute` with one command per shard is how a copy
                // of a several-shard index is taken off a node.
                let under_way = t.shards_of(&index).any(|c| {
                    c.shard == shard
                        && c.relocating_node.as_ref() == Some(&to_n.id)
                        && c.node.as_ref() == Some(&from_n.id)
                });
                if under_way {
                    accept(&mut explanations, params, &[]);
                    continue;
                }
                let Some(copy) = copy else {
                    refuse(
                        &mut explanations,
                        format!(
                            "[move_allocation] can't move [{index}][{shard}], failed to find it on node {}",
                            node_text(from_n)
                        ),
                        params,
                    )?;
                    continue;
                };
                if copy.state != ShardState::Started {
                    refuse(
                        &mut explanations,
                        format!(
                            "[move_allocation] can't move [{index}][{shard}], shard is not started (state = {}]",
                            copy.state.as_str()
                        ),
                        params,
                    )?;
                    continue;
                }
                let vs = can_allocate(ctx, &t, &copy, to_n);
                if overall(&vs) == Decision::No {
                    refuse(
                        &mut explanations,
                        format!(
                            "[move_allocation] can't move [{index}][{shard}], from {}, to {}, since its not allowed, reason: {}",
                            node_text(from_n),
                            node_text(to_n),
                            decisions_text(&vs)
                        ),
                        params,
                    )?;
                    continue;
                }
                // Every shard of the index goes, not the one named alone. A
                // copy here is a copy of the whole index (ADR 0003), and the
                // routing says so: `reroute` puts every shard where the
                // first one is. A move of a shard on its own was answered
                // and then undone by that, so the command was acknowledged
                // and nothing happened.
                let shards: Vec<u32> = t
                    .shards_of(&index)
                    .filter(|c| {
                        c.state == ShardState::Started
                            && c.primary == copy.primary
                            && c.node.as_ref() == Some(&from_n.id)
                    })
                    .map(|c| c.shard)
                    .collect();
                for moving in shards {
                    let copies = t.indices.get_mut(&index).unwrap().get_mut(&moving).unwrap();
                    let Some(pos) = copies.iter().position(|c| {
                        c.state == ShardState::Started
                            && c.primary == copy.primary
                            && c.node.as_ref() == Some(&from_n.id)
                    }) else {
                        continue;
                    };
                    copies[pos].state = ShardState::Relocating;
                    copies[pos].relocating_node = Some(to_n.id.clone());
                    copies.push(ShardRouting {
                        index: index.clone(),
                        shard: moving,
                        primary: copy.primary,
                        state: ShardState::Initializing,
                        node: Some(to_n.id.clone()),
                        relocating_node: Some(from_n.id.clone()),
                        allocation_id: Some(new_allocation_id()),
                        unassigned: None,
                    });
                }
                accept(&mut explanations, params, &vs);
            }
            "allocate_replica" | "allocate_empty_primary" | "allocate_stale_primary" => {
                let node = args.get("node").and_then(|v| v.as_str()).unwrap_or("");
                let primary = name != "allocate_replica";
                let params = json!({"index": index, "shard": shard, "node": node});
                let n = match resolve_node(ctx, node) {
                    Ok(n) => n,
                    Err(e) => {
                        refuse(&mut explanations, e, params)?;
                        continue;
                    }
                };
                if primary
                    && !args.get("accept_data_loss").and_then(|v| v.as_bool()).unwrap_or(false)
                {
                    return Err(format!(
                        "[{name}] allocating an empty primary for [{index}][{shard}] can result in data loss. Please confirm by setting the accept_data_loss parameter to true"
                    ));
                }
                let copy = t
                    .shards_of(&index)
                    .find(|c| {
                        c.shard == shard
                            && c.primary == primary
                            && c.state == ShardState::Unassigned
                    })
                    .cloned();
                let Some(copy) = copy else {
                    let what = if primary { "primary" } else { "replica" };
                    refuse(
                        &mut explanations,
                        format!("[{name}] {what} [{index}][{shard}] is already assigned"),
                        params,
                    )?;
                    continue;
                };
                let vs = can_allocate(ctx, &t, &copy, n);
                if overall(&vs) == Decision::No {
                    refuse(
                        &mut explanations,
                        format!(
                            "[{name}] allocation of [{index}][{shard}] on node {} is not allowed, reason: {}",
                            node_text(n),
                            decisions_text(&vs)
                        ),
                        params,
                    )?;
                    continue;
                }
                let copies = t.indices.get_mut(&index).unwrap().get_mut(&shard).unwrap();
                let pos = copies
                    .iter()
                    .position(|c| c.primary == primary && c.state == ShardState::Unassigned)
                    .unwrap();
                copies[pos].state = ShardState::Initializing;
                copies[pos].node = Some(n.id.clone());
                copies[pos].allocation_id = Some(new_allocation_id());
                if let Some(u) = copies[pos].unassigned.as_mut() {
                    u.delayed = false;
                    if primary {
                        u.reason = "FORCED_EMPTY_PRIMARY".into();
                    }
                }
                accept(&mut explanations, params, &vs);
            }
            "cancel" => {
                let node = args.get("node").and_then(|v| v.as_str()).unwrap_or("");
                let allow_primary =
                    args.get("allow_primary").and_then(|v| v.as_bool()).unwrap_or(false);
                let params = json!({"index": index, "shard": shard, "node": node, "allow_primary": allow_primary});
                let n = match resolve_node(ctx, node) {
                    Ok(n) => n,
                    Err(e) => {
                        refuse(&mut explanations, e, params)?;
                        continue;
                    }
                };
                let copy = t
                    .shards_of(&index)
                    .find(|c| c.shard == shard && c.node.as_ref() == Some(&n.id))
                    .cloned();
                let Some(copy) = copy else {
                    refuse(
                        &mut explanations,
                        format!(
                            "[cancel_allocation] can't cancel [{index}][{shard}], failed to find it on node {}",
                            node_text(n)
                        ),
                        params,
                    )?;
                    continue;
                };
                if copy.primary && !allow_primary {
                    refuse(
                        &mut explanations,
                        format!(
                            "[cancel_allocation] can't cancel [{index}][{shard}] on node {}, shard is primary and initializing its state",
                            node_text(n)
                        ),
                        params,
                    )?;
                    continue;
                }
                let copies = t.indices.get_mut(&index).unwrap().get_mut(&shard).unwrap();
                let pos =
                    copies.iter().position(|c| c.allocation_id == copy.allocation_id).unwrap();
                match copy.state {
                    ShardState::Initializing if copy.relocating_node.is_some() => {
                        // a relocation target: the source stays
                        let src = copy.relocating_node.clone();
                        copies.remove(pos);
                        if let Some(sp) = copies
                            .iter()
                            .position(|c| c.state == ShardState::Relocating && c.node == src)
                        {
                            copies[sp].state = ShardState::Started;
                            copies[sp].relocating_node = None;
                        }
                    }
                    ShardState::Relocating => {
                        // the source of a relocation: the target goes, the source stays
                        let target = copy.relocating_node.clone();
                        copies[pos].state = ShardState::Started;
                        copies[pos].relocating_node = None;
                        if let Some(tp) = copies
                            .iter()
                            .position(|c| c.state == ShardState::Initializing && c.node == target)
                        {
                            copies.remove(tp);
                        }
                    }
                    _ => {
                        let c = &mut copies[pos];
                        c.state = ShardState::Unassigned;
                        c.node = None;
                        c.relocating_node = None;
                        c.allocation_id = None;
                        c.unassigned =
                            Some(unassigned_info("REROUTE_CANCELLED", ctx.now, "no_attempt", 0));
                    }
                }
                accept(&mut explanations, params, &[]);
            }
            other => return Err(format!("[{other}] is not a known allocation command")),
        }
    }
    Ok((t, explanations))
}

// ---- `_cluster/allocation/explain` --------------------------------------------

fn node_json(n: &DiscoveryNode, weight_ranking: usize) -> Value {
    json!({
        "id": n.id.as_str(),
        "name": n.name,
        "transport_address": n.transport_address,
        "attributes": n.attributes,
        "weight_ranking": weight_ranking,
    })
}

fn verdicts_json(vs: &[Verdict], include_yes: bool) -> Value {
    Value::Array(
        vs.iter()
            .filter(|v| include_yes || v.decision != Decision::Yes)
            .map(|v| json!({"decider": v.decider, "decision": v.decision.label(), "explanation": v.explanation}))
            .collect(),
    )
}

/// The explanation for one copy, in the plugin's shape.
pub fn explain(
    ctx: &Context,
    state: &ClusterState,
    copy: &ShardRouting,
    include_yes: bool,
) -> Value {
    let table = &state.routing;
    let mut out = json!({
        "index": copy.index,
        "shard": copy.shard,
        "primary": copy.primary,
    });
    let ranking = ranked(ctx, table, &copy.index);
    let rank_of =
        |id: &NodeId| ranking.iter().position(|(n, _)| n.id == *id).map(|p| p + 1).unwrap_or(1);
    match copy.state {
        ShardState::Unassigned => {
            out["current_state"] = json!("unassigned");
            if let Some(u) = &copy.unassigned {
                let mut info = json!({
                    "reason": u.reason,
                    "at": super::state::iso_millis(u.at_millis),
                    "last_allocation_status": u.allocation_status,
                });
                if u.failed_allocations > 0 {
                    info["failed_allocation_attempts"] = json!(u.failed_allocations);
                }
                if let Some(d) = &u.details {
                    info["details"] = json!(d);
                }
                out["unassigned_info"] = info;
            }
            let mut decisions = Vec::new();
            let mut best = Decision::No;
            let mut yes_node: Option<&DiscoveryNode> = None;
            for (node, _) in &ranking {
                let vs = can_allocate(ctx, table, copy, node);
                let d = overall(&vs);
                if d < best {
                    best = d;
                }
                if d == Decision::Yes && yes_node.is_none() {
                    yes_node = Some(node);
                }
                let mut nj = json!({
                    "node_id": node.id.as_str(),
                    "node_name": node.name,
                    "transport_address": node.transport_address,
                    "node_attributes": node.attributes,
                    "node_decision": match d { Decision::Yes => "yes", Decision::Throttle => "throttle", Decision::No => "no" },
                    "weight_ranking": rank_of(&node.id),
                });
                nj["deciders"] = verdicts_json(&vs, include_yes);
                decisions.push(nj);
            }
            let delayed = copy.unassigned.as_ref().map(|u| u.delayed).unwrap_or(false);
            let (can, why) = if ranking.is_empty() {
                (
                    "no",
                    "cannot allocate because allocation is not permitted to any of the nodes"
                        .to_string(),
                )
            } else if delayed {
                let is = ctx.index_settings(&copy.index);
                let left = copy
                    .unassigned
                    .as_ref()
                    .map(|u| (u.at_millis + is.delayed_timeout).saturating_sub(ctx.now))
                    .unwrap_or(0);
                out["configured_delay_in_millis"] = json!(is.delayed_timeout);
                out["remaining_delay_in_millis"] = json!(left);
                (
                    "allocation_delayed",
                    format!(
                        "cannot allocate because the cluster is still waiting [{}ms] for the departed node holding a replica to rejoin, despite being allowed to allocate the shard to at least one other node",
                        left
                    ),
                )
            } else {
                match best {
                    Decision::Yes => ("yes", "can allocate the shard".to_string()),
                    Decision::Throttle => {
                        ("throttled", "allocation temporarily throttled".to_string())
                    }
                    Decision::No => (
                        "no",
                        "cannot allocate because allocation is not permitted to any of the nodes"
                            .to_string(),
                    ),
                }
            };
            out["can_allocate"] = json!(can);
            out["allocate_explanation"] = json!(why);
            if let Some(n) = yes_node {
                out["target_node"] = json!({"id": n.id.as_str(), "name": n.name, "transport_address": n.transport_address, "attributes": n.attributes});
            }
            out["node_allocation_decisions"] = Value::Array(decisions);
        }
        _ => {
            let Some(node_id) = copy.node.clone() else { return out };
            let node = ctx.nodes.get(&node_id);
            out["current_state"] = json!(copy.state.as_str().to_ascii_lowercase());
            if let Some(n) = node {
                out["current_node"] = node_json(n, rank_of(&n.id));
            }
            if copy.state == ShardState::Relocating
                && let Some(t) = copy.relocating_node.as_ref().and_then(|r| ctx.nodes.get(r))
            {
                out["current_node"]["relocating_to"] = json!({"id": t.id.as_str(), "name": t.name, "transport_address": t.transport_address});
            }
            if copy.state == ShardState::Started {
                let remain = node.map(|n| can_remain(ctx, table, copy, n)).unwrap_or_default();
                let remain_d = overall(&remain);
                out["can_remain_on_current_node"] =
                    json!(if remain_d == Decision::Yes { "yes" } else { "no" });
                if remain_d != Decision::Yes {
                    out["can_remain_decisions"] = verdicts_json(&remain, include_yes);
                }
                let reb = can_rebalance(ctx, table, copy);
                let reb_d = overall(&reb);
                let cluster_level: Vec<Verdict> = reb.clone();
                out["can_rebalance_cluster"] = json!(match overall(&cluster_level) {
                    Decision::Yes => "yes",
                    Decision::Throttle => "throttled",
                    Decision::No => "no",
                });
                if overall(&cluster_level) != Decision::Yes {
                    out["can_rebalance_cluster_decisions"] =
                        verdicts_json(&cluster_level, include_yes);
                }
                // the other nodes
                let mut decisions = Vec::new();
                let mut target: Option<&DiscoveryNode> = None;
                let mut best_other = Decision::No;
                for (other, w) in &ranking {
                    if other.id == node_id {
                        continue;
                    }
                    let vs = can_allocate(ctx, table, copy, other);
                    let d = overall(&vs);
                    let better = *w
                        < weight(ctx, table, &copy.index, &node_id) - ctx.cluster.balance_threshold;
                    let d = if d == Decision::Yes && !better { Decision::No } else { d };
                    if d < best_other {
                        best_other = d;
                    }
                    if d == Decision::Yes && target.is_none() {
                        target = Some(other);
                    }
                    let mut nj = json!({
                        "node_id": other.id.as_str(),
                        "node_name": other.name,
                        "transport_address": other.transport_address,
                        "node_attributes": other.attributes,
                        "node_decision": match d { Decision::Yes => "yes", Decision::Throttle => "throttle", Decision::No => "no" },
                        "weight_ranking": rank_of(&other.id),
                    });
                    nj["deciders"] = verdicts_json(&vs, include_yes);
                    decisions.push(nj);
                }
                if reb_d != Decision::Yes || overall(&cluster_level) != Decision::Yes {
                    out["can_rebalance_to_other_node"] = json!("no");
                    out["rebalance_explanation"] = json!("rebalancing is not allowed");
                } else if let Some(t) = target {
                    out["can_rebalance_to_other_node"] = json!("yes");
                    out["rebalance_explanation"] = json!(
                        "shard cannot remain on this node, moving to a node that can hold it"
                            .to_string()
                    );
                    if remain_d == Decision::Yes {
                        out["rebalance_explanation"] = json!("can rebalance shard");
                    }
                    out["target_node"] = json!({"id": t.id.as_str(), "name": t.name, "transport_address": t.transport_address, "attributes": t.attributes});
                } else if best_other == Decision::Throttle {
                    out["can_rebalance_to_other_node"] = json!("throttled");
                    out["rebalance_explanation"] = json!("rebalancing is throttled");
                } else {
                    out["can_rebalance_to_other_node"] = json!("no");
                    out["rebalance_explanation"] = json!(if remain_d == Decision::Yes {
                        "cannot rebalance as no target node exists that can both allocate this shard and improve the cluster balance"
                    } else {
                        "cannot move shard to another node, even though it is not allowed to remain on its current node"
                    });
                }
                out["node_allocation_decisions"] = Value::Array(decisions);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, attrs: &[(&str, &str)]) -> DiscoveryNode {
        DiscoveryNode {
            id: NodeId(name.into()),
            name: name.into(),
            ephemeral_id: NodeId::random(),
            transport_address: format!("10.0.0.{}:9300", name.len()),
            roles: vec!["cluster_manager".into(), "data".into()],
            attributes: attrs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            http_address: String::new(),
        }
    }

    fn index(name: &str, shards: u32, replicas: u32, settings: Value) -> IndexMetadata {
        IndexMetadata {
            name: name.into(),
            uuid: format!("{name}-uuid"),
            version: 1,
            mapping_version: 1,
            settings_version: 1,
            aliases_version: 1,
            state: "open".into(),
            settings,
            mappings: json!({}),
            aliases: json!({}),
            number_of_shards: shards,
            number_of_replicas: replicas,
            primary_terms: BTreeMap::new(),
            in_sync_allocations: BTreeMap::new(),
            creation_date: 1_000,
        }
    }

    struct World {
        nodes: BTreeMap<NodeId, DiscoveryNode>,
        indices: BTreeMap<String, IndexMetadata>,
        cluster: ClusterSettings,
        home: BTreeMap<String, NodeId>,
        home_documents: BTreeSet<String>,
        held: BTreeMap<NodeId, Vec<(String, String, String)>>,
        table: RoutingTable,
        now: Millis,
    }

    impl World {
        fn new(nodes: Vec<DiscoveryNode>, indices: Vec<IndexMetadata>, home: &str) -> World {
            let home_map = indices.iter().map(|i| (i.name.clone(), NodeId(home.into()))).collect();
            World {
                nodes: nodes.into_iter().map(|n| (n.id.clone(), n)).collect(),
                indices: indices.into_iter().map(|i| (i.name.clone(), i)).collect(),
                cluster: ClusterSettings::default(),
                home: home_map,
                home_documents: BTreeSet::new(),
                held: BTreeMap::new(),
                table: RoutingTable::default(),
                now: 10_000,
            }
        }

        fn ctx(&self) -> Context<'_> {
            Context {
                nodes: &self.nodes,
                indices: &self.indices,
                cluster: &self.cluster,
                primary_home: &self.home,
                home_holds_documents: &self.home_documents,
                held: &self.held,
                now: self.now,
            }
        }

        fn reroute(&mut self) -> Changes {
            let (t, c) = reroute(&self.ctx(), &self.table);
            self.table = t;
            c
        }

        /// Every initializing copy reports started, as data nodes would.
        fn start_all(&mut self) {
            let starts: Vec<(String, u32, String)> = self
                .table
                .all()
                .filter(|c| c.state == ShardState::Initializing)
                .map(|c| (c.index.clone(), c.shard, c.allocation_id.clone().unwrap()))
                .collect();
            for (i, s, a) in starts {
                assert!(shard_started(&mut self.table, &i, s, &a).is_some());
            }
        }

        /// Reroute and start until nothing moves.
        fn settle(&mut self) {
            for _ in 0..20 {
                let c = self.reroute();
                self.start_all();
                if c.is_empty() && self.table.all().all(|c| c.state == ShardState::Started) {
                    return;
                }
            }
        }

        fn count(&self, node: &str, index: &str) -> usize {
            self.table
                .on_node(&NodeId(node.into()))
                .filter(|c| c.index == index && c.state != ShardState::Unassigned)
                .count()
        }
    }

    #[test]
    fn replicas_go_to_other_nodes_and_spread_evenly() {
        let mut w = World::new(
            vec![node("a", &[]), node("b", &[]), node("c", &[])],
            vec![index("logs", 4, 1, json!({}))],
            "a",
        );
        w.settle();
        let copies: Vec<&ShardRouting> = w.table.shards_of("logs").collect();
        assert_eq!(copies.len(), 8);
        assert!(copies.iter().all(|c| c.state == ShardState::Started), "{copies:?}");
        // no shard has two copies on one node
        for s in 0..4 {
            let nodes: BTreeSet<_> =
                copies.iter().filter(|c| c.shard == s).map(|c| c.node.clone()).collect();
            assert_eq!(nodes.len(), 2);
        }
        // the shards of an index live together: a copy here is a copy of the
        // whole index, so the eight copies sit on two nodes, four each, and
        // the third node holds none of this index
        let counts = [w.count("a", "logs"), w.count("b", "logs"), w.count("c", "logs")];
        let mut sorted = counts;
        sorted.sort();
        assert_eq!(sorted, [0, 4, 4], "a={} b={} c={}", counts[0], counts[1], counts[2]);
    }

    #[test]
    fn one_node_keeps_the_replica_unassigned_with_the_plugins_reason() {
        let mut w = World::new(vec![node("a", &[])], vec![index("x", 1, 1, json!({}))], "a");
        w.settle();
        let replica = w.table.shards_of("x").find(|c| !c.primary).unwrap().clone();
        assert_eq!(replica.state, ShardState::Unassigned);
        let vs = can_allocate(&w.ctx(), &w.table, &replica, &w.nodes[&NodeId("a".into())]);
        let same = vs.iter().find(|v| v.decider == "same_shard").unwrap();
        assert_eq!(same.decision, Decision::No);
        assert!(same.explanation.starts_with("a copy of this shard is already allocated to this node [[x][0], node[a], [P], s[STARTED], a[id="), "{}", same.explanation);
        let state = ClusterState { routing: w.table.clone(), ..ClusterState::empty("c", "u") };
        let e = explain(&w.ctx(), &state, &replica, false);
        assert_eq!(e["can_allocate"], "no");
        assert_eq!(
            e["allocate_explanation"],
            "cannot allocate because allocation is not permitted to any of the nodes"
        );
        assert_eq!(e["node_allocation_decisions"][0]["deciders"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn filters_and_enable_and_limits_say_no() {
        let mut w = World::new(
            vec![
                node("a", &[("zone", "z1")]),
                node("b", &[("zone", "z2")]),
                node("c", &[("zone", "z2")]),
            ],
            vec![index(
                "f",
                2,
                1,
                json!({"index": {"routing": {"allocation": {"exclude": {"_name": "b"}}}}}),
            )],
            "a",
        );
        w.settle();
        // b is excluded: every replica on c
        assert_eq!(w.count("c", "f"), 2);
        assert_eq!(w.count("b", "f"), 0);
        // now the cluster forbids replica allocation and asks for one more replica
        w.cluster.enable = Enable::Primaries;
        w.indices.get_mut("f").unwrap().number_of_replicas = 2;
        w.settle();
        let waiting: Vec<&ShardRouting> =
            w.table.all().filter(|c| c.state == ShardState::Unassigned).collect();
        assert_eq!(waiting.len(), 2);
        let vs = can_allocate(&w.ctx(), &w.table, waiting[0], &w.nodes[&NodeId("b".into())]);
        assert!(vs.iter().any(|v| v.decider == "enable" && v.decision == Decision::No && v.explanation == "replica allocations are forbidden due to cluster setting [cluster.routing.allocation.enable=primaries]"), "{vs:?}");
        // a per-index limit of one shard per node
        w.cluster.enable = Enable::All;
        w.indices.get_mut("f").unwrap().settings =
            json!({"index": {"routing": {"allocation": {"total_shards_per_node": "1"}}}});
        let copy = waiting[0].clone();
        let vs = can_allocate(&w.ctx(), &w.table, &copy, &w.nodes[&NodeId("c".into())]);
        let sl = vs.iter().find(|v| v.decider == "shards_limit").unwrap();
        assert_eq!(sl.decision, Decision::No);
        assert!(sl.explanation.starts_with("too many shards [2] allocated to this node for index [f], index setting [index.routing.allocation.total_shards_per_node=1]"), "{}", sl.explanation);
    }

    fn with_roles(mut n: DiscoveryNode, roles: &[&str]) -> DiscoveryNode {
        n.roles = roles.iter().map(|r| r.to_string()).collect();
        n
    }

    #[test]
    fn a_new_index_made_on_a_manager_without_the_data_role_is_placed_on_a_data_node() {
        let mut w = World::new(
            vec![
                with_roles(node("m", &[]), &["cluster_manager"]),
                with_roles(node("d1", &[]), &["data"]),
                with_roles(node("d2", &[]), &["data"]),
                with_roles(node("c", &[]), &[]),
            ],
            vec![index("fresh", 2, 1, json!({}))],
            "m",
        );
        w.reroute();
        // never started where it was made: placed like any unassigned copy
        assert_eq!(w.count("m", "fresh"), 0);
        let primary = w.table.primary("fresh", 0).unwrap();
        assert_eq!(primary.state, ShardState::Initializing);
        assert!(matches!(primary.node.as_ref().map(|n| n.as_str()), Some("d1" | "d2")));
        w.settle();
        assert_eq!(w.count("m", "fresh") + w.count("c", "fresh"), 0);
        assert_eq!(w.count("d1", "fresh") + w.count("d2", "fresh"), 4);
        // and nothing may be put on either of them by hand
        let copy = w.table.primary("fresh", 0).unwrap().clone();
        for name in ["m", "c"] {
            let vs = can_allocate(&w.ctx(), &w.table, &copy, &w.nodes[&NodeId(name.into())]);
            assert!(vs.iter().any(|v| v.decider == "data_role" && v.decision == Decision::No));
        }
    }

    #[test]
    fn a_primary_whose_documents_are_on_a_node_without_the_data_role_is_moved_with_them() {
        let mut w = World::new(
            vec![
                with_roles(node("m", &[]), &["cluster_manager"]),
                with_roles(node("d1", &[]), &["data"]),
            ],
            vec![index("written", 1, 0, json!({}))],
            "m",
        );
        w.home_documents.insert("written".into());
        w.reroute();
        // started where the documents are, and already on its way to a data
        // node: made again empty there, the documents would be lost
        let copies: Vec<&ShardRouting> = w.table.shards_of("written").collect();
        assert!(copies.iter().any(|c| c.node == Some(NodeId("m".into()))
            && c.state == ShardState::Relocating
            && c.relocating_node == Some(NodeId("d1".into()))));
        assert!(copies.iter().any(|c| c.node == Some(NodeId("d1".into()))
            && c.state == ShardState::Initializing
            && c.relocating_node == Some(NodeId("m".into()))));
        w.settle();
        assert_eq!(w.count("m", "written"), 0);
        assert_eq!(w.count("d1", "written"), 1);
    }

    #[test]
    fn a_home_a_filter_excludes_is_not_where_a_new_primary_starts() {
        let mut w = World::new(
            vec![node("a", &[]), node("b", &[])],
            vec![index("kept-off", 1, 0, json!({"index.routing.allocation.exclude._name": "a"}))],
            "a",
        );
        w.settle();
        assert_eq!(w.count("a", "kept-off"), 0);
        assert_eq!(w.count("b", "kept-off"), 1);
    }

    #[test]
    fn a_cluster_exclude_moves_what_the_node_holds() {
        let mut w = World::new(
            vec![node("a", &[]), node("b", &[])],
            vec![index("drained", 3, 0, json!({}))],
            "a",
        );
        w.settle();
        assert_eq!(w.count("a", "drained"), 3);
        w.cluster.filters = filters_from(
            &json!({"cluster.routing.allocation.exclude._name": "a"}),
            "cluster.routing.allocation",
        );
        w.settle();
        assert_eq!(w.count("a", "drained"), 0);
        assert_eq!(w.count("b", "drained"), 3);
    }

    #[test]
    fn a_copy_is_not_moved_to_the_node_it_is_on() {
        let mut w = World::new(
            vec![node("a", &[]), node("b", &[])],
            vec![index("here", 1, 0, json!({}))],
            "a",
        );
        w.settle();
        let copy = w.table.primary("here", 0).unwrap().clone();
        let vs = can_allocate(&w.ctx(), &w.table, &copy, &w.nodes[&NodeId("a".into())]);
        assert!(
            vs.iter().any(|v| v.decider == "same_shard" && v.decision == Decision::No),
            "{vs:?}"
        );
        let commands =
            [json!({"move": {"index": "here", "shard": 0, "from_node": "a", "to_node": "a"}})];
        assert!(apply_commands(&w.ctx(), &w.table, &commands, false).is_err());
    }

    /// A move of any shard of an index moves the copy, and a second command
    /// for another shard of it is the same move: the shards of an index are
    /// held together, so a move of one alone was answered and then undone by
    /// the placement that keeps them together.
    #[test]
    fn a_move_of_any_shard_moves_the_whole_copy() {
        let mut w = World::new(
            vec![node("a", &[]), node("b", &[])],
            vec![index("pair", 2, 0, json!({}))],
            "a",
        );
        w.settle();
        assert_eq!(w.count("a", "pair"), 2);
        let commands =
            [json!({"move": {"index": "pair", "shard": 1, "from_node": "a", "to_node": "b"}})];
        let (table, _) = apply_commands(&w.ctx(), &w.table, &commands, false).unwrap();
        w.table = table;
        w.settle();
        assert_eq!(w.count("a", "pair"), 0);
        assert_eq!(w.count("b", "pair"), 2);
        // and one command per shard, which is how a copy is taken off a node
        let commands = [
            json!({"move": {"index": "pair", "shard": 0, "from_node": "b", "to_node": "a"}}),
            json!({"move": {"index": "pair", "shard": 1, "from_node": "b", "to_node": "a"}}),
        ];
        let (table, _) = apply_commands(&w.ctx(), &w.table, &commands, false).unwrap();
        w.table = table;
        w.settle();
        assert_eq!(w.count("a", "pair"), 2);
        assert_eq!(w.count("b", "pair"), 0);
    }

    #[test]
    fn awareness_keeps_copies_in_different_zones() {
        let mut w = World::new(
            vec![
                node("a", &[("zone", "z1")]),
                node("b", &[("zone", "z1")]),
                node("c", &[("zone", "z2")]),
            ],
            vec![index("z", 2, 1, json!({}))],
            "a",
        );
        w.cluster.awareness_attributes = vec!["zone".into()];
        w.settle();
        // no shard has both copies in one zone: every copy in z1 has its
        // twin on c, in z2
        for shard in 0..2 {
            let zones: BTreeSet<&str> = w
                .table
                .shards_of("z")
                .filter(|c| c.shard == shard && c.state != ShardState::Unassigned)
                .map(|c| w.nodes[c.node.as_ref().unwrap()].attributes["zone"].as_str())
                .collect();
            assert_eq!(zones.len(), 2, "shard {shard} copies share a zone");
        }
        assert_eq!(w.count("c", "z"), 2);
        let replica = w.table.shards_of("z").find(|c| !c.primary).unwrap().clone();
        let vs = can_allocate(&w.ctx(), &w.table, &replica, &w.nodes[&NodeId("b".into())]);
        let aw = vs.iter().find(|v| v.decider == "awareness").unwrap();
        assert_eq!(aw.decision, Decision::No);
        assert!(aw.explanation.starts_with("there are [2] copies of this shard and [2] values for attribute [zone] ([z1, z2] from nodes in the cluster and no forced awareness) so there may be at most [1] copies of this shard allocated to nodes with each value, but (including this copy) there would be [2] copies allocated to nodes with [node.attr.zone: z1]"), "{}", aw.explanation);
    }

    #[test]
    fn a_node_that_leaves_loses_its_replicas_after_the_delay_and_a_primary_is_promoted() {
        let mut w = World::new(
            vec![node("a", &[]), node("b", &[]), node("c", &[])],
            vec![index(
                "d",
                2,
                1,
                json!({"index": {"unassigned": {"node_left": {"delayed_timeout": "5s"}}}}),
            )],
            "a",
        );
        w.settle();
        // whichever node holds the replica copies: an index sits on two of the
        // three, so the third holds none of it
        let holder = w
            .table
            .shards_of("d")
            .find(|c| !c.primary)
            .and_then(|c| c.node.clone())
            .expect("a replica somewhere");
        let on_b: Vec<(String, u32)> =
            w.table.on_node(&holder).map(|c| (c.index.clone(), c.shard)).collect();
        assert!(!on_b.is_empty());
        w.nodes.remove(&holder);
        w.now += 1_000;
        let ch = w.reroute();
        assert_eq!(ch.unassigned.len(), on_b.len());
        let waiting: Vec<&ShardRouting> =
            w.table.all().filter(|c| c.state == ShardState::Unassigned).collect();
        assert!(waiting.iter().all(|c| c.unassigned.as_ref().unwrap().delayed
            && c.unassigned.as_ref().unwrap().reason == "NODE_LEFT"));
        // the node left at 11s; the replicas may move at 16s
        assert_eq!(ch.next_delay_at, Some(16_000));
        // still waiting before the delay is up
        w.now += 2_000;
        let ch = w.reroute();
        assert!(ch.assigned.is_empty());
        // after it, allocated to c
        w.now += 3_000;
        w.settle();
        assert!(w.table.all().all(|c| c.state == ShardState::Started));
        // the copies went to whichever node was free: two shards' worth of one
        // copy of the index, wherever the allocator put them
        let elsewhere: usize = w
            .nodes
            .keys()
            .map(|n| w.table.on_node(n).filter(|c| c.index == "d" && !c.primary).count())
            .max()
            .unwrap_or(0);
        assert_eq!(elsewhere, 2);
        // the primaries' node leaves: the copies elsewhere take over, and
        // both shards' primaries land on the one node that holds the index
        let primary_node = w
            .table
            .shards_of("d")
            .find(|c| c.primary)
            .and_then(|c| c.node.clone())
            .expect("a primary");
        w.nodes.remove(&primary_node);
        w.home.clear();
        let ch = w.reroute();
        assert_eq!(ch.promoted.len(), 2, "{ch:?}");
        let promoted_to: BTreeSet<Option<NodeId>> =
            w.table.shards_of("d").filter(|c| c.primary).map(|c| c.node.clone()).collect();
        assert_eq!(promoted_to.len(), 1, "{promoted_to:?}");
        assert!(
            w.table.shards_of("d").filter(|c| c.primary).all(|c| c.state == ShardState::Started)
        );
    }

    #[test]
    fn a_failed_copy_is_retried_until_the_limit_and_then_only_by_hand() {
        let mut w = World::new(
            vec![node("a", &[]), node("b", &[])],
            vec![index("r", 1, 1, json!({}))],
            "a",
        );
        w.reroute();
        for attempt in 1..=5 {
            let rep = w.table.shards_of("r").find(|c| !c.primary).unwrap().clone();
            assert_eq!(rep.state, ShardState::Initializing, "attempt {attempt}");
            assert!(shard_failed(
                &mut w.table,
                "r",
                0,
                rep.allocation_id.as_ref().unwrap(),
                w.now,
                "boom"
            ));
            w.reroute();
        }
        let rep = w.table.shards_of("r").find(|c| !c.primary).unwrap().clone();
        assert_eq!(rep.state, ShardState::Unassigned);
        assert_eq!(rep.unassigned.as_ref().unwrap().failed_allocations, 5);
        // a refused replica reads `no_attempt`, as the plugin leaves it
        assert_eq!(rep.unassigned.as_ref().unwrap().allocation_status, "no_attempt");
        let vs = can_allocate(&w.ctx(), &w.table, &rep, &w.nodes[&NodeId("b".into())]);
        let mr = vs.iter().find(|v| v.decider == "max_retry").unwrap();
        assert_eq!(mr.decision, Decision::No);
        assert!(mr.explanation.starts_with("shard has exceeded the maximum number of retries [5] on failed allocation attempts - manually call [/_cluster/reroute?retry_failed=true] to retry, [unassigned_info[[reason=ALLOCATION_FAILED]"), "{}", mr.explanation);
        assert_eq!(retry_failed(&mut w.table), 1);
        w.reroute();
        assert_eq!(
            w.table.shards_of("r").find(|c| !c.primary).unwrap().state,
            ShardState::Initializing
        );
    }

    /// A primary whose only copy is gone stays unassigned, and the index is
    /// red, until someone accepts the data loss.
    #[test]
    fn a_lost_primary_is_not_made_again_out_of_nothing() {
        let mut w = World::new(
            vec![node("a", &[]), node("b", &[])],
            vec![index("solo", 1, 0, json!({}))],
            "a",
        );
        w.settle();
        assert_eq!(w.table.primary("solo", 0).unwrap().node, Some(NodeId("a".into())));
        w.nodes.remove(&NodeId("a".into()));
        w.home.clear();
        w.now += 1_000;
        w.settle();
        let p = w.table.primary("solo", 0).unwrap().clone();
        assert_eq!(p.state, ShardState::Unassigned, "{p:?}");
        assert_eq!(p.unassigned.as_ref().unwrap().allocation_status, "no_valid_shard_copy");
        let state = ClusterState { routing: w.table.clone(), ..ClusterState::empty("c", "u") };
        assert_eq!(state.health_status(None), "red");
        // the command that accepts the loss places an empty one
        let cmd = json!([{"allocate_empty_primary": {"index": "solo", "shard": 0, "node": "b", "accept_data_loss": true}}]);
        let (t, _) = apply_commands(&w.ctx(), &w.table, cmd.as_array().unwrap(), false).unwrap();
        w.table = t;
        assert_eq!(w.table.primary("solo", 0).unwrap().state, ShardState::Initializing);
        w.settle();
        assert_eq!(w.table.primary("solo", 0).unwrap().node, Some(NodeId("b".into())));
        // without accepting the loss the command is refused
        let cmd = json!([{"allocate_empty_primary": {"index": "solo", "shard": 0, "node": "b"}}]);
        let mut fresh = w.table.clone();
        if let Some(c) = fresh.indices.get_mut("solo").unwrap().get_mut(&0).unwrap().first_mut() {
            c.state = ShardState::Unassigned;
            c.node = None;
            c.unassigned = Some(unassigned_info("NODE_LEFT", 0, "no_valid_shard_copy", 0));
        }
        assert!(apply_commands(&w.ctx(), &fresh, cmd.as_array().unwrap(), false).is_err());
    }

    /// The node that held a lost primary's data comes back: the primary goes
    /// back to it, an existing-store recovery, not an empty one.
    #[test]
    fn a_lost_primary_goes_back_to_the_node_that_still_holds_its_data() {
        let mut w = World::new(
            vec![node("a", &[]), node("b", &[])],
            vec![index("solo", 1, 0, json!({}))],
            "a",
        );
        w.settle();
        let uuid = w.indices["solo"].uuid.clone();
        w.nodes.remove(&NodeId("a".into()));
        w.home.clear();
        w.now += 1_000;
        w.settle();
        assert_eq!(w.table.primary("solo", 0).unwrap().state, ShardState::Unassigned);
        // a returns, and says it still holds the index, with the copy that was in sync
        let aid = "aid-a".to_string();
        w.indices.get_mut("solo").unwrap().in_sync_allocations.insert(0, vec![aid.clone()]);
        w.nodes.insert(NodeId("a".into()), node("a", &[]));
        w.held.insert(NodeId("a".into()), vec![("solo".into(), uuid.clone(), aid)]);
        let ch = w.reroute();
        assert_eq!(ch.assigned.len(), 1, "{ch:?}");
        let p = w.table.primary("solo", 0).unwrap().clone();
        assert_eq!(p.node, Some(NodeId("a".into())));
        assert_eq!(p.state, ShardState::Initializing);
        assert_eq!(p.to_json()["recovery_source"]["type"], "EXISTING_STORE");
        // a node holding a different creation of the index (another uuid) does not count
        let mut w2 = World::new(
            vec![node("a", &[]), node("b", &[])],
            vec![index("solo", 1, 0, json!({}))],
            "a",
        );
        w2.settle();
        w2.nodes.remove(&NodeId("a".into()));
        w2.home.clear();
        w2.now += 1_000;
        w2.settle();
        w2.held
            .insert(NodeId("b".into()), vec![("solo".into(), "another-uuid".into(), "x".into())]);
        w2.reroute();
        assert_eq!(w2.table.primary("solo", 0).unwrap().state, ShardState::Unassigned);
        // and a copy of the right index that was not in sync does not count either
        w2.held.insert(
            NodeId("b".into()),
            vec![("solo".into(), w2.indices["solo"].uuid.clone(), "stale".into())],
        );
        w2.indices.get_mut("solo").unwrap().in_sync_allocations.insert(0, vec!["fresh".into()]);
        w2.reroute();
        assert_eq!(w2.table.primary("solo", 0).unwrap().state, ShardState::Unassigned);
    }

    #[test]
    fn a_new_node_gets_copies_moved_to_it_one_at_a_time() {
        // two indices, so the third node has something to take: one index with
        // one replica fills exactly two nodes and has no reason to move
        let mut w = World::new(
            vec![node("a", &[]), node("b", &[])],
            vec![index("m", 6, 1, json!({})), index("n", 6, 1, json!({}))],
            "a",
        );
        w.settle();
        assert_eq!(w.count("b", "m"), 6);
        w.nodes.insert(NodeId("c".into()), node("c", &[]));
        let ch = w.reroute();
        // one copy is chosen to move, and the index moves with it: a copy is
        // a copy of the whole index, so every shard of that copy relocates
        assert!(!ch.relocating.is_empty(), "{ch:?}");
        let (_, _, _, from, to) = &ch.relocating[0];
        assert!(from.as_str() == "a" || from.as_str() == "b", "{from:?}");
        assert_eq!(to.as_str(), "c");
        assert_eq!(w.table.all().filter(|c| c.state == ShardState::Relocating).count(), 6);
        assert_eq!(
            w.table
                .all()
                .filter(|c| c.state == ShardState::Initializing && c.relocating_node.is_some())
                .count(),
            6
        );
        // the explain of a relocating copy names where it goes
        let moving = w.table.all().find(|c| c.state == ShardState::Relocating).unwrap().clone();
        let state = ClusterState { routing: w.table.clone(), ..ClusterState::empty("c", "u") };
        let e = explain(&w.ctx(), &state, &moving, false);
        assert_eq!(e["current_state"], "relocating");
        assert_eq!(e["current_node"]["relocating_to"]["id"], "c");
        // once it has started, the source copy is gone and the next move begins
        w.start_all();
        assert_eq!(w.table.all().filter(|c| c.index == "m").count(), 12);
        w.settle();
        // the copies end up shared out: twelve over three nodes, none with
        // more than five, no shard with both copies on one node
        let total = w.count("a", "m") + w.count("b", "m") + w.count("c", "m");
        assert_eq!(
            total,
            12,
            "a={} b={} c={}",
            w.count("a", "m"),
            w.count("b", "m"),
            w.count("c", "m")
        );
        // the index sits on two nodes, six shards' worth of a copy on each
        let mut counts = [w.count("a", "m"), w.count("b", "m"), w.count("c", "m")];
        counts.sort();
        assert_eq!(counts, [0, 6, 6], "{counts:?}");
        for shard in 0..6 {
            let nodes: BTreeSet<_> = w
                .table
                .shards_of("m")
                .filter(|c| c.shard == shard)
                .map(|c| c.node.clone())
                .collect();
            assert_eq!(nodes.len(), 2, "shard {shard}");
        }
        assert!(w.table.all().all(|c| c.state == ShardState::Started));
    }

    #[test]
    fn rebalancing_waits_for_every_copy_and_the_explain_says_so() {
        let mut w = World::new(
            vec![node("a", &[]), node("b", &[])],
            vec![index("w", 2, 1, json!({}))],
            "a",
        );
        w.reroute();
        // replicas are initializing: nothing may move yet
        let primary = w.table.primary("w", 0).unwrap().clone();
        let vs = can_rebalance(&w.ctx(), &w.table, &primary);
        assert!(vs.iter().any(|v| v.decider == "rebalance_only_when_active"
            && v.decision == Decision::No
            && v.explanation
                == "rebalancing is not allowed until all replicas in the cluster are active"));
        assert!(vs.iter().any(|v| v.decider == "cluster_rebalance" && v.decision == Decision::No && v.explanation == "the cluster has inactive shards and cluster setting [cluster.routing.allocation.allow_rebalance] is set to [indices_all_active]"));
        let state = ClusterState { routing: w.table.clone(), ..ClusterState::empty("c", "u") };
        let e = explain(&w.ctx(), &state, &primary, false);
        assert_eq!(e["can_remain_on_current_node"], "yes");
        assert_eq!(e["can_rebalance_cluster"], "no");
        assert_eq!(e["can_rebalance_to_other_node"], "no");
        assert_eq!(e["rebalance_explanation"], "rebalancing is not allowed");
    }

    #[test]
    fn the_settings_are_read_from_the_cluster_document() {
        let v = json!({
            "persistent": {"cluster.routing.allocation.enable": "none", "cluster": {"routing": {"allocation": {"awareness": {"attributes": "zone,rack"}}}}},
            "transient": {"cluster.routing.allocation.exclude._name": "old*", "cluster.routing.allocation.node_concurrent_recoveries": "5"}
        });
        let s = ClusterSettings::from_value(&v);
        assert_eq!(s.enable, Enable::None);
        assert_eq!(s.awareness_attributes, vec!["zone", "rack"]);
        assert_eq!(s.filters.exclude["_name"], vec!["old*"]);
        assert_eq!(s.node_concurrent_incoming, 5);
        let excluded = Filters { exclude: s.filters.exclude.clone(), ..Filters::default() };
        assert!(excluded.check(&node("oldnode", &[])).is_some());
        assert!(excluded.check(&node("new", &[])).is_none());
        assert_eq!(time_ms("1m"), Some(60_000));
        assert_eq!(time_ms("0"), Some(0));
    }
}
