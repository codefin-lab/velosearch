//! The server the OpenSearch Dashboards front end talks to.
//!
//! It is a separate program from the engine for the same reason the one it
//! replaces is: they are deployed apart, on different machines as often as
//! not, and a console that has to run beside its engine is a worse console.
//!
//!   VELOSEARCH_CONSOLE_ADDR       where to listen (default 127.0.0.1:5601)
//!   VELOSEARCH_CONSOLE_PATH       an OpenSearch Dashboards distribution
//!   VELOSEARCH_CONSOLE_BASE_PATH  the path everything is served under
//!   VELOSEARCH_CONSOLE_BRANDING   `opensearch` leaves the distribution's own
//!                                 name, marks and colours in place
//!   VELOSEARCH_ENGINE             the engine behind it
//!   VELOSEARCH_CONSOLE_OVERRIDE   `key=value` pairs, comma separated: settings
//!                                  an operator fixes and no reader may change
//!
//! The distribution is pointed at rather than carried, the way the geoip
//! databases are: it is the OpenSearch project's to publish, it is a gigabyte,
//! and which one is in front of this server is an operator's decision.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::Value;
use velosearch::console::Console;
use velosearch::console::engine::{Engine, Failed, path_segment};
use velosearch::console::metrics::Metrics;
use velosearch::console::saved::{Looking, Saved, Writing};
use velosearch::console::settings::Settings;

/// Everything a handler needs: what to serve, and what to serve it from.
struct Serving {
    console: Console,
    engine: Engine,
    metrics: Metrics,
    /// where this listens, for the stats route to say so
    addr: String,
    /// the referrers compressed answers may go to; empty for any
    compression_referrers: Vec<String>,
    /// whether a request that changes something has to carry the
    /// `osd-xsrf` header, which a page from another origin cannot add
    xsrf: bool,
    /// the engine paths the Dev Tools proxy carries, as regular
    /// expressions; `console.proxyFilter` in the Node server
    proxy_filter: Vec<regex::Regex>,
}

type Shared = Arc<Serving>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr =
        std::env::var("VELOSEARCH_CONSOLE_ADDR").unwrap_or_else(|_| "127.0.0.1:5601".to_string());
    let home = std::env::var("VELOSEARCH_CONSOLE_PATH").unwrap_or_default();
    if home.is_empty() {
        eprintln!(
            "VELOSEARCH_CONSOLE_PATH is not set. It is an OpenSearch Dashboards\n\
             distribution -- the front end this serves, which is theirs rather than\n\
             ours. In a container it is /usr/share/opensearch-dashboards."
        );
        std::process::exit(2);
    }
    let base_path = std::env::var("VELOSEARCH_CONSOLE_BASE_PATH").unwrap_or_default();
    let base_path = base_path.trim_end_matches('/').to_string();
    let console = match Console::open(
        home.into(),
        std::path::Path::new("console"),
        base_path,
        velosearch::console::overrides_from(
            &std::env::var("VELOSEARCH_CONSOLE_OVERRIDE").unwrap_or_default(),
        ),
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let engine_url =
        std::env::var("VELOSEARCH_ENGINE").unwrap_or_else(|_| "http://127.0.0.1:9200".into());
    println!(
        "velosearch console: OpenSearch Dashboards {} ({} bundles) on {addr}, engine {engine_url}",
        console.pinned.version,
        console.pinned.bundles.len()
    );

    let build = console.pinned.build_number;
    let base = console.base_path.clone();
    let compression_referrers: Vec<String> =
        std::env::var("VELOSEARCH_CONSOLE_COMPRESSION_REFERRERS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    // on unless the operator turns it off, as `server.xsrf.disableProtection`
    // does in the Node server -- and its own suite needs it off
    let xsrf = std::env::var("VELOSEARCH_CONSOLE_XSRF").map(|v| v != "false").unwrap_or(true);
    let proxy_filter: Vec<regex::Regex> = std::env::var("VELOSEARCH_CONSOLE_PROXY_FILTER")
        .unwrap_or_else(|_| ".*".to_string())
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| regex::Regex::new(s).ok())
        .collect();
    let console = Arc::new(Serving {
        console,
        engine: Engine::at(&engine_url),
        metrics: Metrics::default(),
        addr: addr.clone(),
        compression_referrers,
        xsrf,
        proxy_filter,
    });
    let routes: Router<Shared> = Router::new()
        .route("/", get(root))
        .route("/app/{app}", get(page))
        .route("/app/{app}/{*rest}", get(page))
        .route("/bootstrap.js", get(bootstrap))
        .route("/startup.js", get(startup))
        .route(&format!("/{build}/bundles/{{*rest}}"), get(bundle))
        .route("/ui/{*rest}", get(ui_asset))
        .route("/plugins/{id}/assets/{*rest}", get(plugin_asset))
        .route("/node_modules/@osd/ui-framework/dist/{*rest}", get(ui_framework))
        .route("/translations/{locale}", get(translations))
        .route("/api/status", get(status))
        .route("/api/core/capabilities", post(capabilities))
        .route("/api/opensearch-dashboards/settings", get(read_settings).post(write_settings))
        .route(
            "/api/opensearch-dashboards/settings/{key}",
            post(write_setting).delete(reset_setting),
        )
        // the migration, asked for rather than done at startup. Anything
        // that has written to the console's index behind its back -- a
        // restore, a fixture loaded for a test -- says so this way, and the
        // index is made right again.
        .route("/internal/saved_objects/_migrate", post(migrate_now))
        .route("/api/saved_objects/_find", get(find))
        .route("/api/saved_objects/_export", post(export))
        .route("/api/saved_objects/_import", post(import))
        .route("/api/saved_objects/_resolve_import_errors", post(resolve_import_errors))
        .route(
            "/api/opensearch-dashboards/management/saved_objects/_allowed_types",
            get(allowed_types),
        )
        .route("/api/opensearch-dashboards/management/saved_objects/_find", get(management_find))
        .route(
            "/api/opensearch-dashboards/management/saved_objects/scroll/counts",
            post(scroll_counts),
        )
        .route(
            "/api/opensearch-dashboards/management/saved_objects/scroll/export",
            post(scroll_export),
        )
        .route(
            "/api/opensearch-dashboards/management/saved_objects/relationships/{kind}/{id}",
            get(relationships),
        )
        .route(
            "/api/opensearch-dashboards/management/saved_objects/{kind}/{id}",
            get(management_one),
        )
        .route("/api/index_patterns/_fields_for_wildcard", get(fields_for_wildcard))
        .route("/api/index_patterns/_fields_for_time_pattern", get(fields_for_time_pattern))
        .route("/internal/_msearch", post(msearch))
        .route("/internal/search/{strategy}", post(search_strategy))
        .route("/internal/search/{strategy}/{id}", post(search_strategy).delete(cancel_search))
        .route("/api/opensearch-dashboards/suggestions/values/{index}", post(suggestions))
        .route("/api/opensearch-dashboards/scripts/languages", get(script_languages))
        .route("/api/shorten_url", post(shorten_url))
        .route("/api/short_url/{id}", get(short_url))
        .route("/goto/{id}", get(goto))
        .route("/api/console/proxy", post(console_proxy))
        .route("/api/console/opensearch_config", get(opensearch_config))
        .route("/api/opensearch-dashboards/dql_opt_in_stats", post(dql_opt_in_stats))
        .route("/api/ui_metric/report", post(ui_metric_report))
        .route("/api/stats", get(stats))
        .route("/internal/index-pattern-management/resolve_index/{query}", get(resolve_index))
        .route(
            "/internal/index-pattern-management/preview_scripted_field",
            post(preview_scripted_field),
        )
        .route("/api/home/hits_status", post(hits_status))
        .route("/api/opensearch-dashboards/home/tutorials", get(tutorials))
        .route("/api/console/api_server", get(dev_tools_api))
        .route("/api/ism/_indices", get(ism_indices))
        .route("/api/ism/_data_streams", get(ism_data_streams))
        .route("/api/ism/apiCaller", post(ism_api_caller))
        .route("/api/ism/accountInfo", post(ism_api_caller))
        .route("/api/sample_data", get(sample_data_list))
        .route("/api/sample_data/{id}", post(sample_data_install).delete(sample_data_uninstall))
        .route("/api/saved_objects/_bulk_get", post(bulk_get))
        .route("/api/saved_objects/_bulk_create", post(bulk_create))
        .route("/api/saved_objects/_bulk_update", axum::routing::put(bulk_update))
        .route("/api/saved_objects/{kind}", post(create_auto))
        .route(
            "/api/saved_objects/{kind}/{id}",
            get(get_one).post(create_one).put(update_one).delete(delete_one),
        );
    // a base path is a prefix on every route, and the one route that is not
    // under it is the redirect that sends a reader to it
    let routes: Router<Shared> = match base.as_str() {
        "" => routes,
        base => Router::new().nest(base, routes).route("/", get(root)),
    };
    let app = routes
        .fallback(not_found)
        .layer(axum::middleware::from_fn_with_state(console.clone(), compressed))
        .layer(axum::middleware::from_fn(cookies_checked))
        .layer(axum::middleware::from_fn_with_state(console.clone(), xsrf_checked))
        .layer(axum::middleware::from_fn_with_state(console.clone(), counted))
        .with_state(console.clone());

    // the index everything is kept in, made if nothing has and moved on if
    // its shape has changed. A console that cannot do this can still serve
    // every page, so it says what happened and carries on rather than
    // refusing to start: an engine that is not up yet is the ordinary case
    // when both are started at once.
    {
        let engine = console.engine.clone();
        let mapping = console.console.pinned.saved_object_index.get("mappings").cloned();
        let found = tokio::task::spawn_blocking(move || {
            velosearch::console::migrate::ensure_because(
                &engine,
                &mapping.unwrap_or_default(),
                "startup",
            )
        })
        .await;
        if let Err(e) = found {
            eprintln!("  the console's index: {e}");
        }
    }

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// A reader who asked for nothing in particular is sent to the home page.
async fn root(State(console): State<Shared>) -> Response {
    redirect(&console.console.at("/app/home"))
}

fn redirect(to: &str) -> Response {
    (StatusCode::FOUND, [(header::LOCATION, to.to_string())]).into_response()
}

/// Every application is the same page. Which one it is is in the URL, and the
/// front end reads it from there.
async fn page(State(serving): State<Shared>) -> Response {
    // the settings are read for the page rather than fetched by it: a console
    // that drew itself with the default theme and then redrew with the chosen
    // one would flash white at every reader who did not want it. An engine
    // that cannot be reached is a page that still loads, with the defaults --
    // which is better than no page at all.
    let user = {
        let serving = serving.clone();
        tokio::task::spawn_blocking(move || settings_of(&serving).read())
            .await
            .ok()
            .and_then(|r| r.ok())
            .and_then(|found| found.get("settings").cloned())
            .unwrap_or_else(|| serde_json::json!({}))
    };
    let console = &serving.console;
    let body = console.page(user);
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (header::CONTENT_SECURITY_POLICY, console.content_security_policy()),
        ],
        body,
    )
        .into_response()
}

