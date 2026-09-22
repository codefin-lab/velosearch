//! An index's settings, and the defaults underneath them.

use super::*;

/// Settings OpenSearch reports under `defaults` when asked for them. Only the
/// handful the conformance suite reads are modelled.
pub(crate) fn default_settings() -> Value {
    json!({"index": {
        "refresh_interval": "1s",
        "max_result_window": "10000",
        "number_of_routing_shards": "1",
        "codec": "default",
        "auto_expand_replicas": "false",
        "max_inner_result_window": "100",
        "max_rescore_window": "10000",
        "query": {"default_field": ["*"]},
    }})
}

/// `flat_settings=true` renders `{"index":{"a":1}}` as `{"index.a":1}`.
pub(crate) fn flatten_settings(v: &Value, prefix: &str, out: &mut serde_json::Map<String, Value>) {
    match v {
        Value::Object(o) => {
            for (k, child) in o {
                let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten_settings(child, &path, out);
            }
        }
        leaf => {
            out.insert(prefix.to_string(), leaf.clone());
        }
    }
}

/// Take a setting out of every spelling it may have been written in: under
/// `index` or at the top, flat or nested, with its `index.` prefix or without.
fn forget_setting(settings: &mut Value, key: &str) {
    fn remove_path(node: &mut Value, parts: &[&str]) {
        let Some(o) = node.as_object_mut() else { return };
        // the rest of the path written flat under this object
        o.remove(&parts.join("."));
        if parts.len() > 1
            && let Some(child) = o.get_mut(parts[0])
        {
            remove_path(child, &parts[1..]);
        }
    }
    let parts: Vec<&str> = key.split('.').collect();
    let prefixed: Vec<&str> = std::iter::once("index").chain(parts.iter().copied()).collect();
    remove_path(settings, &parts);
    remove_path(settings, &prefixed);
    if let Some(index) = settings.get_mut("index") {
        remove_path(index, &parts);
        remove_path(index, &prefixed);
    }
}

/// `human` asks for a readable form beside each machine one.
pub(crate) fn add_human_settings(view: &mut Value, st: &IdxState) {
    let created = st.created_millis();
    let text = st.created_string();
    if let Some(o) = view.pointer_mut("/index").and_then(|v| v.as_object_mut()) {
        o.insert("creation_date_string".into(), json!(text));
        o.entry("creation_date".to_string()).or_insert_with(|| json!(created.to_string()));
        // the version an index was made under is reported the same two ways
        let ver = o
            .get("version")
            .and_then(|v| v.get("created"))
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "136407827".to_string());
        o.insert(
            "version".into(),
            json!({
                "created": ver, "created_string": crate::OPENSEARCH_VERSION,
            }),
        );
    } else if let Some(o) = view.as_object_mut() {
        o.insert("index.creation_date_string".into(), json!(text));
        o.entry("index.creation_date".to_string()).or_insert_with(|| json!(created.to_string()));
    }
}

pub(crate) fn settings_view(raw: &Value, name: Option<&str>, flat: bool) -> Value {
    let mut flat_map = serde_json::Map::new();
    flatten_settings(raw, "", &mut flat_map);
    // how many routing shards an index was made with is kept for routing,
    // not as a setting anyone set, and the reference does not list it
    flat_map.remove("index.velo_routing_shards");
    if let Some(name) = name
        && name != "_all"
        && name != "*"
    {
        let pats: Vec<regex::Regex> =
            name.split(',').map(|p| crate::store::wildcard_to_regex(p.trim())).collect();
        flat_map.retain(|k, _| pats.iter().any(|re| re.is_match(k)));
    }
    if flat {
        return Value::Object(flat_map);
    }
    // rebuild the nested shape from whatever survived the filter
    let mut nested = json!({});
    for (k, v) in flat_map {
        let mut cur = &mut nested;
        let segs: Vec<&str> = k.split('.').collect();
        for seg in &segs[..segs.len() - 1] {
            cur = entry_of(cur, seg, || json!({}));
        }
        if let Some(o) = cur.as_object_mut() {
            o.insert(segs[segs.len() - 1].to_string(), v);
        }
    }
    nested
}

pub async fn get_settings(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    settings_response(store, index.map(|Path(i)| i), None, p)
}

