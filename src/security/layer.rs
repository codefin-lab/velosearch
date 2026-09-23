//! The gate every request passes: who is asking, and whether they may.
//!
//! Authentication reads the `Authorization` header and answers 401 the way
//! the plugin does (`text/plain` `Unauthorized`, with a Basic challenge).
//! Authorization maps the request to the transport action it stands for
//! and asks the evaluator; a refusal is the plugin's `security_exception`.

use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use super::{Caller, Verdict, is_cluster_action};

tokio::task_local! {
    /// The caller of the request being handled, visible to everything the
    /// handler runs on this task.
    pub static CALLER: Caller;
}

/// The caller of the request in hand, if a request is in hand.
pub fn current_caller() -> Option<Caller> {
    CALLER.try_with(|c| c.clone()).ok()
}

async fn run_as(caller: Caller, req: Request, next: Next) -> Response {
    let mut req = req;
    req.extensions_mut().insert(caller.clone());
    CALLER.scope(caller, next.run(req)).await
}
use crate::store::Store;

/// The subject DN of the client certificate a connection presented.
#[derive(Clone, Debug)]
pub struct PeerDn(pub String);

/// The `401 Unauthorized` the plugin answers, with the challenge asked for.
pub fn unauthorized_with(challenge: &str) -> Response {
    // the plugin's basic challenge says `Unauthorized`; its bearer and
    // SAML challenges say nothing at all
    let body = if challenge.starts_with("Basic") { "Unauthorized" } else { "" };
    let mut r = (StatusCode::UNAUTHORIZED, body).into_response();
    if let Ok(v) = HeaderValue::from_str(challenge) {
        r.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=UTF-8"));
    r
}

/// The `401 Unauthorized` the plugin answers.
pub fn unauthorized() -> Response {
    let mut r = (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    r.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"OpenSearch Security\""),
    );
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=UTF-8"));
    r
}

/// The `403` `security_exception` the plugin answers.
pub fn forbidden(reason: String) -> Response {
    let body = json!({
        "error": {
            "root_cause": [{"type": "security_exception", "reason": reason}],
            "type": "security_exception",
            "reason": reason,
        },
        "status": 403,
    });
    (StatusCode::FORBIDDEN, axum::Json(body)).into_response()
}

/// The answer to anybody while the node has no configuration it may let them
/// in by: the plugin's own words for a node whose security index is not
/// there, or the `no cluster-manager` block for a node of a cluster that has
/// lost its manager and so cannot know what was revoked while it was gone.
fn not_ready(standing: super::Standing) -> Response {
    match standing {
        super::Standing::NoManager => crate::api::err(
            StatusCode::SERVICE_UNAVAILABLE,
            "cluster_block_exception",
            "blocked by: [SERVICE_UNAVAILABLE/2/no cluster-manager];",
        ),
        _ => {
            let mut r = (StatusCode::SERVICE_UNAVAILABLE, "OpenSearch Security not initialized.")
                .into_response();
            r.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=UTF-8"),
            );
            r
        }
    }
}

/// `no permissions for [action] and User [...]`
pub fn no_permissions(action: &str, caller: &Caller) -> Response {
    forbidden(format!("no permissions for [{action}] and {}", caller.describe()))
}