async fn bootstrap(State(console): State<Shared>) -> Response {
    script(console.console.bootstrap())
}

async fn startup(State(console): State<Shared>) -> Response {
    script(console.console.startup())
}

fn script(body: String) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/javascript; charset=utf-8"),
            // the boot script names the bundles, and the bundles are named
            // after the build they came from, so a stale one is a page that
            // loads files which are no longer there
            (header::CACHE_CONTROL, "must-revalidate"),
        ],
        body,
    )
        .into_response()
}

async fn bundle(
    State(console): State<Shared>,
    Path(rest): Path<String>,
    headers: HeaderMap,
) -> Response {
    // a bundle's name carries the build it came from, so it can be kept for
    // as long as the reader likes: a new build asks for a different URL
    served(console.console.bundle(&rest, accepts(&headers)), "public, max-age=31536000")
}

async fn ui_framework(
    State(console): State<Shared>,
    Path(rest): Path<String>,
    headers: HeaderMap,
) -> Response {
    served(console.console.ui_framework(&rest, accepts(&headers)), "public, max-age=3600")
}

async fn plugin_asset(
    State(console): State<Shared>,
    Path((id, rest)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    served(console.console.plugin_asset(&id, &rest, accepts(&headers)), "public, max-age=3600")
}

async fn ui_asset(
    State(console): State<Shared>,
    Path(rest): Path<String>,
    headers: HeaderMap,
) -> Response {
    // the brand's own marks and stylesheet are compiled in rather than read
    // from the distribution, and answer under `/ui/velosearch/`
    if let Some(name) = rest.strip_prefix("velosearch/") {
        use velosearch::console::brand;
        if name == "brand.css" {
            return served(
                Some(velosearch::console::assets::Served {
                    bytes: brand::stylesheet().into_bytes(),
                    kind: "text/css; charset=utf-8",
                    encoding: None,
                }),
                "public, max-age=3600",
            );
        }
        return served(
            brand::asset(name).map(|(bytes, kind)| velosearch::console::assets::Served {
                bytes: bytes.to_vec(),
                kind,
                encoding: None,
            }),
            "public, max-age=31536000",
        );
    }
    served(console.console.ui_asset(&rest, accepts(&headers)), "public, max-age=31536000")
}

async fn translations(State(console): State<Shared>, Path(locale): Path<String>) -> Response {
    let locale = locale.trim_end_matches(".json");
    served(Some(console.console.translations(locale)), "must-revalidate")
}

/// Whether the console can do its job, which is a question about the engine.
///
/// A console with no engine behind it can still serve every page and answer
/// nothing useful on any of them, so saying green because this process is
/// running would be the least helpful true statement available.
async fn status(State(serving): State<Shared>) -> Response {
    let console = &serving.console;
    let reachable = tokio::task::spawn_blocking({
        let engine = serving.engine.clone();
        move || engine.reachable()
    })
    .await;
    let (state, message) = match reachable {
        Ok(Ok(_)) => ("green", "OpenSearch is available".to_string()),
        Ok(Err(e)) => ("red", format!("OpenSearch is not available: {}", e.message)),
        Err(e) => ("red", format!("the check could not be run: {e}")),
    };
    let since = velosearch::console::now();
    let colour = |state: &str| match state {
        "green" => ("success", "secondary"),
        _ => ("alert", "danger"),
    };
    let (icon, ui_colour) = colour(state);
    axum::Json(serde_json::json!({
        "name": "velosearch-console",
        "uuid": console.uuid(),
        "version": {
            "number": console.pinned.version,
            "build_hash": console.pinned.env.pointer("/packageInfo/buildSha")
                .and_then(|v| v.as_str()).unwrap_or("unknown"),
            "build_number": console.pinned.build_number,
            "build_snapshot": false,
        },
        "status": {
            "overall": {
                "since": since,
                "state": state,
                "title": if state == "green" { "Green" } else { "Red" },
                "nickname": if state == "green" { "Looking good" } else { "Danger Will Robinson" },
                "icon": icon,
                "uiColor": ui_colour,
            },
            "statuses": [{
                "id": format!("core:opensearch@{}", console.pinned.version),
                "message": message,
                "since": since,
                "state": state,
                "icon": icon,
                "uiColor": ui_colour,
            }],
        },
        "metrics": serving.metrics.report(),
    }))
    .into_response()
}

/// Every request counted on its way through, for the status page.
async fn counted(
    State(serving): State<Shared>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let started = std::time::Instant::now();
    serving.metrics.arrived();
    let response = next.run(request).await;
    serving.metrics.answered(response.status().as_u16(), started.elapsed().as_millis() as u64);
    response
}

/// What a caller may do.
///
/// Most of it is what the plugins between them decided, which is pinned.
/// `navLinks` is not: it is one entry per application the caller asked about,
/// so it is the request's shape rather than the server's.
async fn capabilities(State(serving): State<Shared>, body: axum::Json<Value>) -> Response {
    let asked: Vec<String> = body
        .get("applications")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    axum::Json(serving.console.capabilities(&asked)).into_response()
}

fn settings_of(serving: &Serving) -> Settings<'_> {
    Settings::new(
        &serving.engine,
        &serving.console.pinned.version,
        serving.console.pinned.build_number,
        &serving.console.overrides,
        &serving.console.mapping,
    )
}