pub async fn get_settings_all_named(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    settings_response(store, None, Some(name), p)
}

pub async fn get_settings_named(
    State(store): State<Store>,
    Path((index, name)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    settings_response(store, Some(index), Some(name), p)
}

pub(crate) fn settings_response(
    store: Store,
    index: Option<String>,
    name: Option<String>,
    p: Params,
) -> Response {
    let expr = index.unwrap_or_else(|| "_all".into());
    let targets = store.resolve(&expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" && !ignore_unavailable(&p) {
        return no_such_index(&expr);
    }
    let flat = flag(&p, "flat_settings");
    let mut out = serde_json::Map::new();
    for n in targets {
        let Some(st) = store.get(&n) else { continue };
        let raw = st.read().effective_settings();
        let mut entry = json!({ "settings": settings_view(&raw, name.as_deref(), flat) });
        if flag(&p, "human") {
            add_human_settings(&mut entry["settings"], &st.read());
        }
        if flag(&p, "include_defaults") {
            entry["defaults"] = settings_view(&default_settings(), name.as_deref(), flat);
        }
        out.insert(n.clone(), entry);
    }
    // through `respond`, like every other answer: returned as bare JSON, it
    // skipped the one place `filter_path` is applied, and
    // `?filter_path=**.refresh_interval` answered with every setting
    respond(&p, Value::Object(out))
}

/// Settings an index takes only when it is made, or while it is closed.
///
/// A match is the setting itself or anything under it: `index.analysis`
/// covers every analyzer. What the reference refuses on an open index, and
/// on a closed one where it is final, was checked against it one setting at
/// a time.
const NOT_DYNAMIC: &[&str] = &[
    "index.number_of_shards",
    "index.number_of_routing_shards",
    "index.routing_partition_size",
    "index.codec",
    "index.soft_deletes.enabled",
    "index.sort",
    "index.analysis",
    "index.similarity",
    "index.store.type",
    "index.store.preload",
    "index.shard.check_on_startup",
    "index.replication.type",
    "index.creation_date",
    "index.queries.cache.enabled",
    "index.load_fixed_bitset_filters_eagerly",
    "index.knn",
    "index.format",
    "index.append_only.enabled",
];

/// The ones of those that stay as they were made, closed or not.
const FINAL: &[&str] = &[
    "index.number_of_shards",
    "index.soft_deletes.enabled",
    "index.sort",
    "index.replication.type",
    "index.knn",
    "index.append_only.enabled",
];

/// Settings the node keeps for itself, which no request may write.
const PRIVATE: &[&str] =
    &["index.uuid", "index.version.created", "index.version.upgraded", "index.remote_store"];

/// Whether `key` is `setting` or a setting under it.
fn is_under(key: &str, setting: &str) -> bool {
    key == setting || key.strip_prefix(setting).map(|rest| rest.starts_with('.')).unwrap_or(false)
}

/// Why a settings update may not be applied to these indices, if it may not.
///
/// Every refusal is decided before any index is changed, as the reference
/// decides them: a request that one index refuses changes none of them.
fn settings_refusal(store: &Store, targets: &[String], keys: &[String]) -> Option<Response> {
    if let Some(k) = keys.iter().find(|k| PRIVATE.iter().any(|s| is_under(k, s))) {
        return Some(err(
            StatusCode::BAD_REQUEST,
            "settings_exception",
            format!("can not update private setting [{k}]; this setting is managed by OpenSearch"),
        ));
    }
    // A block that stops an index's metadata changing stops its settings
    // changing too -- except the one change that lifts such a block, or
    // nobody could lift it. The reference allows exactly that: a request
    // naming one setting, and that setting one of these blocks.
    const LIFTS: &[&str] =
        &["index.blocks.read_only", "index.blocks.read_only_allow_delete", "index.blocks.metadata"];
    let lifting = keys.len() == 1 && LIFTS.contains(&keys[0].as_str());
    if !lifting {
        let mut reason = String::new();
        let mut status = 0u16;
        for n in targets {
            let Some(st) = store.get(n) else { continue };
            let blocks = st.read().metadata_blocks();
            if blocks.is_empty() {
                continue;
            }
            let names: Vec<&str> = blocks.iter().map(|(b, _)| *b).collect();
            reason.push_str(&format!("index [{n}] blocked by: [{}];", names.join(", ")));
            status = status.max(blocks.iter().map(|(_, s)| *s).max().unwrap_or(403));
        }
        if !reason.is_empty() {
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN);
            return Some(err(code, "cluster_block_exception", reason));
        }
    }
    let fixed: Vec<&String> =
        keys.iter().filter(|k| NOT_DYNAMIC.iter().any(|s| is_under(k, s))).collect();
    if fixed.is_empty() {
        return None;
    }
    let mut open = Vec::new();
    for n in targets {
        let Some(st) = store.get(n) else { continue };
        let g = st.read();
        if !g.closed {
            open.push(format!("[{}/{}]", g.name, g.uuid));
        }
    }
    if !open.is_empty() {
        let names: Vec<&str> = fixed.iter().map(|k| String::as_str(k)).collect();
        return Some(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!(
                "Can't update non dynamic settings [[{}]] for open indices [{}]",
                names.join(", "),
                open.join(", ")
            ),
        ));
    }
    // every target is closed: what may change on a closed index may change,
    // and a final setting may not change at all
    if let (Some(k), Some(n)) =
        (fixed.iter().find(|k| FINAL.iter().any(|s| is_under(k, s))), targets.first())
    {
        return Some(err(
            StatusCode::BAD_REQUEST,
            "settings_exception",
            format!("final {n} setting [{k}], not updateable"),
        ));
    }
    None
}