/// Work out the caller and put it on the request; refuse what they may
/// not do before the handler runs.
pub async fn authenticate(State(store): State<Store>, req: Request, next: Next) -> Response {
    // a request another node forwarded here comes with the caller it was
    // authenticated as there; only this node's transport handler can set it
    if let Some(f) = req.extensions().get::<crate::cluster::forward::ForwardedCaller>() {
        let caller = f.0.clone();
        return run_as(caller, req, next).await;
    }
    let sec = store.security.clone();
    if !sec.enabled {
        return run_as(Caller::unrestricted(), req, next).await;
    }
    // the SAML token exchange is how a caller gets credentials: it runs
    // for anyone, as the plugin runs it inside its challenge
    // -- and it is that path, not any path that happens to end with it:
    // `PUT /_alias/_plugins/_security/api/authtoken` ended with it too, and
    // ran with no credentials at all
    if req.uri().path().trim_end_matches('/') == "/_plugins/_security/api/authtoken" {
        return run_as(Caller::default(), req, next).await;
    }
    // the plugin's health answers anyone, as the plugin answers it before it
    // asks who is calling: it is what a probe with no credentials -- a
    // container's healthcheck, a load balancer -- can ask. It says whether
    // the node is up and nothing about any index. The exact path, read only.
    if req.method() == axum::http::Method::GET
        && matches!(
            req.uri().path().trim_end_matches('/'),
            "/_plugins/_security/health" | "/_opendistro/_security/health"
        )
    {
        return run_as(Caller::default(), req, next).await;
    }
    // Nobody is let in by a configuration the node does not have, or by one
    // it cannot say is the cluster's. A node that rejoined its cluster used to
    // answer by the files it restarted with, and a user deleted while it was
    // away logged in through it for as long as it ran.
    let standing = sec.standing();
    if standing != super::Standing::Ready {
        return not_ready(standing);
    }
    // the plain listener reports the peer as this crate's own type, the TLS
    // one hands the address in as itself
    let remote = req
        .extensions()
        .get::<axum::extract::ConnectInfo<crate::http_compat::Peer>>()
        .map(|c| c.0.0)
        .or_else(|| {
            req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0)
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_default();
    let peer_dn = req.extensions().get::<PeerDn>().map(|d| d.0.clone());
    let query = req.uri().query().unwrap_or("").to_string();
    let audit = sec.audit.clone();
    // a request carrying the plugin's own internal headers is refused, and
    // that refusal is written down
    if req.headers().keys().any(|k| {
        k.as_str().starts_with("_opendistro_security_") || k.as_str().starts_with("_security_")
    }) {
        let (req, body_text) = buffered(req).await;
        let info = request_info(&req, &query, &remote, &body_text);
        audit.bad_headers(&info);
        return bad_headers_response();
    }
    let caller = {
        let path_asked = req.uri().path().to_string();
        let presented = super::authc::Presented {
            headers: req.headers(),
            query: &query,
            remote: remote.clone(),
            peer_dn,
            path: &path_asked,
            method: req.method().as_str(),
        };
        match sec.caller_for(&presented).await {
            Ok(c) => c,
            Err(refusal) => {
                // the body is read only now, for the record of the failure
                let name = presented_name(req.headers());
                let (req, body_text) = buffered(req).await;
                let info = request_info(&req, &query, &remote, &body_text);
                if let Some(name) = name {
                    audit.failed_login(Some(&name), &info);
                }
                return match refusal {
                    super::authc::Refusal::Challenge(ch) => unauthorized_with(&ch),
                    super::authc::Refusal::Forbidden => {
                        forbidden("Authentication finally failed".into())
                    }
                };
            }
        }
    };
    // the tenant a Dashboards caller works in comes with every request, in
    // either of the two headers the plugin reads; it is part of who the
    // caller is for this request, and every refusal names it
    let mut caller = caller;
    if !caller.admin_cert
        && let Some(t) = ["securitytenant", "security_tenant"]
            .iter()
            .find_map(|h| req.headers().get(*h).and_then(|v| v.to_str().ok()))
    {
        caller.requested_tenant = Some(t.to_string());
    }
    // the body is copied for the log only when a record would quote it;
    // a bulk of a megabyte is otherwise passed straight through
    let path_now = req.uri().path().to_string();
    let method_now = req.method().clone();
    let admin_action = action_for(&method_now, &path_now)
        .map(|a| {
            a.starts_with("indices:admin/")
                && !a.starts_with("indices:admin/get")
                && !a.starts_with("indices:admin/mappings/get")
                && !a.starts_with("indices:admin/aliases/get")
        })
        .unwrap_or(false);
    // a search over no index in its path may be held to a point in time,
    // whose indices are named in the body and have to be read to be judged
    let names_no_index = path_now.trim_end_matches('/') == "/_search";
    let quoted =
        audit.quotes_bodies(admin_action, path_now.starts_with("/_plugins/_security/api/"));
    let (mut req, read_body) =
        if names_no_index || quoted { buffered(req).await } else { (req, String::new()) };
    // the record quotes a body only where it would have quoted it anyway
    let body_text = if quoted { read_body.clone() } else { String::new() };
    let info = request_info(&req, &query, &remote, &body_text);
    audit.authenticated(&caller, &info);
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    // the security API and account endpoints decide for themselves
    if path.starts_with("/_plugins/_security/") {
        let obo = path.trim_end_matches('/') == "/_plugins/_security/api/generateonbehalfoftoken";
        if obo && method == Method::POST {
            // the token endpoint is a named route: judged by its own name as a
            // cluster permission, not by whether the caller administers security
            let allowed = sec.config.read().cluster_allowed(&caller, super::api::OBO_ACTION);
            if !allowed {
                audit.missing_privileges_rest(&caller, super::api::OBO_ACTION, &info);
                let reason = format!(
                    "no permissions for [{}] and {}",
                    super::api::OBO_ACTION,
                    caller.describe()
                );
                let mut r = (StatusCode::UNAUTHORIZED, reason).into_response();
                r.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; charset=UTF-8"),
                );
                return r;
            }
            audit.granted_rest(&caller, &info);
            return run_as(caller, req, next).await;
        }
        if path.starts_with("/_plugins/_security/api/") && sec.may_administer(&caller) {
            audit.granted_rest(&caller, &info);
        }
        let who = caller.clone();
        let response = run_as(caller, req, next).await;
        // the API refuses in its handlers, which know which endpoint and which
        // method was refused; the record of it needs the request, which is here
        if let Some(refused) = response.extensions().get::<super::api::ApiRefused>() {
            audit.missing_privileges_rest(&who, &refused.0, &info);
        }
        return response;
    }
    let Some(action) = action_for(&method, &path) else {
        // an endpoint this node does not have is answered as OpenSearch
        // answers it, to anyone it has authenticated: there is no handler, so
        // there is nothing to be refused permission for
        if !served_here(&path) {
            let body = json!({
                "error": format!("no handler found for uri [{path}] and method [{method}]")
            });
            return (StatusCode::BAD_REQUEST, axum::Json(body)).into_response();
        }
        // a path with no action is a path nothing judged. Running it was
        // how `_upgrade` listed every index and its size to a caller with
        // read on one of them.
        let unmapped = format!("indices:admin/unmapped[{path}]");
        audit.missing_privileges(&caller, &unmapped, &info, &[], &[]);
        return no_permissions(&unmapped, &caller);
    };
    // the query languages name their index in the body, where this layer
    // cannot see it: the handler judges that index itself, the way a bulk
    // judges each item, and this layer only writes the request down
    let query_language = matches!(
        path.trim_end_matches('/'),
        "/_plugins/_sql"
            | "/_plugins/_ppl"
            | "/_plugins/_sql/_explain"
            | "/_plugins/_ppl/_explain"
            | "/_plugins/_sql/close"
    );
    if query_language {
        audit.granted_privileges(&caller, &action, &info, &[], &[]);
        return run_as(caller, req, next).await;
    }
    let named = indices_of(&path);
    // a search held to a point in time names no index in its path: the
    // indices are the ones the point in time was opened over, and those are
    // what the caller needs to be allowed to read
    let pit_indices: Option<Vec<String>> = (action == "indices:data/read/search"
        && named.is_empty())
    .then(|| serde_json::from_str::<serde_json::Value>(&read_body).ok())
    .flatten()
    .and_then(|b| b.pointer("/pit/id").and_then(|i| i.as_str()).map(String::from))
    .and_then(|id| crate::store::PitId::decode(&id))
    .map(|id| id.indices());
    // every index the request turns out to touch, which the audit log records
    // and which is only known once the request has been classified
    let mut resolved: Vec<String>;
    // the indices a partial grant narrows the request to
    let mut narrowed: Option<Vec<String>> = None;
    // the tenant's own index a Dashboards request is moved to
    let mut tenant_index: Option<String> = None;
    // the guard must be gone before the handler is awaited
    let refusal = {
        let cfg = sec.config.read();
        let no_tenant = caller.requested_tenant.as_deref().unwrap_or("").is_empty();
        if is_cluster_action(&action) || named.is_empty() && !action.starts_with("indices:") {
            // the plugin resolves a cluster request to every index it touches
            resolved = store.resolve("*");
            resolved.sort();
            // A bulk with no tenant is only asked whether the cluster action
            // is granted -- its items are judged one index at a time later.
            // Anything else a service account asks at the cluster level is
            // refused whatever its roles grant.
            let bulk_shortcut = action == "indices:data/write/bulk" && no_tenant;
            if (caller.is_service_account() && !bulk_shortcut)
                || !cfg.cluster_allowed(&caller, &action)
            {
                Some(action.clone())
            } else {
                if !bulk_shortcut {
                    let local = resolve_indices(&store, &named);
                    match dashboards_tenant(&cfg, &caller, &action, &named, &local) {
                        TenantVerdict::Continue => {}
                        TenantVerdict::Granted(moved) => tenant_index = moved,
                        // the plugin writes the refusal down and then lets the
                        // request through on its cluster permission
                        TenantVerdict::Denied => {
                            audit.missing_privileges(&caller, &action, &info, &named, &local)
                        }
                    }
                }
                None
            }
        } else {
            // an index action naming no index is over every index there is
            let indices = if let Some(held) = &pit_indices {
                held.clone()
            } else if named.is_empty() || named.iter().any(|n| n == "_all") {
                let mut all = store.resolve("*");
                all.sort();
                all
            } else {
                resolve_indices(&store, &named)
            };
            resolved = indices.clone();
            match dashboards_tenant(&cfg, &caller, &action, &named, &indices) {
                TenantVerdict::Continue => match cfg.index_verdict(&caller, &action, &indices) {
                    Verdict::Allowed => None,
                    // allowed for some of what was asked: the request is
                    // narrowed to those before it runs, which is what
                    // do_not_fail_on_forbidden means -- not that the rest is
                    // reached anyway
                    // a point in time cannot be narrowed to part of what it
                    // holds: the id names every index it reads
                    Verdict::Partial(_) if pit_indices.is_some() => Some(action.clone()),
                    Verdict::Partial(granted) => {
                        resolved = granted.clone();
                        narrowed = Some(granted);
                        None
                    }
                    Verdict::Denied { missing } => Some(missing),
                },
                // allowed by the tenant, not by any index permission
                TenantVerdict::Granted(moved) => {
                    tenant_index = moved;
                    None
                }
                // the plugin writes this refusal down twice: once where the
                // tenant is judged, once where the request is refused
                TenantVerdict::Denied => {
                    audit.missing_privileges(&caller, &action, &info, &named, &resolved);
                    Some(action.clone())
                }
            }
        }
    };
    if let Some(missing) = refusal {
        audit.missing_privileges(&caller, &missing, &info, &named, &resolved);
        return no_permissions(&missing, &caller);
    }
    if let Some(granted) = narrowed {
        if granted.is_empty() {
            return no_permissions(&action, &caller);
        }
        narrow_request(&mut req, &named, &granted);
    }
    if let Some(moved) = tenant_index {
        narrow_request(&mut req, &named, std::slice::from_ref(&moved));
        resolved = vec![moved];
    }
    let admin_action = action.starts_with("indices:admin/")
        && !action.starts_with("indices:admin/get")
        && !action.starts_with("indices:admin/mappings/get")
        && !action.starts_with("indices:admin/aliases/get");
    // an index-administration action is written down twice, as the
    // plugin writes it: the grant, and the index event
    audit.granted_privileges(&caller, &action, &info, &named, &resolved);
    // a single document write is a bulk of one inside OpenSearch, and the
    // bulk is granted in its own record
    if matches!(
        action.as_str(),
        "indices:data/write/index" | "indices:data/write/delete" | "indices:data/write/update"
    ) {
        let mut bulk_info = info.clone();
        bulk_info.params.remove("id");
        audit.granted_privileges(&caller, "indices:data/write/bulk", &bulk_info, &[], &[]);
    }
    if admin_action {
        audit.index_event(&caller, &action, &info, &named, &resolved, Some(&body_text));
    }
    run_as(caller, req, next).await
}