/// Reading and writing settings waits on the engine, and waiting on a socket
/// inside a handler holds a worker of the runtime -- so it happens off it.
async fn on_engine<F>(serving: Shared, work: F) -> Response
where
    F: FnOnce(&Serving) -> Result<Value, Failed> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || work(&serving)).await {
        Ok(Ok(found)) => axum::Json(found).into_response(),
        Ok(Err(e)) => refused(e),
        Err(e) => refused(Failed {
            objects: None,
            error: None,
            attributes: None,
            status: 500,
            message: format!("{e}"),
        }),
    }
}

fn refused(e: Failed) -> Response {
    let status = StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut body = serde_json::json!({
        "statusCode": e.status,
        "error": status.canonical_reason().unwrap_or("Error"),
        "message": e.message,
    });
    if let Some(objects) = e.objects {
        body["attributes"] = serde_json::json!({"objects": objects});
    }
    if let Some(error) = e.error {
        body["attributes"] = serde_json::json!({"error": *error});
    }
    if let Some(attributes) = e.attributes {
        body["attributes"] = *attributes;
    }
    (status, axum::Json(body)).into_response()
}

async fn read_settings(State(serving): State<Shared>) -> Response {
    on_engine(serving, |s| settings_of(s).read()).await
}

async fn write_settings(State(serving): State<Shared>, body: axum::Json<Value>) -> Response {
    let changes = body.get("changes").and_then(|v| v.as_object()).cloned().unwrap_or_default();
    on_engine(serving, move |s| settings_of(s).write(&changes)).await
}

async fn write_setting(
    State(serving): State<Shared>,
    Path(key): Path<String>,
    body: axum::Json<Value>,
) -> Response {
    let mut changes = serde_json::Map::new();
    changes.insert(key, body.get("value").cloned().unwrap_or(Value::Null));
    on_engine(serving, move |s| settings_of(s).write(&changes)).await
}

async fn reset_setting(State(serving): State<Shared>, Path(key): Path<String>) -> Response {
    on_engine(serving, move |s| settings_of(s).reset(&key)).await
}

fn accepts(headers: &HeaderMap) -> &str {
    headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).unwrap_or("")
}

fn served(found: Option<velosearch::console::assets::Served>, cache: &str) -> Response {
    let Some(found) = found else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, found.kind)
        .header(header::CACHE_CONTROL, cache);
    if let Some(encoding) = found.encoding {
        response = response.header(header::CONTENT_ENCODING, HeaderValue::from_static(encoding));
    }
    response.body(Body::from(found.bytes)).expect("a response with a body")
}

/// Put the console's index back into the shape it should be in.
///
/// Something that wrote to it directly may have left it as a plain index
/// where there should be an alias, or with a mapping that lets anything in.
/// This is the same walk that runs at startup, so whatever it finds it does
/// the right thing about.
async fn migrate_now(State(serving): State<Shared>) -> Response {
    let engine = serving.engine.clone();
    let mapping = serving.console.pinned.saved_object_index.get("mappings").cloned();
    let found = tokio::task::spawn_blocking(move || {
        velosearch::console::migrate::ensure_because(
            &engine,
            &mapping.unwrap_or_default(),
            "the migrate route",
        )
    })
    .await;
    match found {
        Ok(Ok(_)) => axum::Json(serde_json::json!({"success": true})).into_response(),
        Ok(Err(e)) => refused(e),
        Err(e) => refused(Failed {
            objects: None,
            error: None,
            attributes: None,
            status: 500,
            message: format!("{e}"),
        }),
    }
}

fn saved_of(serving: &Serving) -> Saved<'_> {
    Saved::new(
        &serving.engine,
        &serving.console.pinned.migration_versions,
        &serving.console.mapping,
    )
}

/// What a request asked to write, however it named it.
fn writing_of(kind: &str, id: Option<String>, body: &Value, overwrite: bool) -> Writing {
    Writing {
        kind: kind.to_string(),
        id,
        attributes: body.get("attributes").cloned().unwrap_or_else(|| serde_json::json!({})),
        references: body.get("references").cloned().unwrap_or_else(|| serde_json::json!([])),
        migration_version: body.get("migrationVersion").cloned(),
        overwrite,
    }
}

async fn get_one(
    State(serving): State<Shared>,
    Path((kind, id)): Path<(String, String)>,
) -> Response {
    on_engine(serving, move |s| saved_of(s).get(&kind, &id)).await
}

async fn create_one(
    State(serving): State<Shared>,
    Path((kind, id)): Path<(String, String)>,
    Query(p): Query<std::collections::HashMap<String, String>>,
    body: axum::Json<Value>,
) -> Response {
    let overwrite = p.get("overwrite").map(|v| v == "true").unwrap_or(false);
    let writing = writing_of(&kind, Some(id), &body, overwrite);
    on_engine(serving, move |s| saved_of(s).create(writing)).await
}

/// An object whose id the caller left to the server.
async fn create_auto(
    State(serving): State<Shared>,
    Path(kind): Path<String>,
    body: axum::Json<Value>,
) -> Response {
    let writing = writing_of(&kind, None, &body, false);
    on_engine(serving, move |s| saved_of(s).create(writing)).await
}

async fn update_one(
    State(serving): State<Shared>,
    Path((kind, id)): Path<(String, String)>,
    body: axum::Json<Value>,
) -> Response {
    let attributes = body.get("attributes").cloned().unwrap_or_else(|| serde_json::json!({}));
    let references = body.get("references").cloned();
    // the version the caller read the object at, where it gave one: what
    // keeps two editors of one dashboard from overwriting each other
    let version = body.get("version").and_then(|v| v.as_str()).map(String::from);
    on_engine(serving, move |s| {
        saved_of(s).update(&kind, &id, &attributes, references.as_ref(), version.as_deref())
    })
    .await
}

async fn delete_one(
    State(serving): State<Shared>,
    Path((kind, id)): Path<(String, String)>,
) -> Response {
    on_engine(serving, move |s| saved_of(s).delete(&kind, &id)).await
}

/// How many objects one of these routes may be asked for at a time. Each is
/// a call to the engine of its own, and an update waits for a refresh: an
/// array of forty thousand held a thread for hours.
const MOST_OBJECTS: usize = 1_000;

/// Whether a multi-object request is larger than this server will answer.
fn too_many(asked: &[Value]) -> Option<Response> {
    (asked.len() > MOST_OBJECTS).then(|| {
        let body = serde_json::json!({
            "statusCode": 400,
            "error": "Bad Request",
            "message": format!(
                "Too many objects in one request: [{}], the most is [{MOST_OBJECTS}]",
                asked.len()
            ),
        });
        (StatusCode::BAD_REQUEST, axum::Json(body)).into_response()
    })
}

async fn bulk_get(State(serving): State<Shared>, body: axum::Json<Value>) -> Response {
    let asked = body.as_array().cloned().unwrap_or_default();
    if let Some(r) = too_many(&asked) {
        return r;
    }
    on_engine(serving, move |s| saved_of(s).bulk_get(&asked)).await
}

