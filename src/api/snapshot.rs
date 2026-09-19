//! Repositories and the snapshots they hold.

use super::*;

pub async fn put_repository(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if body.get("type").and_then(|t| t.as_str()).unwrap_or("").is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "repository_exception",
            format!("[{name}] missing repository type"),
        );
    }
    // a type nothing here can read is refused when it is registered, not at
    // the first snapshot: it was stored and listed as though it were a
    // repository
    let kind = body.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if !matches!(kind, "fs" | "url" | "s3" | "gcs" | "azure") {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "repository_exception",
            format!("[{name}] repository type [{kind}] does not exist"),
        );
    }
    // a location is a name under the root repositories live in; one that tries
    // to climb out of it is refused rather than quietly ignored
    if body.get("type").and_then(|t| t.as_str()) == Some("fs")
        && body.pointer("/settings/location").and_then(|v| v.as_str()).is_some()
        && crate::snapshot::location(&body).is_none()
    {
        // in the reference's words and shape: the registration failed, and
        // why is the cause
        let location = body.pointer("/settings/location").and_then(|v| v.as_str()).unwrap_or("");
        let why = json!({"type": "repository_exception", "reason": format!(
            "[{name}] location [{location}] doesn't match any of the locations specified by path.repo"
        )});
        let error = json!({"root_cause": [why.clone()], "type": "repository_exception",
                           "reason": format!("[{name}] failed to create repository"),
                           "caused_by": why});
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": error, "status": 500})),
        )
            .into_response();
    }
    // A repository read over a URL is one nothing writes to, and a cluster
    // will not read from anywhere it was not told it may: a `file://` URL has
    // to sit under the repository root, and any other has to be named in
    // `repositories.url.allowed_urls`.
    if let Some(url) = crate::snapshot::url::url_of(&body) {
        let allowed_urls = allowed_urls(&store);
        if !crate::snapshot::url::allowed(&url, &allowed_urls) {
            return err(
                StatusCode::BAD_REQUEST,
                "repository_exception",
                format!(
                    "[{name}] file url [{url}] doesn't match any of the locations \
                     specified by path.repo or repositories.url.allowed_urls"
                ),
            );
        }
    }
    // A repository that already holds snapshots says so as soon as it is
    // registered: the records are where the repository is, not in a cluster
    // state this server keeps across a restart. Which is also how a second
    // cluster reads what a first one wrote.
    if let Some(from) = crate::snapshot::Source::of(&body) {
        if let crate::snapshot::Source::Dir(dir) = &from {
            let _ = std::fs::create_dir_all(dir);
        }
        for (snap, record) in off_the_runtime(|| from.records()) {
            store.put_snapshot(&name, &snap, record);
        }
    }
    // A repository every node cannot reach is not a repository: the snapshot
    // taken into it succeeds, because each node truthfully writes what it
    // holds where it can see, and the restore is the first thing to find out
    // that the pieces were never in one place. So it is proved here, before
    // the name is registered and anything is written under it.
    if verify_asked(&p)
        && let Err(why) = verify_shared(&name, &body).await
    {
        return repository_verification_failed(&name, &why);
    }
    store.put_repository(&name, body);
    respond(&p, json!({"acknowledged": true}))
}

pub async fn get_repository(
    State(store): State<Store>,
    name: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let want = name.map(|Path(n)| n).unwrap_or_default();
    let all = store.repositories();
    let picked: serde_json::Map<String, Value> = all
        .into_iter()
        .filter(|(n, _)| {
            want.is_empty()
                || want.split(',').any(|w| {
                    let w = w.trim();
                    w == "_all" || w == "*" || w == n || crate::store::glob_match(w, n)
                })
        })
        .collect();
    if picked.is_empty() && !want.is_empty() && !want.contains('*') && want != "_all" {
        return err(
            StatusCode::NOT_FOUND,
            "repository_missing_exception",
            format!("[{want}] missing"),
        );
    }
    // the reference keeps a repository's settings as text and answers them
    // that way: `"compress": "true"`, not `true`
    let mut picked = picked;
    for repo in picked.values_mut() {
        if let Some(settings) = repo.get_mut("settings").and_then(|s| s.as_object_mut()) {
            for v in settings.values_mut() {
                if matches!(v, Value::Bool(_) | Value::Number(_)) {
                    *v = json!(v.to_string());
                }
            }
        }
    }
    respond(&p, Value::Object(picked))
}

pub async fn delete_repository(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    if store.remove_repository(&name) == 0 && !name.contains('*') {
        return err(
            StatusCode::NOT_FOUND,
            "repository_missing_exception",
            format!("[{name}] missing"),
        );
    }
    respond(&p, json!({"acknowledged": true}))
}

/// `POST /_snapshot/{repo}/_verify` -- every node proves it reaches the same
/// place, and the ones that did are the answer.
pub async fn verify_repository(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let Some(repo) = store.repositories().get(&name).cloned() else {
        return err(
            StatusCode::NOT_FOUND,
            "repository_missing_exception",
            format!("[{name}] missing"),
        );
    };
    match verify_shared(&name, &repo).await {
        Ok(nodes) => {
            let nodes: serde_json::Map<String, Value> =
                nodes.into_iter().map(|(id, node)| (id, json!({"name": node}))).collect();
            respond(&p, json!({"nodes": nodes}))
        }
        Err(why) => repository_verification_failed(&name, &why),
    }
}