/// What the Dashboards tenant a request names makes of it.
enum TenantVerdict {
    /// nothing: the request is judged by its index permissions
    Continue,
    /// allowed, and moved to this tenant index where one is named
    Granted(Option<String>),
    Denied,
}

/// The actions a read-only tenant allows.
const TENANT_READ_ACTIONS: &[&str] = &[
    "indices:admin/get",
    "indices:data/read/get",
    "indices:data/read/search",
    "indices:data/read/msearch",
    "indices:data/read/mget",
    "indices:data/read/mget[shard]",
];

/// The index a tenant's saved objects live in: the Dashboards index, the
/// tenant name's Java hash, and the name lowercased to its letters and digits.
pub fn tenant_index_name(dashboards_index: &str, tenant: &str) -> String {
    let hash = tenant.encode_utf16().fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(c as i32));
    let plain: String = tenant
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect();
    format!("{dashboards_index}_{hash}_{plain}")
}

/// The plugin's multitenancy, for a request by a caller other than the
/// Dashboards server: a request to the Dashboards index alone is moved to the
/// index of the tenant the caller asked for, if the caller may work in that
/// tenant, and refused if not; with no tenant asked for, the global tenant is
/// the one judged. A request naming a tenant's own index directly is allowed
/// to a caller who may work in that tenant.
fn dashboards_tenant(
    cfg: &super::SecurityConfig,
    caller: &super::Caller,
    action: &str,
    named: &[String],
    resolved: &[String],
) -> TenantVerdict {
    let kibana = cfg.dynamic.pointer("/dynamic/kibana");
    let setting = |k: &str| kibana.and_then(|v| v.get(k));
    if caller.unrestricted
        || !setting("multitenancy_enabled").and_then(|v| v.as_bool()).unwrap_or(true)
    {
        return TenantVerdict::Continue;
    }
    let server = setting("server_username").and_then(|v| v.as_str()).unwrap_or("kibanaserver");
    let index = setting("index").and_then(|v| v.as_str()).unwrap_or(".kibana");
    let requested = caller.requested_tenant.as_deref().unwrap_or("");
    if requested == "__user__"
        && !setting("private_tenant_enabled").and_then(|v| v.as_bool()).unwrap_or(true)
    {
        return TenantVerdict::Denied;
    }
    let by_server = caller.name == server;
    let dashboards_only = !by_server && !named.is_empty() && named.iter().all(|n| n == index);
    let write = !TENANT_READ_ACTIONS.contains(&action);
    if requested.is_empty() {
        if dashboards_only && !cfg.tenant_privilege(caller, "global_tenant", write) {
            return TenantVerdict::Denied;
        }
        return TenantVerdict::Continue;
    }
    let private = requested == "__user__" || requested == caller.name;
    let tenant = if private { caller.name.as_str() } else { requested };
    let own_index = tenant_index_name(index, tenant);
    let local_all = named.is_empty() || named.iter().any(|n| n == "_all" || n == "*");
    if !by_server
        && !local_all
        && resolved.len() == 1
        && resolved[0].starts_with(&own_index)
        && (private || cfg.tenant_privilege(caller, tenant, write))
    {
        return TenantVerdict::Granted(None);
    }
    if dashboards_only {
        if !private && !cfg.tenant_privilege(caller, tenant, write) {
            return TenantVerdict::Denied;
        }
        return TenantVerdict::Granted(Some(own_index));
    }
    TenantVerdict::Continue
}