/// Several objects written at once, each answered for on its own.
async fn bulk_create(
    State(serving): State<Shared>,
    Query(p): Query<std::collections::HashMap<String, String>>,
    body: axum::Json<Value>,
) -> Response {
    let overwrite = p.get("overwrite").map(|v| v == "true").unwrap_or(false);
    let asked = body.as_array().cloned().unwrap_or_default();
    if let Some(r) = too_many(&asked) {
        return r;
    }
    on_engine(serving, move |s| {
        let saved = saved_of(s);
        let writings: Vec<_> = asked
            .iter()
            .map(|one| {
                let kind = one.get("type").and_then(|v| v.as_str()).unwrap_or_default();
                let id = one.get("id").and_then(|v| v.as_str()).map(String::from);
                writing_of(kind, id, one, overwrite)
            })
            .collect();
        let mut out = Vec::new();
        for (one, answer) in asked.iter().zip(saved.bulk_create(writings)?) {
            let kind = one.get("type").and_then(|v| v.as_str()).unwrap_or_default();
            let named = one.get("id").and_then(|v| v.as_str()).unwrap_or_default();
            match answer {
                Ok(found) => out.push(found),
                // one that could not be written is reported where it stood,
                // so a caller writing ten knows which of them failed -- and
                // a conflict is said in a few words here, where a single
                // create says it in the engine's
                Err(e) if e.status == 409 => out.push(serde_json::json!({
                    "id": named, "type": kind,
                    "error": {"statusCode": 409, "error": "Conflict",
                              "message": format!("Saved object [{kind}/{named}] conflict")},
                })),
                Err(e) => out.push(serde_json::json!({
                    "id": named, "type": kind,
                    "error": {"statusCode": e.status, "message": e.message},
                })),
            }
        }
        Ok(serde_json::json!({"saved_objects": out}))
    })
    .await
}

async fn bulk_update(State(serving): State<Shared>, body: axum::Json<Value>) -> Response {
    let asked = body.as_array().cloned().unwrap_or_default();
    if let Some(r) = too_many(&asked) {
        return r;
    }
    on_engine(serving, move |s| {
        let saved = saved_of(s);
        let mut out = Vec::new();
        for one in &asked {
            let kind = one.get("type").and_then(|v| v.as_str()).unwrap_or_default();
            let id = one.get("id").and_then(|v| v.as_str()).unwrap_or_default();
            let attributes =
                one.get("attributes").cloned().unwrap_or_else(|| serde_json::json!({}));
            let version = one.get("version").and_then(|v| v.as_str());
            match saved.update(kind, id, &attributes, one.get("references"), version) {
                Ok(found) => out.push(found),
                Err(e) => out.push(serde_json::json!({
                    "id": id, "type": kind,
                    "error": {
                        "statusCode": e.status,
                        "error": StatusCode::from_u16(e.status).ok()
                            .and_then(|s| s.canonical_reason()).unwrap_or("Error"),
                        "message": e.message,
                    },
                })),
            }
        }
        Ok(serde_json::json!({"saved_objects": out}))
    })
    .await
}

async fn find(
    State(serving): State<Shared>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let mut looking = looking_from(query.as_deref().unwrap_or_default());
    if looking.types.is_empty() {
        return refused(Failed {
            objects: None,
            error: None,
            attributes: None,
            status: 400,
            message:
                "[request query.type]: expected at least one defined value but got [undefined]"
                    .into(),
        });
    }
    if let Some(filter) = looking.filter.take() {
        match velosearch::console::filter::parse(&filter, &looking.types) {
            Ok(query) => looking.filter_query = Some(query),
            Err(message) => {
                return refused(Failed {
                    objects: None,
                    error: None,
                    attributes: None,
                    status: 400,
                    message,
                });
            }
        }
    }
    on_engine(serving, move |s| saved_of(s).find(&looking)).await
}

/// What a query string asked to look for.
///
/// A parameter that may be given more than once -- `type`, `fields` -- is a
/// list, which is why this reads the query itself rather than taking a map:
/// a map keeps one of them and the caller asked about all of them.
fn looking_from(query: &str) -> Looking {
    let mut looking = Looking::default();
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        let value = value.to_string();
        match key.as_ref() {
            "type" => looking.types.push(value),
            "fields" => looking.fields.push(value),
            "search_fields" => looking.search_fields.push(value),
            "search" => looking.search = Some(value),
            "page" => looking.page = value.parse().unwrap_or(1),
            "per_page" => looking.per_page = value.parse().unwrap_or(20),
            "sort_field" => looking.sort_field = Some(value),
            "sort_order" => looking.sort_order = Some(value),
            "default_search_operator" => looking.default_search_operator = value,
            "has_reference" => looking.has_reference = serde_json::from_str(&value).ok(),
            "namespaces" => looking.namespaces.push(value),
            "filter" => looking.filter = Some(value),
            _ => {}
        }
    }
    looking
}

fn management_of(serving: &Serving) -> velosearch::console::management::Management<'_> {
    velosearch::console::management::Management {
        saved: saved_of(serving),
        engine: &serving.engine,
        meta: &serving.console.pinned.management_meta,
        allowed: &serving.console.pinned.allowed_types,
    }
}

/// An export is a file, not a document: a line per object, read back a line
/// at a time.
async fn export(State(serving): State<Shared>, body: String) -> Response {
    let body: Value = match serde_json::from_str::<Value>(&body) {
        Ok(v) if v.is_object() => v,
        _ => {
            return refused(Failed {
                objects: None,
                error: None,
                attributes: None,
                status: 400,
                message: "[request body]: expected a plain object value, but found [null] instead."
                    .into(),
            });
        }
    };
    let types: Vec<String> = listed(body.get("type"));
    let objects = body.get("objects").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let include = body.get("includeReferencesDeep").and_then(|v| v.as_bool()).unwrap_or(false);
    let exclude = body.get("excludeExportDetails").and_then(|v| v.as_bool()).unwrap_or(false);
    let found = tokio::task::spawn_blocking(move || {
        velosearch::console::management::export(
            &management_of(&serving),
            &types,
            &objects,
            include,
            exclude,
        )
    })
    .await;
    match found {
        Ok(Ok(lines)) => {
            // no newline after the last line: a reader that splits on them
            // and parses each piece would find an empty piece and fail on it
            let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "application/ndjson"),
                    (header::CONTENT_DISPOSITION, "attachment; filename=\"export.ndjson\""),
                ],
                text.join("\n"),
            )
                .into_response()
        }
        Ok(Err(e)) => refused(e),
        Err(e) => refused(Failed {
            objects: None,
            error: None,
            attributes: None,
            status: 500,
            message: format!("{e}"),
        }),
    }
}

async fn import(
    State(serving): State<Shared>,
    Query(p): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    // an import is a file, sent as one: the front end uploads it as a form,
    // and anything else is not the request this answers
    if !is_form_upload(&headers) {
        return refused(Failed {
            objects: None,
            error: None,
            attributes: None,
            status: 415,
            message: "Unsupported Media Type".into(),
        });
    }
    let overwrite = p.get("overwrite").map(|v| v == "true").unwrap_or(false);
    let Some(lines) = file_part(&body) else {
        return refused(Failed {
            objects: None,
            error: None,
            attributes: None,
            status: 400,
            message: "[request body.file]: expected value of type [Stream] but got [undefined]"
                .into(),
        });
    };
    on_engine(serving, move |s| {
        velosearch::console::management::import(&management_of(s), &lines, overwrite, None)
    })
    .await
}

fn is_form_upload(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.starts_with("multipart/form-data"))
}

/// The reader has been shown the conflicts and said what to do about each.
async fn resolve_import_errors(
    State(serving): State<Shared>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !is_form_upload(&headers) {
        return refused(Failed {
            objects: None,
            error: None,
            attributes: None,
            status: 415,
            message: "Unsupported Media Type".into(),
        });
    }
    let Some(lines) = file_part(&body) else {
        return refused(Failed {
            objects: None,
            error: None,
            attributes: None,
            status: 400,
            message: "[request body.file]: expected value of type [Stream] but got [undefined]"
                .into(),
        });
    };
    let retries = retries_of(&body);
    on_engine(serving, move |s| {
        velosearch::console::management::import(&management_of(s), &lines, false, Some(&retries))
    })
    .await
}