/// A verification that did not hold, in the reference's shape: the
/// registration or the check failed, and why is the cause underneath it.
fn repository_verification_failed(name: &str, why: &str) -> Response {
    let cause = json!({"type": "repository_verification_exception", "reason": why});
    let error = json!({
        "root_cause": [cause.clone()],
        "type": "repository_verification_exception",
        "reason": format!("[{name}] cannot be verified"),
        "caused_by": cause,
    });
    (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": error, "status": 500})))
        .into_response()
}

/// `POST /_snapshot/{repo}/_cleanup` -- nothing is left behind here, so there
/// is nothing to sweep up.
pub async fn cleanup_repository(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    if !store.repositories().contains_key(&name) {
        return err(
            StatusCode::NOT_FOUND,
            "repository_missing_exception",
            format!("[{name}] missing"),
        );
    }
    respond(&p, json!({"results": {"deleted_bytes": 0, "deleted_blobs": 0}}))
}

pub(crate) fn snapshot_record(
    store: &Store,
    name: &str,
    indices: Vec<String>,
    streams: Vec<String>,
    global: bool,
) -> Value {
    let now = IdxState::now_iso();
    let shards: u64 =
        indices.iter().filter_map(|n| store.get(n)).map(|st| st.read().shard_count()).sum();
    json!({
        "snapshot": name,
        "uuid": crate::store::index_uuid(name),
        "version_id": 136_217_827,
        "version": "3.0.0",
        // this server keeps no remote store, so no snapshot of it is shallow
        "remote_store_index_shallow_copy": false,
        "indices": indices,
        "data_streams": streams,
        "include_global_state": global,
        "state": "SUCCESS",
        "start_time": now,
        "start_time_in_millis": 0,
        "end_time": now,
        "end_time_in_millis": 0,
        "duration_in_millis": 0,
        "failures": [],
        "shards": {"total": shards, "failed": 0, "successful": shards},
    })
}

/// The data streams a snapshot request reaches, as the reference records them.
///
/// A snapshot keeps the streams the caller asked for by name, not the streams
/// its indices happen to belong to: naming `.ds-logs-app-000001` outright
/// keeps that index and no stream, while naming `logs-app`, a pattern that
/// fits it, or nothing at all keeps the stream. That list is what a restore
/// puts the stream back from, so it is the difference between a restore that
/// gives back a data stream and one that gives back two loose indices.
fn streams_asked_for(store: &Store, asked: Option<&str>) -> Vec<String> {
    let held = store.data_streams();
    let mut out: Vec<String> = match asked {
        Some(expr) => held
            .keys()
            .filter(|name| {
                expr.split(',').map(|s| s.trim()).any(|part| {
                    part == "_all" || part == *name || crate::store::glob_match(part, name)
                })
            })
            .cloned()
            .collect(),
        None => held.keys().cloned().collect(),
    };
    out.sort();
    out
}

/// The data streams a snapshot record says it holds.
fn held_streams(record: &Value) -> Vec<String> {
    record["data_streams"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default()
}

/// A repository's work, done here.
///
/// Writing a snapshot or reading one back is a whole index over a network or
/// a disk, and it runs on the thread that is answering the request -- one of
/// the runtime's. Handing it to `block_in_place` was tried and taken out
/// again: moving the worker out of the runtime and waiting for a replacement
/// left the node not accepting connections for seconds at a time, which is a
/// worse fault than the one it was meant to fix. What bounds the damage is
/// that every call this makes now has a timeout on it; doing the work
/// somewhere else is a larger change than a review can carry.
fn off_the_runtime<R>(f: impl FnOnce() -> R) -> R {
    f()
}

/// Read again what a repository nothing writes to has come to hold.
///
/// A repository read over a URL is written to by somebody else -- that is the
/// whole point of one -- so what it holds is looked at when it is asked about
/// rather than remembered from the moment it was registered.
fn refresh_readonly(store: &Store, repo: &str) {
    let Some(found) = store.repositories().get(repo).cloned() else { return };
    // A directory this node writes to is already known, and anywhere else may
    // have been written to by somebody else since it was last looked at.
    //
    // Unless this node has never read the directory, which is where a
    // cluster manager that has just taken over from another finds itself: it
    // has the repository, because that is cluster metadata, and none of the
    // records of what is in it, because those are the repository's own and
    // the publication that would have carried them may never have gone out.
    if crate::snapshot::location(&found).is_some() && !store.snapshots(repo).is_empty() {
        return;
    }
    let Some(from) = crate::snapshot::Source::of(&found) else { return };
    // Reading it means going over the network, and the answer may be a long
    // time coming: the client has timeouts now, but a runtime thread spent
    // waiting on a repository is a thread not answering anybody. This tells
    // the runtime to carry on without it.
    let records = off_the_runtime(|| from.records());
    for (snap, record) in records {
        store.put_snapshot(repo, &snap, record);
    }
}

/// Where this node is willing to read a repository from.
///
/// A cluster setting says so if one was set, and the node's own configuration
/// says so otherwise -- which is where OpenSearch reads it from as well, its
/// `repositories.url.allowed_urls` being a node setting rather than a cluster
/// one. A pattern may end in `*`.
pub(crate) fn allowed_urls(store: &Store) -> Vec<String> {
    let listed = |v: Value| -> Vec<String> {
        match v {
            Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect(),
            Value::String(one) => one.split(',').map(|s| s.trim().to_string()).collect(),
            _ => Vec::new(),
        }
    };
    // A cluster setting says so where one names anything. OpenSearch has no
    // such cluster setting -- `repositories.url.allowed_urls` is a node
    // setting -- so this is ours, and it may only add to what the node was
    // started with. Returning the setting whatever it held meant a
    // `null` written over it (which is what clearing the cluster settings
    // writes) hid the node's own list, and every URL repository the node was
    // configured for stopped being registrable.
    if let Some(v) = store.cluster_setting("repositories.url.allowed_urls") {
        let named = listed(v);
        if !named.is_empty() {
            return named;
        }
    }
    std::env::var("VELOSEARCH_URL_ALLOWED")
        .ok()
        .map(|v| listed(Value::String(v)))
        .unwrap_or_default()
}

/// A repository read over a URL, or one told it is read-only, refuses to be
/// written to -- and says so in the words OpenSearch says it in.
fn refuse_if_readonly(store: &Store, repo: &str) -> Option<Response> {
    let found = store.repositories().get(repo).cloned()?;
    let readonly = crate::snapshot::url::url_of(&found).is_some()
        || found
            .pointer("/settings/readonly")
            .and_then(|v| v.as_bool().or_else(|| v.as_str().map(|s| s == "true")))
            .unwrap_or(false);
    readonly.then(|| {
        err(
            StatusCode::BAD_REQUEST,
            "repository_exception",
            format!("[{repo}] cannot delete snapshot from a readonly repository"),
        )
    })
}

/// A snapshot name that could name a path is refused, with OpenSearch's own
/// words: a name is a segment under the repository, never a route out of it.
fn bad_snapshot_name(repo: &str, name: &str) -> Option<Response> {
    let bad = name.is_empty()
        || name.starts_with('_')
        || name.chars().any(|c| c.is_whitespace() || "\\/*?\"<>|,#".contains(c))
        || name == "."
        || name == ".."
        || name.chars().any(|c| c.is_uppercase());
    bad.then(|| {
        err(
            StatusCode::BAD_REQUEST,
            "invalid_snapshot_name_exception",
            format!("[{repo}:{name}] Invalid snapshot name [{name}], must be lowercase, must not contain whitespace, \
                     must not contain '\\', '/', '*', '?', '\"', '<', '>', '|', ',', '#', and must not start with '_'"),
        )
    })
}

/// A name looked up or deleted may be a pattern or a list of names, so it is
/// not held to what a name that is written must be; it is still not allowed
/// to be a path.
fn bad_snapshot_lookup(repo: &str, names: &str) -> Option<Response> {
    names
        .split(',')
        .find(|one| one.contains(['/', '\\']) || *one == "." || *one == "..")
        .and_then(|one| bad_snapshot_name(repo, one))
}

/// The action a node is asked to write shards of a snapshot by.
pub const SNAPSHOT_SHARDS: &str = "internal:cluster/snapshot/shards";
pub const REPOSITORY_VERIFY: &str = "internal:cluster/repository/verify";

/// Whether a repository has to be proved shared before it is registered.
///
/// The reference verifies on registration unless it is told not to, under the
/// same name: `?verify=false`.
fn verify_asked(p: &Params) -> bool {
    !matches!(p.get("verify").map(|v| v.trim()), Some("false") | Some("0"))
}

/// Ask every node to leave a blob in the repository, then read them all back
/// from here.
///
/// Each node writes `tests-<token>/<node id>.dat` holding the token, and this
/// node reads every one of them out of its own view of the repository. A node
/// that cannot reach the repository at all says so; a node whose blob cannot
/// be read back from here reached a different place, which is the case that
/// was passing silently. The directory is thrown away afterwards either way.
///
/// A repository read over a URL is one nothing writes to, and reaching it is
/// the whole of what can be checked, so no blob is written or looked for.
async fn verify_shared(name: &str, repo: &Value) -> Result<Vec<(String, String)>, String> {
    let Some(here) = crate::snapshot::Source::of(repo) else {
        return Err(format!(
            "[{name}] is not a repository this node can reach: its location is not under this \
             node's path.repo"
        ));
    };
    let state = crate::cluster::current_state();
    let me = crate::cluster::runtime().map(|rt| rt.local());
    let mut nodes: Vec<(crate::cluster::NodeId, String)> =
        state.nodes.iter().map(|(id, n)| (id.clone(), n.name.clone())).collect();
    // a node that is the whole of its cluster is still asked: a path.repo it
    // cannot write to is worth hearing about now rather than at the restore
    if nodes.is_empty() {
        let me = crate::cluster::identity();
        nodes.push((me.id.clone(), me.name.clone()));
    }
    let token = crate::cluster::NodeId::random().as_str().to_string();
    let dir = format!("tests-{token}");
    let outcome = verify_round(&here, repo, &dir, &token, &nodes, me.as_ref()).await;
    off_the_runtime(|| here.remove_prefix(&dir));
    outcome
}

/// One round of the above: every node writes, then this one reads.
async fn verify_round(
    here: &crate::snapshot::Source,
    repo: &Value,
    dir: &str,
    token: &str,
    nodes: &[(crate::cluster::NodeId, String)],
    me: Option<&crate::cluster::NodeId>,
) -> Result<Vec<(String, String)>, String> {
    let mut waits = Vec::new();
    for (id, _) in nodes {
        if Some(id) == me {
            continue;
        }
        let Some(rt) = crate::cluster::runtime() else { continue };
        let id = id.clone();
        let body = json!({"repository": repo, "dir": dir, "token": token});
        waits.push(tokio::spawn(async move {
            let answer = rt
                .call(
                    &id,
                    REPOSITORY_VERIFY,
                    body.to_string().into_bytes(),
                    std::time::Duration::from_secs(30),
                )
                .await;
            let wrote = match answer {
                None => Err("did not answer".to_string()),
                Some(e) if e.kind == crate::cluster::transport::Kind::Error => {
                    Err(String::from_utf8_lossy(&e.body).into_owned())
                }
                Some(_) => Ok(()),
            };
            (id, wrote)
        }));
    }
    // this node's own blob, written the way every other node writes its own
    let mine = me.map(|id| id.as_str().to_string()).unwrap_or_else(|| {
        nodes.first().map(|(id, _)| id.as_str().to_string()).unwrap_or_default()
    });
    let wrote_mine = off_the_runtime(|| write_verify_blob(here, dir, token, &mine));
    let mut failure = wrote_mine.err();
    for w in waits {
        if let Ok((id, Err(why))) = w.await {
            failure.get_or_insert(format!("node [{}] {why}", id.as_str()));
        }
    }
    if let Some(why) = failure {
        return Err(why);
    }
    let seen: Vec<(String, String)> =
        nodes.iter().map(|(id, n)| (id.as_str().to_string(), n.clone())).collect();
    if !here.writable() {
        return Ok(seen);
    }
    // the half that catches a location which resolved somewhere else: what
    // the other nodes wrote, read through this node's own view
    for (id, _) in nodes {
        let blob = format!("{dir}/{}.dat", id.as_str());
        let read = off_the_runtime(|| here.read(&blob));
        if read.as_deref() != Some(token.as_bytes()) {
            return Err(format!(
                "node [{}] left its mark in the repository and this node cannot read it back: \
                 the repository is not one place every node of the cluster reaches",
                id.as_str()
            ));
        }
    }
    Ok(seen)
}

/// A node's own blob, or why it could not leave one.
fn write_verify_blob(
    to: &crate::snapshot::Source,
    dir: &str,
    token: &str,
    node: &str,
) -> Result<(), String> {
    if !to.writable() {
        return Ok(());
    }
    to.write(&format!("{dir}/{node}.dat"), token.as_bytes())
        .map_err(|e| format!("could not write into the repository: {e}"))
}

/// The committed state of a cluster this node is one of several nodes in;
/// `None` for a node that is the whole of its cluster, whose own store is
/// everything there is.
fn clustered() -> Option<crate::cluster::state::ClusterState> {
    crate::cluster::runtime()?;
    let state = crate::cluster::current_state();
    (state.version > 0 && state.nodes.len() > 1).then_some(state)
}

/// Which node writes which primary shards of a snapshot.
struct ShardPlan {
    /// every index in the snapshot, and how many primary shards it has
    shards: std::collections::BTreeMap<String, u32>,
    /// the work of each node, by index: `None` is this node
    work: Vec<(Option<crate::cluster::NodeId>, String, Vec<u32>)>,
    /// the primaries no node can be asked for, and why
    unassigned: Vec<(String, u32, String)>,
}

fn plan_shards(
    store: &Store,
    cluster: Option<&crate::cluster::state::ClusterState>,
    indices: &[String],
) -> ShardPlan {
    use crate::cluster::state::ShardState;
    let mut plan =
        ShardPlan { shards: Default::default(), work: Vec::new(), unassigned: Vec::new() };
    let me = crate::cluster::runtime().map(|rt| rt.local());
    for index in indices {
        let Some(state) = cluster else {
            let count = store.get(index).map(|st| st.read().shard_count().max(1) as u32);
            if let Some(count) = count {
                plan.shards.insert(index.clone(), count);
                plan.work.push((None, index.clone(), (0..count).collect()));
            }
            continue;
        };
        let Some(meta) = state.indices.get(index) else { continue };
        let count = meta.number_of_shards.max(1);
        plan.shards.insert(index.clone(), count);
        let mut by_node: std::collections::BTreeMap<crate::cluster::NodeId, Vec<u32>> =
            Default::default();
        for shard in 0..count {
            // the primary is what a snapshot is taken from; one being moved
            // still answers from where it is until its target takes over
            match state.routing.primary(index, shard).and_then(|p| {
                matches!(p.state, ShardState::Started | ShardState::Relocating)
                    .then(|| p.node.clone())
                    .flatten()
            }) {
                Some(node) => by_node.entry(node).or_default().push(shard),
                None => plan.unassigned.push((
                    index.clone(),
                    shard,
                    "primary shard is not allocated".to_string(),
                )),
            }
        }
        for (node, shards) in by_node {
            let node = (Some(&node) != me.as_ref()).then_some(node);
            plan.work.push((node, index.clone(), shards));
        }
    }
    plan
}

/// Ask every node in a plan to write its shards, this one included, all at
/// once; what each answered, or why it did not.
async fn run_shard_plan(
    store: &Store,
    to: &crate::snapshot::Source,
    repo: &Value,
    snapshot: &str,
    plan: &ShardPlan,
) -> Vec<(Option<String>, String, Vec<u32>, Result<Value, String>)> {
    let mut waits = Vec::new();
    let mut out = Vec::new();
    for (node, index, shards) in &plan.work {
        let (Some(node), Some(rt)) = (node, crate::cluster::runtime()) else { continue };
        let body =
            json!({"repository": repo, "snapshot": snapshot, "index": index, "shards": shards});
        let (node, index, shards) = (node.clone(), index.clone(), shards.clone());
        waits.push(tokio::spawn(async move {
            // a large shard is a long write: the wait is for a node that has
            // stopped answering, not for one that is busy
            let answer = rt
                .call(
                    &node,
                    SNAPSHOT_SHARDS,
                    body.to_string().into_bytes(),
                    std::time::Duration::from_secs(3600),
                )
                .await;
            let result = match answer {
                None => Err(format!("node [{}] did not answer", node.as_str())),
                Some(e) if e.kind == crate::cluster::transport::Kind::Error => {
                    Err(String::from_utf8_lossy(&e.body).into_owned())
                }
                Some(e) => serde_json::from_slice::<Value>(&e.body)
                    .map_err(|_| format!("node [{}] answered nonsense", node.as_str())),
            };
            (Some(node.as_str().to_string()), index, shards, result)
        }));
    }
    let me = crate::cluster::runtime().map(|rt| rt.local().as_str().to_string());
    for (node, index, shards) in &plan.work {
        if node.is_some() {
            continue;
        }
        let result =
            off_the_runtime(|| crate::snapshot::write_shards(store, to, snapshot, index, shards));
        out.push((me.clone(), index.clone(), shards.clone(), result));
    }
    for w in waits {
        if let Ok(answer) = w.await {
            out.push(answer);
        }
    }
    out
}

/// A node's side of a snapshot: write the shards it was asked for, from the
/// primaries it holds, into the repository every node shares.
pub fn snapshot_install(store: Store) {
    use crate::cluster::runtime::DataFuture;
    use crate::cluster::transport::Envelope;
    let Some(rt) = crate::cluster::runtime() else { return };
    let me = rt.local();
    {
        // the node's side of a verification: reach the repository, leave the
        // mark it was asked for, and say so. Whether the mark can be read
        // from anywhere else is not this node's question.
        let me = me.clone();
        rt.register(
            REPOSITORY_VERIFY,
            std::sync::Arc::new(move |e: Envelope| -> DataFuture {
                let me = me.clone();
                Box::pin(async move {
                    let state = crate::cluster::current_state();
                    if e.from != me && !state.nodes.contains_key(&e.from) {
                        return e.error(me, "not a node of this cluster");
                    }
                    let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                    let Some(to) = crate::snapshot::Source::of(&v["repository"]) else {
                        return e.error(
                            me,
                            "cannot reach the repository: its location is not under this node's \
                             path.repo",
                        );
                    };
                    let dir = v["dir"].as_str().unwrap_or("").to_string();
                    let token = v["token"].as_str().unwrap_or("").to_string();
                    let node = me.as_str().to_string();
                    let wrote = tokio::task::spawn_blocking(move || {
                        write_verify_blob(&to, &dir, &token, &node)
                    })
                    .await
                    .unwrap_or_else(|e| Err(format!("the verification panicked: {e}")));
                    match wrote {
                        Ok(()) => e.response(me, b"{}".to_vec()),
                        Err(why) => e.error(me, &why),
                    }
                })
            }),
        );
    }
    rt.register(
        SNAPSHOT_SHARDS,
        std::sync::Arc::new(move |e: Envelope| -> DataFuture {
            let store = store.clone();
            let me = me.clone();
            Box::pin(async move {
                let state = crate::cluster::current_state();
                if e.from != me && !state.nodes.contains_key(&e.from) {
                    return e.error(me, "not a node of this cluster");
                }
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                let snapshot = v["snapshot"].as_str().unwrap_or("").to_string();
                let index = v["index"].as_str().unwrap_or("").to_string();
                let asked: Vec<u32> = v["shards"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|s| s.as_u64()).map(|s| s as u32).collect())
                    .unwrap_or_default();
                let Some(to) = crate::snapshot::Source::of(&v["repository"]) else {
                    return e.error(
                        me,
                        "the repository cannot be reached from this node: its location is not \
                         under this node's path.repo",
                    );
                };
                // only a primary this node holds is written from here: a copy
                // that has just stopped being one may be behind the one that is
                let mut failed = serde_json::Map::new();
                let mine: Vec<u32> = asked
                    .into_iter()
                    .filter(|shard| {
                        let held = state.routing.primary(&index, *shard).is_some_and(|p| {
                            p.node.as_ref() == Some(&me)
                                && matches!(
                                    p.state,
                                    crate::cluster::state::ShardState::Started
                                        | crate::cluster::state::ShardState::Relocating
                                )
                        });
                        if !held {
                            failed.insert(
                                shard.to_string(),
                                json!("this node no longer holds the primary"),
                            );
                        }
                        held
                    })
                    .collect();
                let result = tokio::task::spawn_blocking(move || {
                    crate::snapshot::write_shards(&store, &to, &snapshot, &index, &mine)
                })
                .await
                .unwrap_or_else(|e| Err(format!("the snapshot of the shards panicked: {e}")));
                match result {
                    Ok(mut answer) => {
                        answer["failed"] = Value::Object(failed);
                        e.response(me, answer.to_string().into_bytes())
                    }
                    Err(why) => e.error(me, &why),
                }
            })
        }),
    );
}

pub async fn create_snapshot(
    State(store): State<Store>,
    Path((repo, name)): Path<(String, String)>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    if let Some(refused) = bad_snapshot_name(&repo, &name) {
        return refused;
    }
    if !store.repositories().contains_key(&repo) {
        return err(
            StatusCode::NOT_FOUND,
            "repository_missing_exception",
            format!("[{repo}] missing"),
        );
    }
    if let Some(r) = refuse_if_readonly(&store, &repo) {
        return r;
    }
    // a snapshot is written under its name, so taking one under a name the
    // repository already holds writes over the older snapshot's files while
    // its record still says it is there: what was kept is gone, and nothing
    // said so
    refresh_readonly(&store, &repo);
    if store.snapshots(&repo).contains_key(&name) {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_snapshot_name_exception",
            format!(
                "[{repo}:{name}] Invalid snapshot name [{name}], snapshot with the same name \
                 already exists"
            ),
        );
    }
    let body: Value = parse_body(&body).unwrap_or_else(|_| json!({}));
    let asked = match body.get("indices") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(a)) => Some(
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
                .join(","),
        ),
        _ => None,
    };
    // On a cluster the indices are the cluster's, not whichever of them the
    // node answering this request happens to hold a copy of: naming an index
    // kept on other nodes was answered 404, and a snapshot of everything
    // left it out and said SUCCESS.
    let cluster = clustered();
    let resolve = |expr: &str| -> Vec<String> {
        match &cluster {
            Some(state) => crate::api::cluster_resolve(&store, expr)
                .into_iter()
                .filter(|n| state.indices.contains_key(n))
                .collect(),
            None => store.resolve(expr),
        }
    };
    let indices = match asked.as_deref() {
        Some(expr) => {
            // an index named outright has to be there to be kept
            // `ignore_unavailable` may be asked for in the body as well as
            // on the path
            let lenient = ignore_unavailable(&p)
                || body.get("ignore_unavailable").and_then(|v| v.as_bool()).unwrap_or(false);
            for part in expr.split(',').map(|s| s.trim()).filter(|s| !s.contains('*')) {
                if resolve(part).is_empty() && !lenient {
                    return no_such_index(part);
                }
            }
            resolve(expr)
        }
        None => match &cluster {
            Some(state) => state.indices.keys().cloned().collect(),
            None => store.names(),
        },
    };
    let global = body.get("include_global_state").and_then(|v| v.as_bool()).unwrap_or(true);
    let partial = body.get("partial").and_then(|v| v.as_bool()).unwrap_or(false);
    let streams = streams_asked_for(&store, asked.as_deref());
    let mut record = snapshot_record(&store, &name, indices.clone(), streams, global);
    // whatever the caller attached to the snapshot travels with it
    if let Some(meta) = body.get("metadata") {
        record["metadata"] = meta.clone();
    }
    // a repository nothing can be written to cannot hold a snapshot. It used
    // to keep the record and warn: the snapshot read back as SUCCESS, and a
    // restore from it answered 200 having restored nothing at all
    let Some(to) = store.repositories().get(&repo).and_then(crate::snapshot::Source::of) else {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "repository_exception",
            format!(
                "[{repo}] has nowhere to write snapshot [{name}]: the repository has no usable \
                 location"
            ),
        );
    };
    let plan = plan_shards(&store, cluster.as_ref(), &indices);
    // A primary with nowhere to be read from is a snapshot that cannot be
    // whole, and the reference refuses it before it starts unless it was
    // asked for what there is.
    if !partial && !plan.unassigned.is_empty() {
        let mut missing: Vec<&str> = plan.unassigned.iter().map(|(i, _, _)| i.as_str()).collect();
        missing.dedup();
        let uuid = record["uuid"].as_str().unwrap_or("_na_");
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "snapshot_exception",
            format!(
                "[{repo}:{name}/{uuid}] Indices don't have primary shards [{}]",
                missing.join(", ")
            ),
        );
    }
    let started = crate::snapshot::now_millis();
    let repo_def = store.repositories().get(&repo).cloned().unwrap_or(Value::Null);
    let answers = run_shard_plan(&store, &to, &repo_def, &name, &plan).await;
    // every shard is accounted for: written, or failed with the reason
    let mut failures: Vec<Value> = Vec::new();
    let mut written_meta: std::collections::BTreeMap<String, Value> = Default::default();
    let mut written: std::collections::BTreeMap<String, serde_json::Map<String, Value>> =
        Default::default();
    let mut failed: std::collections::BTreeMap<String, serde_json::Map<String, Value>> =
        Default::default();
    let mut fail = |index: &str, shard: u32, node: Option<&str>, reason: String| {
        failed.entry(index.to_string()).or_default().insert(shard.to_string(), json!(reason));
        failures.push(json!({
            "index": index, "index_uuid": index, "shard_id": shard, "reason": reason,
            "node_id": node, "status": "INTERNAL_SERVER_ERROR",
        }));
    };
    for (index, shard, why) in &plan.unassigned {
        fail(index, *shard, None, why.clone());
    }
    for (node, index, shards, answer) in answers {
        match answer {
            Ok(v) => {
                let done = v.get("shards").and_then(|s| s.as_object()).cloned().unwrap_or_default();
                for shard in &shards {
                    match done.get(&shard.to_string()) {
                        Some(stats) => {
                            written
                                .entry(index.clone())
                                .or_default()
                                .insert(shard.to_string(), stats.clone());
                        }
                        None => {
                            let why = v
                                .pointer(&format!("/failed/{shard}"))
                                .and_then(|w| w.as_str())
                                .unwrap_or("the shard was not written")
                                .to_string();
                            fail(&index, *shard, node.as_deref(), why);
                        }
                    }
                }
                if let Some(meta) = v.get("meta") {
                    written_meta.entry(index.clone()).or_insert_with(|| meta.clone());
                }
            }
            Err(why) => {
                for shard in &shards {
                    fail(&index, *shard, node.as_deref(), why.clone());
                }
            }
        }
    }
    let write_rest = || -> std::io::Result<()> {
        for (index, count) in &plan.shards {
            // an index no shard of which could be written is still described,
            // so what it is missing can be said -- by `_status`, and by the
            // restore that refuses it
            let meta = written_meta
                .get(index)
                .cloned()
                .unwrap_or_else(|| json!({"name": index, "number_of_shards": count}));
            let meta = &meta;
            let empty = serde_json::Map::new();
            crate::snapshot::write_index_meta(
                &to,
                &name,
                index,
                meta,
                written.get(index).unwrap_or(&empty),
                failed.get(index).unwrap_or(&empty),
            )?;
        }
        if global {
            crate::snapshot::write_global(&to, &name, &crate::snapshot::global_state(&store))?;
        }
        Ok(())
    };
    let total: u64 = plan.shards.values().map(|n| *n as u64).sum();
    let failed_count = failures.len() as u64;
    record["shards"] =
        json!({"total": total, "failed": failed_count, "successful": total - failed_count});
    record["state"] = json!(if failed_count == 0 {
        "SUCCESS"
    } else if failed_count == total {
        "FAILED"
    } else {
        "PARTIAL"
    });
    failures.sort_by_key(|f| (f["index"].to_string(), f["shard_id"].as_u64()));
    record["failures"] = json!(failures);
    let ended = crate::snapshot::now_millis();
    record["start_time_in_millis"] = json!(started);
    record["end_time_in_millis"] = json!(ended);
    record["duration_in_millis"] = json!(ended.saturating_sub(started));
    if let Err(e) =
        off_the_runtime(write_rest).and_then(|_| crate::snapshot::write_record(&to, &name, &record))
    {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "repository_exception",
            format!("[{repo}] could not write snapshot [{name}]: {e}"),
        );
    }
    store.put_snapshot(&repo, &name, record.clone());
    // without `wait_for_completion` the caller is told it has begun; with it,
    // the finished snapshot comes back
    if p.get("wait_for_completion").map(|v| v != "false").unwrap_or(false) {
        respond(&p, json!({"snapshot": record}))
    } else {
        respond(&p, json!({"accepted": true}))
    }
}