/// The first path segments this node has endpoints under: a path beginning
/// with any other `_` word has no handler at all.
const SERVED: &[&str] = &[
    "_alias",
    "_aliases",
    "_all",
    "_analyze",
    "_velo",
    "_velosearch",
    "_bulk",
    "_cache",
    "_cat",
    "_cluster",
    "_component_template",
    "_count",
    "_data_stream",
    "_delete_by_query",
    "_field_caps",
    "_flush",
    "_forcemerge",
    "_index_template",
    "_ingest",
    "_insights",
    "_list",
    "_mapping",
    "_mget",
    "_msearch",
    "_mtermvectors",
    "_nodes",
    "_opendistro",
    "_plugins",
    "_prometheus",
    "_rank_eval",
    "_recovery",
    "_refresh",
    "_reindex",
    "_remote",
    "_render",
    "_resolve",
    "_script_context",
    "_script_language",
    "_scripts",
    "_search",
    "_search_shards",
    "_segments",
    "_settings",
    "_shard_stores",
    "_snapshot",
    "_stats",
    "_tasks",
    "_template",
    "_update_by_query",
    "_upgrade",
    "_validate",
];

/// Whether a path could reach an endpoint here: one naming an index first
/// may, one beginning with an `_` word only if that word is served. A path
/// of one word alone is the index routes' to answer, as it is OpenSearch's:
/// `GET /_x` is an index with a name no index may have, not a missing handler.
fn served_here(path: &str) -> bool {
    let mut segs = path.trim_matches('/').split('/');
    let first = segs.next().unwrap_or("");
    !first.starts_with('_') || SERVED.contains(&first) || segs.next().is_none()
}

/// The request with its body read into memory, and that body as text.
async fn buffered(req: Request) -> (Request, String) {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, crate::api::max_content_bytes() as usize)
        .await
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes).to_string();
    (Request::from_parts(parts, axum::body::Body::from(bytes)), text)
}

/// What the audit log quotes of a request.
fn request_info(req: &Request, query: &str, remote: &str, body: &str) -> super::audit::RequestInfo {
    let path = req.uri().path().to_string();
    let mut params = std::collections::BTreeMap::new();
    // the route's own names, as OpenSearch's REST handlers name them
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    if let Some(first) = segs.first().filter(|s| !s.is_empty() && !s.starts_with('_')) {
        params.insert("index".to_string(), first.to_string());
        if segs.len() >= 3
            && matches!(
                segs[1],
                "_doc" | "_create" | "_update" | "_source" | "_explain" | "_termvectors"
            )
        {
            params.insert("id".to_string(), segs[2].to_string());
        }
    }
    if path.starts_with("/_plugins/_security/api/") && segs.len() >= 5 {
        params.insert("name".to_string(), segs[4].to_string());
    }
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(
            k.to_string(),
            percent_encoding::percent_decode_str(v).decode_utf8_lossy().replace('+', " "),
        );
    }
    let headers = req
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    super::audit::RequestInfo {
        method: req.method().as_str().to_string(),
        path,
        params,
        headers,
        body: if body.is_empty() { None } else { Some(body.to_string()) },
        remote: remote.to_string(),
    }
}