/// The parts of a form upload, by the name each was sent under.
///
/// A multipart body is boundaries around parts, each with its own headers
/// and a blank line before its content. Nothing here is more than that: no
/// nested multiparts, no encodings, which is all the front end ever sends.
fn form_parts(body: &str) -> Vec<(String, String)> {
    let Some(boundary) = body.lines().next().filter(|l| l.starts_with("--")) else {
        return Vec::new();
    };
    let boundary = boundary.trim_end();
    let mut out = Vec::new();
    for part in body.split(boundary) {
        let part = part.trim_start_matches("\r\n").trim_start_matches('\n');
        if part.is_empty() || part.starts_with("--") {
            continue;
        }
        let Some((head, content)) = part.split_once("\r\n\r\n").or_else(|| part.split_once("\n\n"))
        else {
            continue;
        };
        let name = head
            .split(';')
            .map(str::trim)
            .find_map(|piece| piece.strip_prefix("name=\"").and_then(|n| n.strip_suffix('"')))
            .unwrap_or_default();
        let content = content.trim_end_matches("\r\n").trim_end_matches('\n');
        out.push((name.to_string(), content.to_string()));
    }
    out
}

/// The uploaded file, if the form carried one.
fn file_part(body: &str) -> Option<String> {
    if !body.starts_with("--") {
        return Some(body.to_string()).filter(|b| !b.trim().is_empty());
    }
    form_parts(body).into_iter().find(|(name, _)| name == "file").map(|(_, content)| content)
}

/// What the reader said to do about each conflict, by type and id.
fn retries_of(
    body: &str,
) -> std::collections::BTreeMap<(String, String), velosearch::console::management::Retry> {
    use velosearch::console::management::Retry;
    let raw = form_parts(body)
        .into_iter()
        .find(|(name, _)| name == "retries")
        .map(|(_, content)| content)
        .unwrap_or_else(|| "[]".into());
    let listed: Vec<Value> = serde_json::from_str(&raw).unwrap_or_default();
    listed
        .into_iter()
        .map(|one| {
            let key = (
                one.get("type").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                one.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            );
            let replace = one
                .get("replaceReferences")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|r| {
                    Some((
                        r.get("type")?.as_str()?.to_string(),
                        r.get("from")?.as_str()?.to_string(),
                        r.get("to")?.as_str()?.to_string(),
                    ))
                })
                .collect();
            let retry = Retry {
                overwrite: one.get("overwrite").and_then(|v| v.as_bool()).unwrap_or(false),
                destination: one.get("destinationId").and_then(|v| v.as_str()).map(String::from),
                replace,
            };
            (key, retry)
        })
        .collect()
}

fn listed(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

async fn allowed_types(State(serving): State<Shared>) -> Response {
    axum::Json(management_of(&serving).allowed_types()).into_response()
}

async fn scroll_counts(State(serving): State<Shared>, body: axum::Json<Value>) -> Response {
    let types = listed(body.get("typesToInclude"));
    let search = body.get("searchString").and_then(|v| v.as_str()).map(String::from);
    on_engine(serving, move |s| management_of(s).counts(&types, search.as_deref())).await
}

async fn scroll_export(State(serving): State<Shared>, body: axum::Json<Value>) -> Response {
    let types = listed(body.get("typesToInclude"));
    on_engine(serving, move |s| {
        velosearch::console::management::scroll_export(&saved_of(s), &types)
    })
    .await
}

async fn management_find(
    State(serving): State<Shared>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let raw = query.as_deref().unwrap_or_default();
    // the management page's find is stricter than the API's: it must be told
    // a type, and it does not take `searchFields` at all
    if raw.contains("searchFields=") {
        return refused(Failed {
            objects: None,
            error: None,
            attributes: None,
            status: 400,
            message: "[request query.searchFields]: definition for this key is missing".into(),
        });
    }
    let looking = looking_from(&management_query(raw));
    if looking.types.is_empty() {
        return refused(Failed {
            objects: None,
            error: None,
            attributes: None,
            status: 400,
            message:
                "[request query.type]: expected at least one defined value but got [undefined]"
                    .into(),
        });
    }
    on_engine(serving, move |s| management_of(s).find(&looking)).await
}

/// The management page spells two of its parameters differently.
fn management_query(query: &str) -> String {
    query.replace("perPage=", "per_page=").replace("sortField=", "sort_field=")
}

async fn management_one(
    State(serving): State<Shared>,
    Path((kind, id)): Path<(String, String)>,
) -> Response {
    on_engine(serving, move |s| management_of(s).one(&kind, &id)).await
}

async fn relationships(
    State(serving): State<Shared>,
    Path((kind, id)): Path<(String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let mut types = Vec::new();
    let mut size = 10_000u64;
    for (key, value) in form_urlencoded::parse(query.as_deref().unwrap_or_default().as_bytes()) {
        match key.as_ref() {
            "savedObjectTypes" => types.push(value.to_string()),
            "size" => size = value.parse().unwrap_or(10_000),
            _ => {}
        }
    }
    on_engine(serving, move |s| management_of(s).relationships(&kind, &id, &types, size)).await
}

// ---- what the pages ask for (13.4) -----------------------------------------

/// A query string as pairs, in order, so that a key given twice is a list.
fn query_pairs(query: Option<String>) -> Vec<(String, String)> {
    form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// The refusal the server being replaced gives a request whose query does
/// not fit its schema: the key named, and what was wrong with it.
fn bad_query(key: &str, what: &str) -> Response {
    refused(Failed::of(400, format!("[request query.{key}]: {what}")))
}

/// `meta_fields` as the route takes it: given several times it is a list,
/// given once it is a JSON list, and anything else is refused.
fn meta_fields_of(pairs: &[(String, String)]) -> Result<Vec<String>, Box<Response>> {
    let given: Vec<&String> =
        pairs.iter().filter(|(k, _)| k == "meta_fields").map(|(_, v)| v).collect();
    match given.len() {
        0 => Ok(Vec::new()),
        1 => {
            let parsed: Result<Vec<String>, _> = serde_json::from_str(given[0]);
            parsed.map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "statusCode": 400, "error": "Bad Request", "message": "Bad Request",
                    })),
                )
                    .into_response()
                    .into()
            })
        }
        _ => Ok(given.into_iter().cloned().collect()),
    }
}

async fn fields_for_wildcard(
    State(serving): State<Shared>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let pairs = query_pairs(query);
    for (key, _) in &pairs {
        if !matches!(key.as_str(), "pattern" | "meta_fields" | "data_source") {
            return bad_query(key, "definition for this key is missing");
        }
    }
    let Some(pattern) = pairs.iter().find(|(k, _)| k == "pattern").map(|(_, v)| v.clone()) else {
        return bad_query("pattern", "expected value of type [string] but got [undefined]");
    };
    let meta_fields = match meta_fields_of(&pairs) {
        Ok(m) => m,
        Err(response) => return *response,
    };
    on_engine(serving, move |s| {
        velosearch::console::fields::for_wildcard(&s.engine, &pattern, &meta_fields)
            .map(|fields| serde_json::json!({"fields": fields}))
            .map_err(not_found_for_fields)
    })
    .await
}

/// A pattern nothing matches is a 404 whose attributes carry the code the
/// index-pattern page looks for; any other trouble is a plain 404, as the
/// server being replaced answers.
fn not_found_for_fields(e: Failed) -> Failed {
    if e.message.starts_with("No indices match pattern") {
        let message = e.message.clone();
        Failed::of(404, e.message).with_attributes(serde_json::json!({
            "statusCode": 404, "error": "Not Found", "message": message,
            "code": "no_matching_indices",
        }))
    } else {
        Failed::of(404, "Not Found")
    }
}