pub async fn put_settings(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let expr = index.map(|Path(i)| i).unwrap_or_else(|| "_all".into());
    let targets = store.resolve(&expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" && !ignore_unavailable(&p) {
        return no_such_index(&expr);
    }
    // a settings body may arrive wrapped in `settings`, wrapped in `index`, or flat
    let patch = body.get("settings").unwrap_or(&body);
    let patch = patch.get("index").unwrap_or(patch).clone();
    // every setting the request names, as `index.<dotted name>`
    let mut named = serde_json::Map::new();
    flatten_settings(&patch, "", &mut named);
    let keys: Vec<String> = named
        .keys()
        .map(|k| if k.starts_with("index.") { k.clone() } else { format!("index.{k}") })
        .collect();
    if let Some(refused) = settings_refusal(&store, &targets, &keys) {
        return refused;
    }
    // `preserve_existing` says to fill in only what is not already set
    let preserve = p.get("preserve_existing").map(|v| v != "false").unwrap_or(false);
    for n in targets {
        let Some(st) = store.get(&n) else { continue };
        let mut g = st.write();
        let mut settings = g.settings.clone();
        if !settings.is_object() {
            settings = json!({});
        }
        let mut patch = patch.clone();
        if preserve && let Some(o) = patch.as_object_mut() {
            // a key may be written with the `index.` prefix the setting
            // lookup adds for itself
            o.retain(|k, _| g.setting(k.strip_prefix("index.").unwrap_or(k)).is_none());
        }
        // A setting may be held in any of the shapes it was written in --
        // nested, dotted, with or without `index.` -- and a lookup takes the
        // first it finds. Writing a new value beside an old one in another
        // shape left the old one winning: `_block/write` wrote `blocks.write`,
        // `{"index.blocks.write": false}` was filed under another name, read
        // back as false, and writes were still refused. Each setting named
        // is taken out in every shape before the new value goes in, and a
        // null takes it out for good -- which is what brings the default back
        // rather than whatever the index was created with.
        let mut leaves = serde_json::Map::new();
        flatten_settings(&patch, "", &mut leaves);
        for key in leaves.keys() {
            crate::store::clear_index_setting(&mut settings, key);
        }
        let slot = entry_of(&mut settings, "index", || json!({}));
        crate::store::deep_merge(slot, &patch);
        for (key, value) in &leaves {
            if value.is_null() {
                crate::store::clear_index_setting(&mut settings, key);
                forget_setting(&mut settings, key.strip_prefix("index.").unwrap_or(key));
            }
        }
        g.settings = settings;
        g.refresh_knobs();
        g.apply_analysis();
        g.save_meta();
    }
    respond(&p, json!({"acknowledged": true}))
}