/// The user name a refused request presented, for the failed-login record.
fn presented_name(headers: &axum::http::HeaderMap) -> Option<String> {
    if let Some(h) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some(b) = h.strip_prefix("Basic ").or_else(|| h.strip_prefix("basic ")) {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD.decode(b.trim()).ok()?;
            let text = String::from_utf8_lossy(&bytes).to_string();
            return Some(text.split_once(':').map(|(n, _)| n.to_string()).unwrap_or(text));
        }
        let token = h.trim_start_matches("Bearer ").trim_start_matches("bearer ");
        return jwt_subject(token);
    }
    headers.get("x-proxy-user").and_then(|v| v.to_str().ok()).map(|s| s.to_string())
}

fn jwt_subject(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    let _ = parts.next()?;
    let payload = parts.next()?;
    use base64::Engine;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("sub").and_then(|s| s.as_str()).map(|s| s.to_string())
}

/// The plugin's refusal of a request carrying its internal headers.
fn bad_headers_response() -> Response {
    let reason = "Illegal parameter in http or transport request found.\nThis means that one node is trying to connect to another with \na non-node certificate (no OID or security.nodes_dn incorrect configured) or that someone \nis spoofing requests. Check your TLS certificate setup as described here: See https://opendistro.github.io/for-elasticsearch-docs/docs/troubleshoot/tls/";
    (StatusCode::FORBIDDEN, axum::Json(json!({"error": {"status": "error", "reason": reason}})))
        .into_response()
}

/// The request's path rewritten to name only the indices it was granted:
/// the first segment replaced where it named indices, or the granted
/// list put in front where it named none (`/_search` over everything).
fn narrow_request(req: &mut Request, named: &[String], granted: &[String]) {
    let joined: String = granted
        .iter()
        .map(|g| {
            percent_encoding::utf8_percent_encode(g, percent_encoding::NON_ALPHANUMERIC).to_string()
        })
        .collect::<Vec<_>>()
        .join(",");
    let uri = req.uri().clone();
    let path = uri.path();
    let rest = path.trim_start_matches('/');
    let new_path = match named.is_empty() {
        true => format!("/{joined}/{rest}"),
        false => match rest.split_once('/') {
            Some((_, tail)) => format!("/{joined}/{tail}"),
            None => format!("/{joined}"),
        },
    };
    let full = match uri.query() {
        Some(q) => format!("{new_path}?{q}"),
        None => new_path,
    };
    if let Ok(new_uri) = full.parse::<axum::http::Uri>() {
        *req.uri_mut() = new_uri;
    }
}

/// The index expression a path names, split on commas; nothing for
/// paths that name no index.
pub fn indices_of(path: &str) -> Vec<String> {
    let trimmed = path.trim_start_matches('/');
    // A plugin route may name its index further along: `_ism/add/{index}`
    // and `_knn/warmup/{index}` are done to that index, and judging them
    // over every index instead is both wrong and, where a role grants the
    // action on `*`, wrong in the dangerous direction.
    let parts: Vec<&str> = trimmed.split('/').collect();
    if parts.first() == Some(&"_plugins") {
        let named = match (parts.get(1).copied(), parts.get(2).copied()) {
            (Some("_ism"), Some("add" | "remove" | "change_policy" | "retry" | "explain")) => {
                parts.get(3)
            }
            (Some("_knn"), Some("warmup")) => parts.get(3),
            _ => None,
        };
        if let Some(named) = named {
            let named = percent_encoding::percent_decode_str(named).decode_utf8_lossy();
            return named.split(',').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
        }
        return Vec::new();
    }
    // decoded first, as the handler will see it: `public%2Csecret` is two
    // indices to the handler and must be two to the judge
    let first = percent_encoding::percent_decode_str(trimmed.split('/').next().unwrap_or(""))
        .decode_utf8_lossy()
        .to_string();
    let first = first.as_str();
    if first.is_empty() || (first.starts_with('_') && first != "_all") {
        return Vec::new();
    }
    first.split(',').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect()
}