async fn fields_for_time_pattern(
    State(serving): State<Shared>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let pairs = query_pairs(query);
    for (key, _) in &pairs {
        if !matches!(
            key.as_str(),
            "pattern" | "interval" | "look_back" | "meta_fields" | "data_source"
        ) {
            return bad_query(key, "definition for this key is missing");
        }
    }
    let Some(pattern) = pairs.iter().find(|(k, _)| k == "pattern").map(|(_, v)| v.clone()) else {
        return bad_query("pattern", "expected value of type [string] but got [undefined]");
    };
    let look_back = match pairs.iter().find(|(k, _)| k == "look_back").map(|(_, v)| v) {
        None => {
            return bad_query("look_back", "expected value of type [number] but got [undefined]");
        }
        Some(text) => match text.parse::<f64>() {
            Err(_) => {
                return bad_query("look_back", "expected value of type [number] but got [string]");
            }
            Ok(n) if n < 1.0 => {
                return bad_query("look_back", "Value must be equal to or greater than [1].");
            }
            Ok(n) => n as usize,
        },
    };
    let meta_fields = match meta_fields_of(&pairs) {
        Ok(m) => m,
        Err(response) => return *response,
    };
    on_engine(serving, move |s| {
        velosearch::console::fields::for_time_pattern(&s.engine, &pattern, look_back, &meta_fields)
            .map(|fields| serde_json::json!({"fields": fields}))
            .map_err(|_| Failed::of(404, "Not Found"))
    })
    .await
}

async fn msearch(State(serving): State<Shared>, body: axum::Json<Value>) -> Response {
    let body = body.0;
    on_engine(serving, move |s| velosearch::console::search::msearch(&s.engine, &body)).await
}

async fn search_strategy(
    State(serving): State<Shared>,
    Path(params): Path<Vec<(String, String)>>,
    body: axum::Json<Value>,
) -> Response {
    let strategy =
        params.iter().find(|(k, _)| k == "strategy").map(|(_, v)| v.clone()).unwrap_or_default();
    // the second name is the same strategy with the answer's long numbers
    // kept whole on the way to the browser; what goes to the engine and
    // what comes back is the same
    if strategy != "opensearch" && strategy != "opensearch-with-long-numerals" {
        return refused(Failed::of(404, format!("Search strategy {strategy} not found")));
    }
    let body = body.0;
    on_engine(serving, move |s| velosearch::console::search::search(&s.engine, &body)).await
}

/// A search that cannot be cancelled -- the engine answers each in one
/// piece -- is answered for as the server being replaced answers: done.
async fn cancel_search() -> Response {
    StatusCode::OK.into_response()
}

async fn suggestions(
    State(serving): State<Shared>,
    Path(index): Path<String>,
    body: axum::Json<Value>,
) -> Response {
    let body = body.0;
    let Some(field) = body.get("field").and_then(|v| v.as_str()).map(String::from) else {
        return refused(Failed::of(
            400,
            "[request body.field]: expected value of type [string] but got [undefined]",
        ));
    };
    let Some(query) = body.get("query").and_then(|v| v.as_str()).map(String::from) else {
        return refused(Failed::of(
            400,
            "[request body.query]: expected value of type [string] but got [undefined]",
        ));
    };
    let bool_filter = body.get("boolFilter").cloned();
    on_engine(serving, move |s| {
        velosearch::console::search::suggestions(
            &s.engine,
            &saved_of(s),
            &index,
            &field,
            &query,
            bool_filter.as_ref(),
        )
    })
    .await
}

async fn script_languages() -> Response {
    axum::Json(serde_json::json!(["painless", "expression"])).into_response()
}

async fn shorten_url(State(serving): State<Shared>, body: axum::Json<Value>) -> Response {
    let Some(url) = body.get("url").and_then(|v| v.as_str()).map(String::from) else {
        return refused(Failed::of(
            400,
            "[request body.url]: expected value of type [string] but got [undefined]",
        ));
    };
    on_engine(serving, move |s| {
        velosearch::console::urls::shorten(&saved_of(s), &url)
            .map(|id| serde_json::json!({"urlId": id}))
    })
    .await
}

async fn short_url(State(serving): State<Shared>, Path(id): Path<String>) -> Response {
    on_engine(serving, move |s| {
        velosearch::console::urls::resolve(&saved_of(s), &id)
            .map(|url| serde_json::json!({"url": url}))
    })
    .await
}

/// The browser sent on to the long address -- or, where the operator keeps
/// state in session storage and the address is not the whole of it, the
/// application itself, which knows what to do with the id.
async fn goto(State(serving): State<Shared>, Path(id): Path<String>) -> Response {
    let resolved = tokio::task::spawn_blocking({
        let serving = serving.clone();
        move || {
            let url = velosearch::console::urls::resolve(&saved_of(&serving), &id)?;
            let in_session = settings_of(&serving)
                .read()
                .ok()
                .and_then(|found| {
                    found.pointer("/settings/state:storeInSessionStorage/userValue").cloned()
                })
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Ok::<_, Failed>((url, in_session))
        }
    })
    .await;
    match resolved {
        Ok(Ok((url, false))) => {
            let location = match url.starts_with('/') {
                true => format!("{}{url}", serving.console.base_path),
                false => url,
            };
            (StatusCode::FOUND, [(header::LOCATION, location)]).into_response()
        }
        Ok(Ok((_, true))) => page(State(serving)).await,
        Ok(Err(e)) => refused(e),
        Err(e) => refused(Failed::of(500, format!("{e}"))),
    }
}

/// The Dev Tools page's way to the engine: the request as typed, carried
/// through, and the answer as given.
async fn console_proxy(
    State(serving): State<Shared>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let pairs = query_pairs(query);
    let value = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
    let Some(method) = value("method") else {
        return bad_query("method", "expected value of type [string] but got [undefined]");
    };
    let method = method.to_ascii_uppercase();
    if !matches!(method.as_str(), "HEAD" | "GET" | "POST" | "PUT" | "DELETE") {
        return bad_query(
            "method",
            &format!(
                "Method must be one of, case insensitive ['HEAD', 'GET', 'POST', 'PUT', 'DELETE']. Received '{method}'."
            ),
        );
    }
    let path = match value("path") {
        Some(p) if !p.is_empty() => p,
        Some(_) => return bad_query("path", "Expected non-empty string"),
        None => return bad_query("path", "expected value of type [string] but got [undefined]"),
    };
    // the paths the operator lets the page reach, and no others
    if !serving.proxy_filter.iter().any(|re| re.is_match(&path)) {
        return (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "text/plain")],
            format!("Error connecting to '{path}':\n\nUnable to send requests to that path."),
        )
            .into_response();
    }
    // the engine's answer pretty-printed unless the caller said otherwise,
    // as the page shows it
    let mut path = format!("/{}", path.trim_start_matches('/'));
    if !path.contains("pretty=") {
        path.push(if path.contains('?') { '&' } else { '?' });
        path.push_str("pretty=true");
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let answered = tokio::task::spawn_blocking({
        let serving = serving.clone();
        move || serving.engine.raw(&method, &path, &body, &content_type).map(|a| (method, a))
    })
    .await;
    match answered {
        Ok(Ok((method, answer))) => {
            let status = StatusCode::from_u16(answer.status).unwrap_or(StatusCode::BAD_GATEWAY);
            let warning = answer.warning.unwrap_or_default();
            if method == "HEAD" {
                let text = format!("{} - {}", answer.status, String::from_utf8_lossy(&answer.body));
                return (
                    status,
                    [(header::CONTENT_TYPE, "text/plain".to_string()), (header::WARNING, warning)],
                    text,
                )
                    .into_response();
            }
            let json = answer.content_type.contains("application/json");
            let mut response = (status, answer.body).into_response();
            let headers = response.headers_mut();
            if json {
                headers.insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json; charset=utf-8"),
                );
            } else if let Ok(v) = HeaderValue::from_str(&answer.content_type) {
                headers.insert(header::CONTENT_TYPE, v);
            }
            if let Ok(v) = HeaderValue::from_str(&warning) {
                headers.insert(header::WARNING, v);
            }
            response
        }
        Ok(Err(e)) => (
            StatusCode::from_u16(e.status).unwrap_or(StatusCode::BAD_GATEWAY),
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::json!({"message": e.message}).to_string(),
        )
            .into_response(),
        Err(e) => refused(Failed::of(500, format!("{e}"))),
    }
}