/// The snapshots a name or pattern reaches, and whether anything named
/// outright was missing.
pub(crate) fn pick_snapshots(
    store: &Store,
    repo: &str,
    want: &str,
) -> (Vec<Value>, Option<String>) {
    let (found, missing) = pick_from_memory(store, repo, want);
    let Some(gone) = missing else { return (found, None) };
    // A name this node does not know may still be in the repository: what is
    // there is the repository's own record of it, and this node may never
    // have read the directory -- or may have read it before that snapshot was
    // written, by another node or by the manager that was here before.
    let Some(from) = store.repositories().get(repo).and_then(crate::snapshot::Source::of) else {
        return (found, Some(gone));
    };
    let mut anything_new = false;
    for (snap, record) in off_the_runtime(|| from.records()) {
        if !store.snapshots(repo).contains_key(&snap) {
            store.put_snapshot(repo, &snap, record);
            anything_new = true;
        }
    }
    if !anything_new {
        return (found, Some(gone));
    }
    pick_from_memory(store, repo, want)
}

/// The records this node holds for a repository that the request names.
fn pick_from_memory(store: &Store, repo: &str, want: &str) -> (Vec<Value>, Option<String>) {
    let held = store.snapshots(repo);
    let mut out = Vec::new();
    let mut missing = None;
    for part in want.split(',').map(|s| s.trim()) {
        if part == "_all" || part == "*" || part.contains('*') {
            for (n, v) in held.iter() {
                if part == "_all" || part == "*" || crate::store::glob_match(part, n) {
                    out.push(v.clone());
                }
            }
            continue;
        }
        match held.get(part) {
            Some(v) => out.push(v.clone()),
            None => missing = Some(part.to_string()),
        }
    }
    out.sort_by(|a, b| a["snapshot"].as_str().cmp(&b["snapshot"].as_str()));
    (out, missing)
}