/// Wildcards and aliases resolved to the concrete indices, so that a
/// pattern is judged by what it reaches; a name that reaches nothing is
/// judged as itself.
pub(crate) fn resolve_indices(store: &Store, exprs: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for e in exprs {
        let stripped = e.trim_start_matches('-').trim_start_matches('+');
        if e.starts_with('-') {
            continue;
        }
        let found = store.resolve(stripped);
        if found.is_empty() {
            out.push(stripped.to_string());
        } else {
            out.extend(found);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The transport action a REST request stands for.
pub fn action_for(method: &Method, path: &str) -> Option<String> {
    let p = path.trim_end_matches('/');
    let segs: Vec<&str> = p.trim_start_matches('/').split('/').collect();
    let first = segs.first().copied().unwrap_or("");
    let has_index = !first.is_empty() && (!first.starts_with('_') || first == "_all");
    let rest: Vec<&str> = if has_index { segs[1..].to_vec() } else { segs.clone() };
    let tail = rest.first().copied().unwrap_or("");
    let m = method.as_str();
    // a plugin's routes name their plugin first; none of them may run
    // unjudged, so an unknown plugin is judged under its own name and a
    // role that does not grant it does not reach it
    if !has_index && tail == "_plugins" {
        let plugin = rest.get(1).copied().unwrap_or("");
        let a = match (plugin, m) {
            // the query languages name their index in the body; the handler
            // judges that index itself, and this judges the caller may
            // search at all
            ("_sql" | "_ppl", _) => match rest.get(2).copied() {
                // the stats of a plugin are the cluster's business, not a
                // search: a caller with no index permissions was reading them
                Some("stats") => "cluster:monitor/stats".to_string(),
                _ => "indices:data/read/search".to_string(),
            },
            // The policies themselves are the cluster's; attaching one to an
            // index, taking it off, changing it or asking after it are
            // things done *to an index*, and judging them all as a policy
            // write meant the index was never judged at all: a caller with
            // the ISM cluster permission and no index permission could
            // attach a policy whose first action is `delete` to `*`.
            ("_ism", _) => match rest.get(2).copied().unwrap_or("") {
                "add" => "indices:admin/opendistro/ism/managedindex/add".to_string(),
                "remove" => "indices:admin/opendistro/ism/managedindex/remove".to_string(),
                "change_policy" => "indices:admin/opendistro/ism/managedindex/change".to_string(),
                "retry" => "indices:admin/opendistro/ism/managedindex/retry".to_string(),
                "explain" => "indices:monitor/opendistro/ism/managedindex/explain".to_string(),
                _ if m == "GET" || m == "HEAD" => {
                    "cluster:admin/opendistro/ism/policy/get".to_string()
                }
                _ => "cluster:admin/opendistro/ism/policy/write".to_string(),
            },
            // the plugin's own transport actions for its jobs, so a role
            // written for the reference grants the same things here
            ("_transform", _) => {
                let tail = rest.last().copied().unwrap_or("");
                let verb = match (tail, m) {
                    ("_start", _) => "start",
                    ("_stop", _) => "stop",
                    ("_explain", _) => "explain",
                    ("_preview", _) => "preview",
                    (_, "DELETE") => "delete",
                    ("_transform", "GET" | "HEAD") => "get_transforms",
                    (_, "GET" | "HEAD") => "get",
                    _ => "index",
                };
                format!("cluster:admin/opendistro/transform/{verb}")
            }
            ("_rollup", _) => {
                let tail = rest.last().copied().unwrap_or("");
                let verb = match (tail, m) {
                    ("_start", _) => "start",
                    ("_stop", _) => "stop",
                    ("_explain", _) => "explain",
                    (_, "DELETE") => "delete",
                    ("jobs", "GET" | "HEAD") => "search",
                    (_, "GET" | "HEAD") => "get",
                    _ => "index",
                };
                format!("cluster:admin/opendistro/rollup/{verb}")
            }
            // warming an index's vectors is done to that index, not to the
            // plugin's statistics
            ("_knn", _) if rest.get(2) == Some(&"warmup") => "indices:admin/knn/warmup".to_string(),
            ("_knn", "GET" | "HEAD") => "cluster:admin/knn/stats".to_string(),
            ("_knn", _) => "cluster:admin/knn/model/write".to_string(),
            // a submitted search is judged here as the plugin's own action,
            // and its index again by the handler, as the search it runs
            ("_asynchronous_search", _) => match (rest.get(2).copied(), m) {
                (Some("stats"), _) => "cluster:admin/opendistro/asynchronous_search/stats",
                (None, _) => "cluster:admin/opendistro/asynchronous_search/submit",
                (Some(_), "DELETE") => "cluster:admin/opendistro/asynchronous_search/delete",
                (Some(_), _) => "cluster:admin/opendistro/asynchronous_search/get",
            }
            .to_string(),
            // the plugins whose shipped roles name an action exactly: judged
            // under that action, so a role written for the reference grants
            // the same reads here. The rest fall through to the arm below,
            // which judges a plugin under its own name.
            ("_ltr", "GET" | "HEAD") => "cluster:admin/ltr/stats".to_string(),
            ("_im", "GET" | "HEAD") => {
                "cluster:admin/opensearch/controlcenter/lron/get".to_string()
            }
            ("_notifications", "GET" | "HEAD") if rest.get(2) == Some(&"features") => {
                "cluster:admin/opensearch/notifications/features".to_string()
            }
            ("_flow_framework", "GET" | "HEAD") => {
                "cluster:admin/opensearch/flow_framework/workflow_step/get".to_string()
            }
            ("_alerting", "GET" | "HEAD") if rest.get(2) == Some(&"stats") => {
                "cluster:admin/opendistro/alerting/stats".to_string()
            }
            ("_query", "GET" | "HEAD") => {
                "cluster:admin/opensearch/ql/datasources/read".to_string()
            }
            ("_query", _) => "cluster:admin/opensearch/ql/datasources/write".to_string(),
            (other, _) => format!("cluster:admin/plugins/{}", other.trim_start_matches('_')),
        };
        return Some(a);
    }
    // `/_reindex/{id}/_rethrottle`, and the same for the two by-query jobs:
    // changing the speed of a running task is the cluster's business, not the
    // index's. Judged by its first segment it was `indices:data/write/reindex`
    // on a request that names no index, which every authenticated caller
    // passed -- a caller with no roles at all could rethrottle anyone's job.
    if rest.last().copied() == Some("_rethrottle") {
        return Some("cluster:admin/reindex/rethrottle".to_string());
    }
    let a = match (has_index, tail, m) {
        (false, "", _) => "cluster:monitor/main",
        // `/_search/pipeline/{name}` only begins with `_search`: judging it
        // by that first segment made writing one a *read* permission, and
        // the arm written for it further down was never reached
        (false, "_search", _) if rest.get(1) == Some(&"pipeline") => match m {
            "GET" | "HEAD" => "cluster:admin/search/pipeline/get",
            "DELETE" => "cluster:admin/search/pipeline/delete",
            _ => "cluster:admin/search/pipeline/put",
        },
        // continuing a scroll, or letting one go, is the cluster's business:
        // the plugin judges it as the scroll action, not as a search
        (false, "_search", "DELETE") if rest.get(1) == Some(&"scroll") => {
            "indices:data/read/scroll/clear"
        }
        (false, "_search", _) if rest.get(1) == Some(&"scroll") => "indices:data/read/scroll",
        (_, "_search", _) => "indices:data/read/search",
        (_, "_msearch", _) => "indices:data/read/msearch",
        (_, "_count", _) => "indices:data/read/search",
        (_, "_explain", _) => "indices:data/read/explain",
        (_, "_search_shards", _) => "indices:admin/shards/search_shards",
        (_, "_field_caps", _) => "indices:data/read/field_caps",
        (_, "_validate", _) => "indices:admin/validate/query",
        (_, "_termvectors", _) => "indices:data/read/tv",
        (_, "_mtermvectors", _) => "indices:data/read/mtv",
        (_, "_mget", _) => "indices:data/read/mget",
        (_, "_bulk", _) => "indices:data/write/bulk",
        (_, "_delete_by_query", _) => "indices:data/write/delete/byquery",
        (_, "_update_by_query", _) => "indices:data/write/update/byquery",
        (false, "_reindex", _) => "indices:data/write/reindex",
        (_, "_doc", "GET" | "HEAD") => "indices:data/read/get",
        (_, "_source", _) => "indices:data/read/get",
        (_, "_doc", "DELETE") => "indices:data/write/delete",
        (_, "_doc", _) => "indices:data/write/index",
        (_, "_create", _) => "indices:data/write/index",
        (_, "_update", _) => "indices:data/write/update",
        (_, "_mapping", "GET" | "HEAD") => "indices:admin/mappings/get",
        (_, "_mappings", "GET" | "HEAD") => "indices:admin/mappings/get",
        (_, "_mapping", _) => "indices:admin/mapping/put",
        (_, "_mappings", _) => "indices:admin/mapping/put",
        (_, "_settings", "GET") => "indices:monitor/settings/get",
        (_, "_settings", _) => "indices:admin/settings/update",
        (_, "_alias" | "_aliases", "GET" | "HEAD") => "indices:admin/aliases/get",
        (_, "_alias" | "_aliases", _) => "indices:admin/aliases",
        (_, "_refresh", _) => "indices:admin/refresh",
        (_, "_flush", _) => "indices:admin/flush",
        (_, "_forcemerge", _) => "indices:admin/forcemerge",
        (_, "_cache", _) => "indices:admin/cache/clear",
        (_, "_open", _) => "indices:admin/open",
        (_, "_close", _) => "indices:admin/close",
        (_, "_stats", _) => "indices:monitor/stats",
        (_, "_segments", _) => "indices:monitor/segments",
        (_, "_recovery", _) => "indices:monitor/recovery",
        (_, "_shard_stores", _) => "indices:monitor/shard_stores",
        (_, "_analyze", _) => "indices:admin/analyze",
        (_, "_rollover", _) => "indices:admin/rollover",
        (_, "_shrink", _) => "indices:admin/resize",
        (_, "_split", _) => "indices:admin/resize",
        (_, "_clone", _) => "indices:admin/resize",
        (_, "_block", _) => "indices:admin/block/add",
        (_, "_rank_eval", _) => "indices:data/read/search",
        (_, "_pit", "DELETE") => "indices:data/read/point_in_time/delete",
        (_, "_pit", _) => "indices:data/read/point_in_time/create",
        (_, "_search_pipeline", _) => "cluster:admin/search/pipeline/get",
        (true, "", "GET" | "HEAD") => "indices:admin/get",
        // `POST /{index}` is the same door as `PUT /{index}`; it used to fall
        // through to the document write below, so the `write` action group
        // created indices
        (true, "", "PUT" | "POST") => "indices:admin/create",
        (true, "", "DELETE") => "indices:admin/delete",
        (true, _, "GET" | "HEAD") if !tail.starts_with('_') => "indices:data/read/get",
        (true, _, "DELETE") if !tail.starts_with('_') => "indices:data/write/delete",
        (true, _, _) if !tail.starts_with('_') => "indices:data/write/index",
        (false, "_cluster", _) => match rest.get(1).copied().unwrap_or("") {
            // the weights a zone's shards are searched by, and taking a zone
            // out of service: each is its own permission in the reference,
            // and judging them all as `cluster:monitor/state` let a caller
            // with a monitoring role weigh a zone to nothing
            "routing" if m == "GET" => "cluster:admin/routing/awareness/weights/get",
            "routing" if m == "DELETE" => "cluster:admin/routing/awareness/weights/delete",
            "routing" => "cluster:admin/routing/awareness/weights/put",
            "decommission" if m == "GET" => "cluster:admin/decommission/awareness/get",
            "decommission" if m == "DELETE" => "cluster:admin/decommission/awareness/delete",
            "decommission" => "cluster:admin/decommission/awareness/put",
            // the old spelling of `_nodes`, which the hot threads API names
            "nodes" => "cluster:monitor/nodes/hot_threads",
            "health" => "cluster:monitor/health",
            "state" => "cluster:monitor/state",
            "stats" => "cluster:monitor/stats",
            "settings" if m == "GET" => "cluster:admin/settings/get",
            "settings" => "cluster:admin/settings/update",
            "pending_tasks" => "cluster:monitor/task",
            "allocation" => "cluster:admin/allocation/explain",
            "reroute" => "cluster:admin/reroute",
            "voting_config_exclusions" => "cluster:admin/voting_config/add_exclusions",
            _ => "cluster:monitor/state",
        },
        (false, "_nodes", _) => "cluster:monitor/nodes/info",
        // The metrics endpoint answers what `_nodes/stats` and
        // `_cluster/health` answer, so it is judged as what it is: a read of
        // the cluster's statistics. Unjudged it was neither -- an
        // authenticated caller with no permission at all scraped every index
        // name and count out of a node.
        (false, "_prometheus", _) => "cluster:monitor/stats",
        // Everything under `_cat` that reads an index is an index action.
        // The fallback used to make them all `cluster:monitor/state`, so a
        // monitoring identity with no index permission read every index's
        // field names out of `_cat/fielddata`.
        (false, "_cat", _) => match rest.get(1).copied().unwrap_or("") {
            "indices" => "indices:monitor/settings/get",
            "aliases" => "indices:admin/aliases/get",
            "shards" | "segments" | "count" | "recovery" | "fielddata" | "docs" | "store" => {
                "indices:monitor/stats"
            }
            "snapshots" => "cluster:admin/snapshot/get",
            "repositories" => "cluster:admin/repository/get",
            "templates" => "indices:admin/template/get",
            _ => "cluster:monitor/state",
        },
        // forgetting a finished task's record is a write, not monitoring
        (false, "_tasks", "DELETE") if rest.last() != Some(&"_cancel") => {
            "cluster:admin/tasks/delete"
        }
        (false, "_tasks", _) => "cluster:monitor/task",
        // index data on disk that the cluster does not claim: listing it is
        // monitoring, bringing it in or throwing it away is not
        (false, "_dangling", "GET" | "HEAD") => "cluster:admin/indices/dangling/list",
        (false, "_dangling", "DELETE") => "cluster:admin/indices/dangling/delete",
        (false, "_dangling", _) => "cluster:admin/indices/dangling/import",
        // a shard's remote segment store: recovering from it writes indices
        (false, "_remotestore", _) if rest.get(1) == Some(&"stats") => {
            "cluster:monitor/_remotestore/stats"
        }
        (false, "_remotestore", _) => "cluster:admin/remotestore/restore",
        (false, "_template", "GET" | "HEAD") => "indices:admin/template/get",
        (false, "_template", "DELETE") => "indices:admin/template/delete",
        (false, "_template", _) => "indices:admin/template/put",
        (false, "_index_template", "GET" | "HEAD") => "indices:admin/index_template/get",
        (false, "_index_template", "DELETE") => "indices:admin/index_template/delete",
        (false, "_index_template", _) => "indices:admin/index_template/put",
        (false, "_component_template", "GET" | "HEAD") => "cluster:admin/component_template/get",
        (false, "_component_template", "DELETE") => "cluster:admin/component_template/delete",
        (false, "_component_template", _) => "cluster:admin/component_template/put",
        (false, "_ingest", "GET") => "cluster:admin/ingest/pipeline/get",
        (false, "_ingest", "DELETE") => "cluster:admin/ingest/pipeline/delete",
        (false, "_ingest", _)
            if rest.get(2) == Some(&"_simulate") || rest.get(1) == Some(&"_simulate") =>
        {
            "cluster:admin/ingest/pipeline/simulate"
        }
        (false, "_ingest", _) => "cluster:admin/ingest/pipeline/put",
        // `/_scripts/painless/_execute` compiles and runs what it is given,
        // by GET as well as by POST: reading a stored script is not that
        (false, "_scripts", _)
            if rest.get(1) == Some(&"painless") && rest.get(2) == Some(&"_execute") =>
        {
            "cluster:admin/scripts/painless/execute"
        }
        (false, "_scripts", "GET") => "cluster:admin/script/get",
        (false, "_scripts", "DELETE") => "cluster:admin/script/delete",
        (false, "_scripts", _) => "cluster:admin/script/put",
        (false, "_data_stream", "GET") => "indices:admin/data_stream/get",
        (false, "_data_stream", "DELETE") => "indices:admin/data_stream/delete",
        (false, "_data_stream", _) => "indices:admin/data_stream/create",
        // A restore writes indices into the cluster and lets the caller
        // choose their names: it is not the permission for taking a backup.
        (false, "_snapshot", _) if rest.last() == Some(&"_restore") => {
            "cluster:admin/snapshot/restore"
        }
        (false, "_snapshot", _) if rest.last() == Some(&"_status") => {
            "cluster:admin/snapshot/status"
        }
        (false, "_snapshot", "GET") => "cluster:admin/snapshot/get",
        (false, "_snapshot", "DELETE") => "cluster:admin/snapshot/delete",
        (false, "_snapshot", _) => "cluster:admin/snapshot/create",
        // query insights answers on a path of its own rather than under
        // `_plugins`, and its shipped role grants the top-queries actions;
        // without an arm here the reads would have run unjudged
        (false, "_insights", _) => "cluster:admin/opensearch/insights/top_queries/get",
        (false, "_render", _) => "cluster:admin/script/get",
        (false, "_resolve", _) => "indices:admin/resolve/index",
        _ => return None,
    };
    Some(a.to_string())
}

/// The concrete indices an expression names for a judgement: `_all`, `*`
/// or nothing at all is every index there is.
pub fn indices_for_expr(store: &Store, expr: &str) -> Vec<String> {
    let named: Vec<String> =
        expr.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if named.is_empty() || named.iter().any(|n| n == "_all" || n == "*") {
        let mut all = store.resolve("*");
        all.sort();
        return all;
    }
    resolve_indices(store, &named)
}

tokio::task_local! {
    /// The filter each index of this request carries through the aliases the
    /// request named. Set once, where the request's expression is known, and
    /// read wherever a query is built for one index -- which is the only
    /// place that knows which index it is building for.
    pub static ALIAS_FILTERS: std::collections::BTreeMap<String, serde_json::Value>;
}

/// The alias filter this index is under for the request being answered.
pub fn alias_filter_for(index: &str) -> Option<serde_json::Value> {
    ALIAS_FILTERS.try_with(|f| f.get(index).cloned()).ok().flatten()
}

/// Run `f` with the filters the expression's aliases impose.
///
/// Nothing set means nothing narrowed: a path that has not been taught about
/// alias filters answers as it always did rather than silently dropping them.
pub async fn under_alias_filters<T>(
    store: &crate::store::Store,
    expr: &str,
    f: impl std::future::Future<Output = T>,
) -> T {
    let filters = store.alias_filters(expr);
    if filters.is_empty() {
        return f.await;
    }
    ALIAS_FILTERS.scope(filters, f).await
}