async fn opensearch_config(State(serving): State<Shared>) -> Response {
    axum::Json(serde_json::json!({"host": serving.engine.host()})).into_response()
}

// ---- the plugin routes the pages need (13.5) --------------------------------

/// What the server being replaced says about a path it does not serve.
async fn not_found() -> Response {
    refused(Failed::of(404, "Not Found"))
}

/// A request that changes something has to say it came from the page: the
/// `osd-xsrf` header (or `osd-version`), which a script on another origin
/// cannot add to a request the browser will still send. Refused in the
/// words the server being replaced uses.
async fn xsrf_checked(
    State(serving): State<Shared>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let safe = matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    );
    let carried =
        request.headers().contains_key("osd-xsrf") || request.headers().contains_key("osd-version");
    if serving.xsrf && !safe && !carried {
        return refused(Failed::of(400, "Request must contain the osd-xsrf header."));
    }
    next.run(request).await
}

/// A cookie header that cannot be read is refused before anything looks at
/// it, in the words the server being replaced uses.
async fn cookies_checked(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Some(cookie) = request.headers().get(header::COOKIE) {
        let text = cookie.to_str().unwrap_or("=");
        let readable = text
            .split(';')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .all(|part| part.split_once('=').is_some_and(|(name, _)| !name.trim().is_empty()));
        if !readable {
            return refused(Failed::of(400, "Invalid cookie header"));
        }
    }
    next.run(request).await
}

/// Answers compressed for a caller that takes them, unless the page asking
/// is embedded somewhere the operator did not list.
async fn compressed(
    State(serving): State<Shared>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let wants_gzip = request
        .headers()
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|e| e.trim().starts_with("gzip")));
    let referrer_allowed =
        match request.headers().get(header::REFERER).and_then(|v| v.to_str().ok()) {
            None => true,
            Some(referrer) => {
                serving.compression_referrers.is_empty() || {
                    let host = referrer
                        .split("://")
                        .nth(1)
                        .unwrap_or(referrer)
                        .split(['/', ':', '?', '#'])
                        .next()
                        .unwrap_or("");
                    serving.compression_referrers.iter().any(|h| h == host)
                }
            }
        };
    let response = next.run(request).await;
    if !wants_gzip || !referrer_allowed || response.headers().contains_key(header::CONTENT_ENCODING)
    {
        return response;
    }
    // The largest answer this will hold in memory to compress. A search
    // through the console can answer with hundreds of megabytes, and this
    // used to read every one of them into a buffer and then build a second
    // buffer of the compressed copy -- twice the answer, per request in
    // flight. Anything larger is passed straight through: it costs the
    // caller bandwidth, not the node its memory.
    const MOST_TO_COMPRESS: usize = 8 * 1024 * 1024;
    let (mut parts, body) = response.into_parts();
    // an answer of unknown length is not read into memory to find out. The
    // length is rarely in a header at this point -- it is added on the way
    // out -- so the body is asked what it knows about itself, which for the
    // answers built here (a buffer of bytes) is exact.
    let known = {
        use axum::body::HttpBody as _;
        body.size_hint().exact().map(|n| n as usize).or_else(|| {
            parts
                .headers
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<usize>().ok())
        })
    };
    match known {
        // the server being replaced leaves anything under a kilobyte alone
        Some(n) if (1024..=MOST_TO_COMPRESS).contains(&n) => {}
        _ => return Response::from_parts(parts, body),
    }
    let Ok(bytes) = axum::body::to_bytes(body, MOST_TO_COMPRESS).await else {
        return refused(Failed::of(500, "the answer could not be read back"));
    };
    // compressing is work for a processor, and it used to be done on a
    // runtime thread: eight megabytes of it, while that thread answered
    // nothing else
    let zipped = tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&bytes).ok()?;
        encoder.finish().ok().map(|zipped| (zipped, bytes))
    })
    .await;
    let Ok(Some((zipped, bytes))) = zipped else {
        return refused(Failed::of(500, "the answer could not be compressed"));
    };
    if zipped.len() >= bytes.len() {
        return Response::from_parts(parts, Body::from(bytes));
    }
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    parts.headers.append(header::VARY, HeaderValue::from_static("accept-encoding"));
    Response::from_parts(parts, Body::from(zipped))
}

async fn dql_opt_in_stats(State(serving): State<Shared>, body: axum::Json<Value>) -> Response {
    let Some(opt_in) = body.get("opt_in").and_then(|v| v.as_bool()) else {
        let got = match body.get("opt_in") {
            None | Some(Value::Null) => "undefined",
            Some(Value::String(_)) => "string",
            Some(Value::Number(_)) => "number",
            Some(Value::Array(_)) => "array",
            Some(Value::Object(_)) => "object",
            Some(Value::Bool(_)) => "boolean",
        };
        return refused(Failed::of(
            400,
            format!("[request body.opt_in]: expected value of type [boolean] but got [{got}]"),
        ));
    };
    on_engine(serving, move |s| velosearch::console::usage::dql_opt_in(&saved_of(s), opt_in)).await
}

async fn ui_metric_report(State(serving): State<Shared>, body: axum::Json<Value>) -> Response {
    let Some(report) = body.get("report").cloned() else {
        return refused(Failed::of(
            400,
            "[request body.report]: expected value of type [object] but got [undefined]",
        ));
    };
    on_engine(serving, move |s| {
        match velosearch::console::usage::store_report(&saved_of(s), &report) {
            Ok(()) => Ok(serde_json::json!({"status": "ok"})),
            Err(e) if e.status == 400 => Err(e),
            // a report that could not be kept is not the page's problem
            Err(_) => Ok(serde_json::json!({"status": "fail"})),
        }
    })
    .await
}

/// `/api/stats`: the process's numbers in this route's spelling, what the
/// server is, and -- when asked at length -- the cluster's id and what the
/// server has been used for.
async fn stats(
    State(serving): State<Shared>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let pairs = query_pairs(query);
    let flag = |key: &str| -> Result<bool, Box<Response>> {
        match pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()) {
            None => Ok(false),
            Some("" | "true") => Ok(true),
            Some("false") => Ok(false),
            Some(other) => Err(Box::new(bad_query(
                key,
                &format!(
                    "types that failed validation:\n- [request query.{key}.0]: expected value to equal [] but got [{other}]\n- [request query.{key}.1]: expected value of type [boolean] but got [string]"
                ),
            ))),
        }
    };
    let (extended, legacy, exclude_usage) =
        match (flag("extended"), flag("legacy"), flag("exclude_usage")) {
            (Ok(e), Ok(l), Ok(x)) => (e, l, x),
            (Err(r), _, _) | (_, Err(r), _) | (_, _, Err(r)) => return *r,
        };
    for (key, _) in &pairs {
        if !matches!(key.as_str(), "extended" | "legacy" | "exclude_usage") {
            return bad_query(key, "definition for this key is missing");
        }
    }
    on_engine(serving, move |s| {
        let reachable = s.engine.reachable();
        let (host, port) = s.addr.rsplit_once(':').unwrap_or((s.addr.as_str(), ""));
        let mut metrics = s.metrics.report();
        if let Some(m) = metrics.as_object_mut() {
            m.insert(
                "opensearchDashboards".into(),
                serde_json::json!({
                    "uuid": s.console.uuid(),
                    "name": "velosearch-console",
                    "index": velosearch::console::engine::INDEX,
                    "host": host,
                    "locale": "en",
                    "transport_address": format!("{host}:{port}"),
                    "version": s.console.pinned.version,
                    "snapshot": false,
                    "status": if reachable.is_ok() { "green" } else { "red" },
                }),
            );
        }
        let mut out = velosearch::console::usage::api_field_names(metrics);
        if extended {
            let cluster_uuid = reachable
                .ok()
                .and_then(|info| info.get("cluster_uuid").cloned())
                .unwrap_or(Value::Null);
            let usage = match exclude_usage {
                true => serde_json::json!({}),
                false => velosearch::console::usage::usage(&saved_of(s)),
            };
            if legacy {
                // the old shape: the server's own usage spread at the top,
                // names as the collectors spell them
                let mut flat = serde_json::Map::new();
                if let Some(usage) = usage.as_object() {
                    for (key, value) in usage {
                        if key == "opensearchDashboards" {
                            if let Some(inner) = value.as_object() {
                                flat.extend(inner.clone());
                            }
                        } else {
                            flat.insert(key.clone(), value.clone());
                        }
                    }
                }
                out["usage"] = Value::Object(flat);
                out["clusterUuid"] = cluster_uuid;
            } else {
                out["usage"] = velosearch::console::usage::api_field_names(usage);
                out["cluster_uuid"] = cluster_uuid;
            }
        }
        Ok(out)
    })
    .await
}