pub async fn get_snapshot(
    State(store): State<Store>,
    Path((repo, name)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    if let Some(refused) = bad_snapshot_lookup(&repo, &name) {
        return refused;
    }
    if !store.repositories().contains_key(&repo) {
        return err(
            StatusCode::NOT_FOUND,
            "repository_missing_exception",
            format!("[{repo}] missing"),
        );
    }
    refresh_readonly(&store, &repo);
    let (mut found, missing) = pick_snapshots(&store, &repo, &name);
    if let Some(gone) = missing
        && !ignore_unavailable(&p)
    {
        return err(
            StatusCode::NOT_FOUND,
            "snapshot_missing_exception",
            format!("[{repo}:{gone}] is missing"),
        );
    }
    // `verbose: false` asks only for what a listing needs
    if p.get("verbose").map(|v| v == "false").unwrap_or(false) {
        for s in found.iter_mut() {
            let short = json!({
                "snapshot": s["snapshot"].clone(),
                "uuid": s["uuid"].clone(),
                "state": s["state"].clone(),
                "indices": s["indices"].clone(),
                "data_streams": s["data_streams"].clone(),
            });
            *s = short;
        }
    }
    respond(&p, json!({"snapshots": found}))
}

pub async fn delete_snapshot(
    State(store): State<Store>,
    Path((repo, name)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    if let Some(refused) = bad_snapshot_lookup(&repo, &name) {
        return refused;
    }
    // a repository that is not there has no snapshots to delete, and says
    // it is missing rather than acknowledging a delete of nothing
    if !store.repositories().contains_key(&repo) {
        return err(
            StatusCode::NOT_FOUND,
            "repository_missing_exception",
            format!("[{repo}] missing"),
        );
    }
    refresh_readonly(&store, &repo);
    // a snapshot that was never there is missing whoever asked: only one that
    // is really held runs into the repository being read-only
    let exists =
        store.snapshots(&repo).keys().any(|n| *n == name || crate::store::glob_match(&name, n));
    if exists && let Some(r) = refuse_if_readonly(&store, &repo) {
        return r;
    }
    // what the repository was keeping goes with the record of it
    let held: Vec<String> = store
        .snapshots(&repo)
        .keys()
        .filter(|n| **n == name || crate::store::glob_match(&name, n))
        .cloned()
        .collect();
    if store.remove_snapshots(&repo, &name) == 0 && !name.contains('*') {
        return err(
            StatusCode::NOT_FOUND,
            "snapshot_missing_exception",
            format!("[{repo}:{name}] is missing"),
        );
    }
    if let Some(from) = store.repositories().get(&repo).and_then(crate::snapshot::Source::of) {
        for snap in held {
            crate::snapshot::remove(&from, &snap);
        }
    }
    respond(&p, json!({"acknowledged": true}))
}

/// `/_snapshot/_status` and `/_snapshot/{repo}/_status` -- the snapshots
/// running now, of which there are never any: a snapshot here finishes before
/// its request answers.
pub async fn snapshot_status_running(
    _repo: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    respond(&p, json!({"snapshots": []}))
}

pub async fn snapshot_status(
    State(store): State<Store>,
    Path((repo, name)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    status_answer(&store, &p, &repo, &name, None)
}

/// The same, with the indices of the snapshot to report named.
pub async fn snapshot_status_index(
    State(store): State<Store>,
    Path((repo, name, indices)): Path<(String, String, String)>,
    Query(p): Query<Params>,
) -> Response {
    status_answer(&store, &p, &repo, &name, Some(&indices))
}

fn status_answer(
    store: &Store,
    p: &Params,
    repo: &str,
    name: &str,
    indices: Option<&str>,
) -> Response {
    let (found, missing) = pick_snapshots(store, repo, name);
    if let Some(gone) = missing
        && !ignore_unavailable(p)
    {
        return err(
            StatusCode::NOT_FOUND,
            "snapshot_missing_exception",
            format!("[{repo}:{gone}] is missing"),
        );
    }
    // the indices are named exactly: a pattern is not expanded against a
    // snapshot, so one that names nothing in it is a missing index
    let only: Option<Vec<String>> =
        indices.map(|e| e.split(',').map(|s| s.trim().to_string()).collect());
    if let Some(only) = &only
        && !ignore_unavailable(p)
    {
        for s in &found {
            let held: Vec<String> = s["indices"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).map(String::from).collect())
                .unwrap_or_default();
            if let Some(gone) = only.iter().find(|n| !held.contains(n)) {
                let snapshot = s["snapshot"].as_str().unwrap_or("");
                // the reason names the first one and says there may be more,
                // which is how the reference names a set it stopped checking
                let named = format!("{gone} and possibly more indices");
                let reason = format!("no such index [{named}]");
                let about = json!({
                    "type": "index_not_found_exception", "reason": reason,
                    "index": named, "index_uuid": "_na_",
                });
                let mut error = about.clone();
                error["root_cause"] = json!([about]);
                error["caused_by"] = json!({
                    "type": "illegal_argument_exception",
                    "reason": format!(
                        "indices [{named}] missing in snapshot [{snapshot}] of repository [{repo}]"
                    ),
                });
                return (StatusCode::NOT_FOUND, axum::Json(json!({"error": error, "status": 404})))
                    .into_response();
            }
        }
    }
    let from = store.repositories().get(repo).and_then(crate::snapshot::Source::of);
    let out: Vec<Value> = off_the_runtime(|| {
        found.into_iter().map(|s| status_of(repo, from.as_ref(), &s, only.as_deref())).collect()
    });
    respond(p, json!({"snapshots": out}))
}

/// The status of a finished snapshot, shard by shard, from what its
/// repository holds: each shard wrote one file, and a shard that could not be
/// written is counted failed with the reason it gave.
fn status_of(
    repo: &str,
    from: Option<&crate::snapshot::Source>,
    s: &Value,
    only: Option<&[String]>,
) -> Value {
    let started = s["start_time_in_millis"].as_u64().unwrap_or(0);
    let took = s["duration_in_millis"].as_u64().unwrap_or(0);
    let stats = |files: u64, bytes: u64, start: u64, time: u64| {
        json!({
            "incremental": {"file_count": files, "size_in_bytes": bytes},
            "total": {"file_count": files, "size_in_bytes": bytes},
            "start_time_in_millis": start,
            "time_in_millis": time,
        })
    };
    let counts = |done: u64, failed: u64| {
        json!({"initializing": 0, "started": 0, "finalizing": 0,
               "done": done, "failed": failed, "total": done + failed})
    };
    let snapshot = s["snapshot"].as_str().unwrap_or("");
    let mut names: Vec<String> = s["indices"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if let Some(only) = only {
        names.retain(|n| only.iter().any(|o| o == n));
    }
    let (mut all_done, mut all_failed, mut all_files, mut all_bytes) = (0u64, 0u64, 0u64, 0u64);
    let mut indices = serde_json::Map::new();
    for index in &names {
        let found = from.and_then(|f| crate::snapshot::shard_stats(f, snapshot, index));
        let described = found.as_ref().map(|(_, n)| *n);
        let meta = found.map(|(m, _)| m);
        let count = meta
            .as_ref()
            .and_then(|m| m.get("number_of_shards").and_then(|v| v.as_u64()))
            .unwrap_or(1);
        let mut shards = serde_json::Map::new();
        let (mut done, mut failed, mut bytes) = (0u64, 0u64, 0u64);
        for shard in 0..count {
            let key = shard.to_string();
            let written = meta.as_ref().and_then(|m| m.get("shards")?.get(&key));
            match written {
                Some(w) => {
                    let size = w["size_in_bytes"].as_u64().unwrap_or(0);
                    done += 1;
                    bytes += size;
                    shards.insert(
                        key,
                        json!({"stage": "DONE", "stats": stats(1, size,
                        w["start_time_in_millis"].as_u64().unwrap_or(started),
                        w["time_in_millis"].as_u64().unwrap_or(0))}),
                    );
                }
                None => {
                    failed += 1;
                    let reason = meta
                        .as_ref()
                        .and_then(|m| m.get("failed_shards")?.get(&key)?.as_str().map(String::from))
                        .unwrap_or_else(|| "the shard was not written".to_string());
                    shards.insert(
                        key,
                        json!({"stage": "FAILURE", "reason": reason,
                        "stats": stats(0, 0, started, 0)}),
                    );
                }
            }
        }
        // the index's description is one more file, holding its mapping
        let (files, bytes) = match described {
            Some(size) => (done + 1, bytes + size),
            None => (done, bytes),
        };
        all_done += done;
        all_failed += failed;
        all_files += files;
        all_bytes += bytes;
        indices.insert(
            index.clone(),
            json!({
                "shards_stats": counts(done, failed),
                "stats": stats(files, bytes, started, took),
                "shards": shards,
            }),
        );
    }
    json!({
        "snapshot": s["snapshot"].clone(),
        "repository": repo,
        "uuid": s["uuid"].clone(),
        "state": s.get("state").cloned().unwrap_or(json!("SUCCESS")),
        "include_global_state": s["include_global_state"].clone(),
        "shards_stats": counts(all_done, all_failed),
        "stats": stats(all_files, all_bytes, started, took),
        "indices": indices,
    })
}

pub async fn clone_snapshot(
    State(store): State<Store>,
    Path((repo, name, target)): Path<(String, String, String)>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    if let Some(refused) =
        bad_snapshot_name(&repo, &name).or_else(|| bad_snapshot_name(&repo, &target))
    {
        return refused;
    }
    refresh_readonly(&store, &repo);
    // named rather than matched, so a record this node has not read is read now
    let _ = pick_snapshots(&store, &repo, &name);
    let held = store.snapshots(&repo);
    // a restore that names a snapshot which is not there failed to restore,
    // which is not the same as a request that merely asked after it
    let Some(source) = held.get(&name) else {
        return err(
            StatusCode::BAD_REQUEST,
            "snapshot_restore_exception",
            format!("[{repo}:{name}] snapshot does not exist"),
        );
    };
    let source = source.clone();
    if held.contains_key(&target) {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_snapshot_name_exception",
            format!(
                "[{repo}:{target}] Invalid snapshot name [{target}], snapshot with the same name \
                 already exists"
            ),
        );
    }
    if let Some(r) = refuse_if_readonly(&store, &repo) {
        return r;
    }
    let body: Value = parse_body(&body).unwrap_or_else(|_| json!({}));
    // a clone chooses among the indices the snapshot holds, which is not the
    // same set as the indices the cluster holds now: resolving the pattern
    // against the live cluster cloned indices the snapshot never had, and
    // dropped the ones that have since been deleted -- the very ones a clone
    // is for
    let held_indices: Vec<String> = source["indices"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let wanted: Option<Vec<String>> = match body.get("indices") {
        Some(Value::String(s)) => Some(s.split(',').map(|s| s.trim().to_string()).collect()),
        Some(Value::Array(a)) => {
            Some(a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        }
        _ => None,
    };
    let indices: Vec<String> = match &wanted {
        Some(w) => held_indices
            .iter()
            .filter(|n| w.iter().any(|one| one == *n || crate::store::glob_match(one, n)))
            .cloned()
            .collect(),
        None => held_indices.clone(),
    };
    let global = source["include_global_state"].as_bool().unwrap_or(true);
    // a clone keeps the streams whose backing indices it chose to carry over,
    // and not the ones whose indices it left behind
    let streams: Vec<String> = held_streams(&source)
        .into_iter()
        .filter(|s| indices.iter().any(|n| n.starts_with(&format!(".ds-{s}-"))))
        .collect();
    let mut record = snapshot_record(&store, &target, indices.clone(), streams, global);
    // the shard counts come from the indices as they are now, and a clone is
    // of what the snapshot holds: what it recorded is what is carried over
    record["shards"] = source["shards"].clone();
    // a clone that records a snapshot without writing one is a snapshot that
    // reads as SUCCESS and restores nothing, so the files are copied
    let Some(from) = store.repositories().get(&repo).and_then(crate::snapshot::Source::of) else {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "repository_exception",
            format!("[{repo}] has nowhere to write snapshot [{target}]"),
        );
    };
    if let Err(e) = off_the_runtime(|| {
        crate::snapshot::clone_into(&from, &from, &name, &target, &indices, &record)
    }) {
        // whatever landed before it failed is not a snapshot anybody may
        // restore from
        crate::snapshot::remove(&from, &target);
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "repository_exception",
            format!("[{repo}] could not clone [{name}] to [{target}]: {e}"),
        );
    }
    store.put_snapshot(&repo, &target, record);
    respond(&p, json!({"acknowledged": true}))
}

pub async fn restore_snapshot(
    State(store): State<Store>,
    Path((repo, name)): Path<(String, String)>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    if let Some(refused) = bad_snapshot_name(&repo, &name) {
        return refused;
    }
    refresh_readonly(&store, &repo);
    // named rather than matched, so a record this node has not read is read now
    let _ = pick_snapshots(&store, &repo, &name);
    let held = store.snapshots(&repo);
    // a restore that names a snapshot which is not there failed to restore,
    // which is not the same as a request that merely asked after it
    let Some(source) = held.get(&name) else {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "snapshot_restore_exception",
            format!("[{repo}:{name}] snapshot does not exist"),
        );
    };
    let body: Value = parse_body(&body).unwrap_or_else(|_| json!({}));
    let held_indices: Vec<String> = source["indices"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    // a record naming no index at all is a repository this node could not
    // read rather than a snapshot of nothing: answering 200 for it is a
    // restore that says it worked and restored nothing. A snapshot of the
    // cluster's global state alone is a snapshot of something, when that is
    // what is asked back.
    let global_only = source.get("include_global_state").and_then(|v| v.as_bool()) == Some(true)
        && body.get("include_global_state").and_then(|v| v.as_bool()) == Some(true);
    if held_indices.is_empty() && !global_only {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "repository_exception",
            format!(
                "[{repo}:{name}] lists no indices; the repository could not be read, or the \
                 snapshot was never finished"
            ),
        );
    }
    let asked: Vec<String> = match body.get("indices") {
        Some(Value::String(s)) => s.split(',').map(|s| s.trim().to_string()).collect(),
        Some(Value::Array(a)) => {
            a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()
        }
        _ => held_indices.clone(),
    };
    // A data stream is asked back by its own name, which is not the name of
    // anything the snapshot holds: the indices it holds are the stream's
    // backing ones. A restore of `logs-app` was answered `no such index
    // [logs-app]` for want of this, and the only way to get the stream back
    // was to name every generation of it.
    let streams = held_streams(source);
    let wanted: Vec<String> = asked
        .iter()
        .flat_map(|want| {
            let named: Vec<String> = streams
                .iter()
                .filter(|s| *want == **s || crate::store::glob_match(want, s))
                .map(|s| format!(".ds-{s}-*"))
                .collect();
            if named.is_empty() { vec![want.clone()] } else { named }
        })
        .collect();
    let uuid = source.get("uuid").and_then(|v| v.as_str()).unwrap_or("_na_").to_string();
    // An index named outright that the snapshot does not hold is missing: it
    // was answered 200 with nothing restored, which reads as a restore that
    // worked. A pattern that matches none is not an error, and neither is a
    // name the caller said may be unavailable.
    let lenient = body.get("ignore_unavailable").and_then(|v| v.as_bool()).unwrap_or(false)
        || p.get("ignore_unavailable").map(|v| v == "true").unwrap_or(false);
    if !lenient
        && let Some(missing) =
            wanted.iter().find(|w| !w.contains('*') && !held_indices.iter().any(|h| h == *w))
    {
        // in the restore action's shape, which names the index and its uuid
        // but not the resource the other 404s carry
        let reason = format!("no such index [{missing}]");
        let cause = json!({"type": "index_not_found_exception", "reason": reason,
                           "index": missing, "index_uuid": "_na_"});
        let mut error = cause.clone();
        error["root_cause"] = json!([cause]);
        return (StatusCode::NOT_FOUND, axum::Json(json!({"error": error, "status": 404})))
            .into_response();
    }
    // How many shards an index has is what its documents were routed by; a
    // restore that changed it would put every document where no lookup by id
    // finds it. The reference refuses it, and so do the settings it lists as
    // fixed at creation. It used to be taken and quietly ignored.
    if let Some(Value::Object(asked)) = body.get("index_settings") {
        let fixed = ["number_of_shards", "uuid", "version.created", "creation_date"];
        let named = |key: &str| key.strip_prefix("index.").unwrap_or(key).to_string();
        let mut keys: Vec<String> = Vec::new();
        for (k, v) in asked {
            match (k.as_str(), v) {
                ("index", Value::Object(inner)) => keys.extend(inner.keys().cloned()),
                _ => keys.push(named(k)),
            }
        }
        if let Some(bad) = keys.iter().find(|k| fixed.contains(&String::as_str(k))) {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "snapshot_restore_exception",
                format!(
                    "[{repo}:{name}/{uuid}] cannot modify UnmodifiableOnRestore setting \
                     [index.{bad}] on restore"
                ),
            );
        }
    }
    // a name may be given back changed, which is how a snapshot is restored
    // beside the index it was taken from
    let rename = |n: &str| -> String {
        let (Some(pat), Some(rep)) = (
            body.get("rename_pattern").and_then(|v| v.as_str()),
            body.get("rename_replacement").and_then(|v| v.as_str()),
        ) else {
            return n.to_string();
        };
        match regex::Regex::new(pat) {
            Ok(re) => re.replace(n, rep).to_string(),
            Err(_) => n.to_string(),
        }
    };
    let from = store.repositories().get(&repo).and_then(crate::snapshot::Source::of);
    let cluster = clustered();
    let restore_failed =
        |e: String| err(StatusCode::INTERNAL_SERVER_ERROR, "repository_exception", e);
    let open_exists = |target: &str| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "snapshot_restore_exception",
            format!(
                "[{repo}:{name}/{uuid}] cannot restore index [{target}] because an open index \
                 with same name already exists in the cluster. Either close or delete the \
                 existing index or restore the index under a different name by providing a \
                 rename pattern and replacement name"
            ),
        )
    };
    // First, everything the restore will do is decided and everything it will
    // read is read and checked, with nothing in the cluster touched. A restore
    // used to take a closed index out of its way having only looked at the
    // snapshot's description of it, and then find the documents damaged: it
    // reported the failure, and the index that had been there was gone.
    let mut steps: Vec<(String, String, Replaces)> = Vec::new();
    for n in held_indices
        .iter()
        .filter(|n| wanted.iter().any(|w| w == *n || crate::store::glob_match(w, n)))
    {
        let target = rename(n);
        // a rename is a name a caller made up, and it becomes an index: it
        // goes through the same door `PUT /{index}` does. A replacement of
        // `` or `..` used to reach the store as an index name, and a delete
        // of it reached the filesystem
        if target.is_empty() || target == "." || target == ".." {
            return err(
                StatusCode::BAD_REQUEST,
                "invalid_index_name_exception",
                format!("Invalid index name [{target}], must not be empty, '.' or '..'"),
            );
        }
        if let Some(refused) = crate::api::indices::reserved_index_name(&target)
            .or_else(|| crate::api::indices::bad_index_name(&target))
        {
            return refused;
        }
        // a rename made up an upper-case name and the restore created it: no
        // index can be made under it any other way
        if target != target.to_lowercase() {
            let reason = format!("Invalid index name [{target}], must be lowercase");
            let cause = json!({"type": "invalid_index_name_exception", "reason": reason,
                               "index": target, "index_uuid": "_na_"});
            let mut error = cause.clone();
            error["root_cause"] = json!([cause]);
            return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": error, "status": 400})))
                .into_response();
        }
        // a name that stands for several indices is not a name a restore
        // may write to: `store.get` answers for an alias with one of the
        // indices behind it, while deleting that name deletes all of them
        if store.resolve(&target).len() > 1 || store.is_alias(&target) {
            return err(
                StatusCode::BAD_REQUEST,
                "invalid_index_name_exception",
                format!(
                    "[{target}] is an alias, and a restore writes to an index: restore under a \
                     different name by providing a rename pattern and replacement name"
                ),
            );
        }
        // an open index is being written to: restoring over it would mean
        // two sets of documents under one name, so the reference refuses it
        // and names the two ways out. On a cluster the index may be held
        // only by other nodes.
        let replaces = match store.get(&target) {
            Some(st) if !st.read().closed => return open_exists(&target),
            Some(_) => Replaces::ClosedHere,
            None => match cluster.as_ref().and_then(|s| s.indices.get(&target)) {
                Some(m) if m.state != "close" => return open_exists(&target),
                Some(m) => Replaces::ClosedElsewhere(m.uuid.clone()),
                None => Replaces::Nothing,
            },
        };
        match from.as_ref() {
            Some(source) => {
                if let Err(e) =
                    off_the_runtime(|| crate::snapshot::prepare(source, &name, n, &body))
                {
                    return restore_failed(e);
                }
            }
            // nothing to read an index back from, and none to replace
            None if matches!(replaces, Replaces::Nothing) => continue,
            None => {}
        }
        steps.push((n.clone(), target, replaces));
    }
    // the cluster's templates, pipelines, scripts and settings, where they
    // were kept and are asked for
    let restore_global = body.get("include_global_state").and_then(|v| v.as_bool()) == Some(true)
        && source.get("include_global_state").and_then(|v| v.as_bool()) == Some(true);
    let global = match (restore_global, from.as_ref()) {
        (true, Some(f)) => match off_the_runtime(|| crate::snapshot::read_global(f, &name)) {
            Ok(g) => Some(g),
            Err(e) => {
                return err(StatusCode::INTERNAL_SERVER_ERROR, "snapshot_restore_exception", e);
            }
        },
        _ => None,
    };

    // Then the indices are made. An index being replaced is set aside rather
    // than deleted, and every index this restore made is taken away and every
    // one it set aside put back if any of them cannot be made whole: a
    // restore brings back everything it was asked for, or leaves the cluster
    // as it found it and says why.
    let mut made: Vec<(String, Option<crate::store::SetAside>)> = Vec::new();
    let undo = |made: Vec<(String, Option<crate::store::SetAside>)>| {
        for (target, aside) in made.into_iter().rev() {
            store.delete(&target);
            store.end_restore(&target);
            if let Some(aside) = aside {
                store.put_back(aside);
            }
        }
    };
    let mut restored = Vec::new();
    for (n, target, replaces) in steps {
        let Some(source) = from.as_ref() else {
            // nothing to read it back from: the closed index is opened again
            if let Some(st) = store.get(&target) {
                let mut g = st.write();
                g.closed = false;
                g.restored = true;
                g.save_meta();
            }
            restored.push(target);
            continue;
        };
        // read again, and checked again, right before it is used: the
        // repository is somebody's directory and may have changed since
        let prepared = match off_the_runtime(|| crate::snapshot::prepare(source, &name, &n, &body))
        {
            Ok(p) => p,
            Err(e) => {
                undo(made);
                return restore_failed(e);
            }
        };
        let aside = match replaces {
            Replaces::Nothing => None,
            // still closed, or the restore does not happen: an index opened
            // and written to while the repository was read is not one to
            // replace
            Replaces::ClosedHere => match store.set_aside_if_closed(&target) {
                Some(a) => Some(a),
                None => {
                    undo(made);
                    return open_exists(&target);
                }
            },
            // held only by other nodes: it is deleted through the cluster,
            // and the restore waits for the cluster to have let it go before
            // it makes the index that replaces it
            Replaces::ClosedElsewhere(old) => {
                store.tombstone(&target, &old);
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                while crate::cluster::current_state().indices.get(&target).map(|m| m.uuid == old)
                    == Some(true)
                    && std::time::Instant::now() < deadline
                {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                None
            }
        };
        // The documents go into an index no other node has a copy of yet: the
        // copies the cluster places are filled from this one once it is
        // published. Written inside the request's replication scope they
        // were taken for writes to be sent on, to a primary the published
        // state does not know of, and the restore was answered as a write
        // this node was no longer the primary for.
        let unshared = crate::cluster::replication::Writes::default();
        let result = match store.begin_restore(&target) {
            Ok(()) => off_the_runtime(|| {
                crate::cluster::replication::WRITES
                    .sync_scope(unshared, || crate::snapshot::apply(&store, prepared, &target))
            }),
            Err(e) => Err(format!("[{target}] could not be marked as being restored: {e}")),
        };
        made.push((target.clone(), aside));
        match result {
            Ok(docs) => {
                tracing::info!("restored [{target}] from [{repo}:{name}] with {docs} documents");
                restored.push(target);
            }
            Err(e) => {
                undo(made);
                return restore_failed(e);
            }
        }
    }
    // every index is whole: what was set aside for them goes, and they are
    // no longer being restored
    for (target, aside) in made {
        store.end_restore(&target);
        if let Some(aside) = aside {
            store.let_go(aside);
        }
        // and open: an index put in the place of a closed one kept the closed
        // mark of the one it replaced, so `_cat` said open and a search said
        // `index_closed_exception`
        if let Some(st) = store.get(&target) {
            let mut g = st.write();
            g.closed = false;
            g.restored = true;
            g.save_meta();
        }
    }
    if let Some(global) = global {
        crate::snapshot::apply_global(&store, &global);
    }
    // A stream is the name in front of its backing indices, so bringing the
    // indices back leaves them loose until the name is put back too: the
    // stream read as gone, a write to it made an ordinary index of its name,
    // and the documents that were restored were not searchable under it. The
    // streams the snapshot recorded come back with whichever of their backing
    // indices this restore made, under whatever name those indices now have.
    for stream in &streams {
        let prefix = format!(".ds-{stream}-");
        if !restored.iter().any(|n| n.starts_with(&prefix)) {
            continue;
        }
        if store.data_streams().contains_key(stream) {
            continue;
        }
        // the template is resolved now rather than remembered: a stream
        // reports the template it fits today, as `GET _data_stream` does
        let template = crate::api::data_stream_template(&store, stream)
            .map(|(name, _)| name)
            .unwrap_or_default();
        store.add_data_stream(stream, &template);
    }
    let shards = restored.len().max(1);
    respond(
        &p,
        json!({"snapshot": {
            "snapshot": name,
            "indices": restored,
            "shards": {"total": shards, "failed": 0, "successful": shards},
        }}),
    )
}

/// What an index a restore makes takes the place of.
enum Replaces {
    Nothing,
    /// a closed index this node holds
    ClosedHere,
    /// a closed index held only by other nodes, by its uuid
    ClosedElsewhere(String),
}