async fn sample_data_list(State(serving): State<Shared>) -> Response {
    on_engine(serving, move |s| {
        Ok(Value::Array(velosearch::console::sample_data::list(
            &s.engine,
            &saved_of(s),
            &s.console.sample_data,
        )))
    })
    .await
}

fn sample_set<'a>(serving: &'a Serving, id: &str) -> Option<&'a Value> {
    serving.console.sample_data.iter().find(|s| s.get("id").and_then(|v| v.as_str()) == Some(id))
}

async fn sample_data_install(
    State(serving): State<Shared>,
    Path(id): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let now = query_pairs(query).into_iter().find(|(k, _)| k == "now").map(|(_, v)| v);
    on_engine(serving, move |s| {
        let Some(set) = sample_set(s, &id) else { return Err(Failed::of(404, "Not Found")) };
        velosearch::console::sample_data::install(
            &s.engine,
            &saved_of(s),
            &s.console.home,
            set,
            now.as_deref(),
        )
    })
    .await
}

async fn sample_data_uninstall(State(serving): State<Shared>, Path(id): Path<String>) -> Response {
    let done = tokio::task::spawn_blocking({
        let serving = serving.clone();
        move || {
            let Some(set) = sample_set(&serving, &id) else {
                return Err(Failed::of(404, "Not Found"));
            };
            velosearch::console::sample_data::uninstall(&serving.engine, &saved_of(&serving), set)
        }
    })
    .await;
    match done {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => refused(e),
        Err(e) => refused(Failed::of(500, format!("{e}"))),
    }
}

// ---- the Index Management plugin's server half ------------------------------

async fn ism_indices(
    State(serving): State<Shared>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let pairs = query_pairs(query);
    on_engine(serving, move |s| Ok(velosearch::console::ism::indices(&s.engine, &pairs))).await
}

async fn ism_data_streams(
    State(serving): State<Shared>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let search = query_pairs(query).into_iter().find(|(k, _)| k == "search").map(|(_, v)| v);
    on_engine(serving, move |s| {
        Ok(velosearch::console::ism::data_streams(&s.engine, search.as_deref()))
    })
    .await
}

/// One call by the old client's name, from the body or -- for a caller
/// that sent none -- the query string.
async fn ism_api_caller(
    State(serving): State<Shared>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    body: axum::body::Bytes,
) -> Response {
    let asked: Value = match serde_json::from_slice::<Value>(&body) {
        Ok(v) if v.is_object() => v,
        _ => {
            let pairs = query_pairs(query);
            let value = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
            serde_json::json!({
                "endpoint": value("endpoint"),
                "data": value("data").and_then(|d| serde_json::from_str::<Value>(&d).ok()).unwrap_or(Value::Null),
            })
        }
    };
    let endpoint = asked.get("endpoint").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let data = asked.get("data").cloned().unwrap_or(Value::Null);
    on_engine(serving, move |s| {
        if endpoint.is_empty() {
            return Ok(
                serde_json::json!({"ok": false, "error": "Expected non-empty string on endpoint"}),
            );
        }
        let filter = s.proxy_filter.clone();
        let allowed = move |path: &str| filter.iter().any(|re| re.is_match(path));
        Ok(velosearch::console::ism::api_caller(&s.engine, &endpoint, &data, &allowed))
    })
    .await
}

// ---- the rest of what the core pages ask their own plugins' servers -------

/// The engine's answer carried back as the pages' search routes carry
/// theirs: the body on success, `{message, attributes: {error}}` otherwise.
fn carried(answer: velosearch::console::engine::Answer) -> Result<Value, Failed> {
    let found: Value = serde_json::from_slice(&answer.body)
        .map_err(|e| Failed::of(502, format!("the engine's answer could not be read: {e}")))?;
    if answer.status >= 300 {
        let message = found
            .pointer("/error/reason")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| found.to_string());
        return Err(Failed::of(answer.status, message)
            .with_error(found.get("error").cloned().unwrap_or(Value::Null)));
    }
    Ok(found)
}

async fn resolve_index(
    State(serving): State<Shared>,
    Path(query): Path<String>,
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> Response {
    let expand =
        query_pairs(raw).into_iter().find(|(k, _)| k == "expand_wildcards").map(|(_, v)| v);
    if let Some(e) = &expand
        && !matches!(e.as_str(), "all" | "open" | "closed" | "hidden" | "none")
    {
        return bad_query(
            "expand_wildcards",
            &format!("expected value to equal [all] but got [{e}]"),
        );
    }
    on_engine(serving, move |s| {
        let mut path = format!("/_resolve/index/{}", path_segment(&query));
        if let Some(e) = expand {
            path.push_str(&format!("?expand_wildcards={e}"));
        }
        carried(s.engine.raw("GET", &path, b"", "application/json")?)
    })
    .await
}

async fn preview_scripted_field(
    State(serving): State<Shared>,
    body: axum::Json<Value>,
) -> Response {
    let body = body.0;
    let (Some(index), Some(name), Some(script)) = (
        body.get("index").and_then(|v| v.as_str()).map(String::from),
        body.get("name").and_then(|v| v.as_str()).map(String::from),
        body.get("script").and_then(|v| v.as_str()).map(String::from),
    ) else {
        return refused(Failed::of(
            400,
            "[request body.index]: expected value of type [string] but got [undefined]",
        ));
    };
    let query = body.get("query").cloned().unwrap_or_else(|| serde_json::json!({"match_all": {}}));
    let fields =
        body.get("additionalFields").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    on_engine(serving, move |s| {
        let mut search = serde_json::json!({
            "size": 10,
            "timeout": "30s",
            "query": query,
            "script_fields": {name: {"script": {"lang": "painless", "source": script}}},
        });
        if !fields.is_empty() {
            search["_source"] = Value::Array(fields);
        }
        let path = format!("/{}/_search", path_segment(&index));
        carried(s.engine.raw("POST", &path, search.to_string().as_bytes(), "application/json")?)
    })
    .await
}

/// Whether an index has anything in it, for the home page's "add data"
/// step: one hit asked for, and how many came back.
async fn hits_status(State(serving): State<Shared>, body: axum::Json<Value>) -> Response {
    let (Some(index), Some(query)) = (
        body.get("index").and_then(|v| v.as_str()).map(String::from),
        body.get("query").filter(|q| q.is_object()).cloned(),
    ) else {
        return refused(Failed::of(
            400,
            "[request body.index]: expected value of type [string] but got [undefined]",
        ));
    };
    on_engine(serving, move |s| {
        let path = format!("/{}/_search", path_segment(&index));
        let search = serde_json::json!({"size": 1, "query": query});
        let found = carried(s.engine.raw(
            "POST",
            &path,
            search.to_string().as_bytes(),
            "application/json",
        )?)
        .map_err(|e| Failed::of(400, e.message))?;
        let count =
            found.pointer("/hits/hits").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
        Ok(serde_json::json!({"count": count}))
    })
    .await
}

async fn tutorials(State(serving): State<Shared>) -> Response {
    let pinned = &serving.console.pinned.tutorials;
    axum::Json(if pinned.is_null() { serde_json::json!([]) } else { pinned.clone() })
        .into_response()
}

async fn dev_tools_api(State(serving): State<Shared>) -> Response {
    axum::Json(serving.console.pinned.dev_tools_api.clone()).into_response()
}
