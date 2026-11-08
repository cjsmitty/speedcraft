//! HTTP API shared by the UI host and the headless agent.
//!
//! All `/api/*` routes require `Authorization: Bearer <token>` when a token is
//! configured. The UI host proxies `/api/agents/{id}/*` to remote agents with
//! their stored tokens, so the browser only ever talks to localhost.

use super::agents::AgentRegistry;
use super::supervisor::Supervisor;
use crate::engine::Control;
use crate::runtime::Mode;
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

pub const INDEX_HTML: &str = include_str!("ui/index.html");
pub const APP_JS: &str = include_str!("ui/app.js");
pub const APP_CSS: &str = include_str!("ui/app.css");

#[derive(Clone)]
pub struct AppState {
    pub sup: Arc<Supervisor>,
    /// None on a pure agent (no remote-agent management).
    pub agents: Option<Arc<AgentRegistry>>,
    pub token: Option<String>,
    pub http: reqwest::Client,
    /// Present on the local UI host: lets the panel's Quit button stop the process.
    pub quit: Option<tokio::sync::mpsc::Sender<()>>,
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/status", get(status))
        .route("/events", get(events))
        .route("/engine/start", post(engine_start))
        .route("/engine/stop", post(engine_stop))
        .route("/engine/halt", post(engine_halt))
        .route("/engine/resume", post(engine_resume))
        .route("/stopfile", post(stopfile))
        .route("/config", get(config_get).put(config_put))
        .route("/record", post(record_toggle))
        .route("/quit", post(quit))
        .route("/coins", get(coins_get).put(coins_put))
        .route("/settings", get(settings_get).put(settings_put))
        .route("/exchanges/:id/test", post(exchange_test))
        .route("/scan", post(scan_run))
        .route("/agents", get(agents_list).post(agents_add))
        .route("/agents/:id", axum::routing::delete(agents_remove))
        .route("/agents/:id/*rest", axum::routing::any(agents_proxy))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth));
    Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route(
            "/app.js",
            get(|| async {
                (
                    [(
                        header::CONTENT_TYPE,
                        "application/javascript; charset=utf-8",
                    )],
                    APP_JS,
                )
            }),
        )
        .route(
            "/app.css",
            get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS) }),
        )
        .route("/healthz", get(|| async { "ok" }))
        .nest("/api", api)
        .with_state(state)
}

async fn auth(
    State(st): State<AppState>,
    headers: HeaderMap,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    if let Some(expected) = &st.token {
        let got = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if !constant_time_eq(got.as_bytes(), expected.as_bytes()) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "missing or invalid bearer token"})),
            )
                .into_response();
        }
    }
    next.run(req).await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(StatusCode::BAD_REQUEST, format!("{e:#}"))
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

async fn status(State(st): State<AppState>) -> impl IntoResponse {
    Json(st.sup.status())
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default)]
    since: u64,
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_limit() -> usize {
    500
}

async fn events(State(st): State<AppState>, Query(q): Query<EventsQuery>) -> impl IntoResponse {
    let (events, latest) = st.sup.events(q.since, q.limit.min(5000));
    Json(json!({"events": events, "latest_seq": latest}))
}

#[derive(Deserialize)]
struct StartReq {
    mode: String,
    #[serde(default)]
    confirm: Option<String>,
}

async fn engine_start(State(st): State<AppState>, Json(req): Json<StartReq>) -> ApiResult {
    let mode = Mode::parse(&req.mode)
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "mode must be paper or live".into()))?;
    st.sup.start(mode, req.confirm.as_deref())?;
    Ok(Json(json!({"ok": true, "state": st.sup.state()})))
}

async fn engine_stop(State(st): State<AppState>) -> ApiResult {
    st.sup.stop()?;
    Ok(Json(json!({"ok": true, "state": st.sup.state()})))
}

#[derive(Deserialize)]
struct HaltReq {
    #[serde(default)]
    reason: Option<String>,
}

async fn engine_halt(State(st): State<AppState>, Json(req): Json<HaltReq>) -> ApiResult {
    st.sup
        .control(Control::Halt {
            reason: req.reason.unwrap_or_else(|| "manual halt".into()),
        })
        .await?;
    Ok(Json(json!({"ok": true})))
}

async fn engine_resume(State(st): State<AppState>) -> ApiResult {
    st.sup.control(Control::Resume).await?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct StopFileReq {
    present: bool,
}

async fn stopfile(State(st): State<AppState>, Json(req): Json<StopFileReq>) -> ApiResult {
    st.sup.set_stop_file(req.present)?;
    Ok(Json(
        json!({"ok": true, "present": st.sup.stop_file_path().exists()}),
    ))
}

async fn config_get(State(st): State<AppState>) -> ApiResult {
    let text = st.sup.read_config_text()?;
    Ok(Json(
        json!({"path": st.sup.cfg_path.display().to_string(), "toml": text, "summary": st.sup.config_summary()}),
    ))
}

#[derive(Deserialize)]
struct ConfigPut {
    toml: String,
}

async fn config_put(State(st): State<AppState>, Json(req): Json<ConfigPut>) -> ApiResult {
    st.sup.write_config_text(&req.toml)?;
    let running = st.sup.state() != super::supervisor::EngineState::Stopped;
    Ok(Json(
        json!({"ok": true, "restart_required": running, "summary": st.sup.config_summary()}),
    ))
}

fn cfg_dir(st: &AppState) -> std::path::PathBuf {
    st.sup
        .cfg_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

async fn settings_get(State(st): State<AppState>) -> ApiResult {
    let cfg = st.sup.load_config()?;
    let secrets = super::settings::secrets_view(&cfg_dir(&st))?;
    Ok(Json(
        json!({"config": serde_json::to_value(&cfg).unwrap_or(Value::Null), "secrets": secrets, "path": st.sup.cfg_path.display().to_string()}),
    ))
}

#[derive(Deserialize)]
struct SettingsPut {
    #[serde(default)]
    config: Option<Value>,
    #[serde(default)]
    secrets: Option<super::settings::SecretsPatch>,
}

async fn settings_put(State(st): State<AppState>, Json(req): Json<SettingsPut>) -> ApiResult {
    if let Some(patch) = &req.config {
        let text = st.sup.read_config_text()?;
        let new_text = super::settings::apply_patch(&text, patch)?;
        st.sup.write_config_text(&new_text)?;
    }
    if let Some(sp) = &req.secrets {
        super::settings::apply_secrets(&cfg_dir(&st), sp)?;
    }
    let cfg = st.sup.load_config()?;
    let secrets = super::settings::secrets_view(&cfg_dir(&st))?;
    let running = st.sup.state() != super::supervisor::EngineState::Stopped;
    Ok(Json(
        json!({"ok": true, "restart_required": running && req.config.is_some(), "config": serde_json::to_value(&cfg).unwrap_or(Value::Null), "secrets": secrets, "summary": st.sup.config_summary()}),
    ))
}

async fn exchange_test(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let ex: crate::types::ExchangeId = id
        .parse()
        .map_err(|e: String| ApiError(StatusCode::BAD_REQUEST, e))?;
    let cfg = st.sup.load_config()?;
    let r = super::settings::test_exchange(&cfg, ex).await;
    Ok(Json(serde_json::to_value(r).unwrap_or(Value::Null)))
}

async fn coins_get(State(st): State<AppState>) -> ApiResult {
    Ok(Json(
        serde_json::to_value(st.sup.coins()?).unwrap_or(Value::Null),
    ))
}

#[derive(Deserialize)]
struct CoinsPut {
    bases: Vec<String>,
}

async fn coins_put(State(st): State<AppState>, Json(req): Json<CoinsPut>) -> ApiResult {
    let v = st.sup.set_coins(&req.bases)?;
    let running = st.sup.state() != super::supervisor::EngineState::Stopped;
    Ok(Json(
        json!({"ok": true, "restart_required": running, "coins": serde_json::to_value(v).unwrap_or(Value::Null)}),
    ))
}

async fn quit(State(st): State<AppState>) -> ApiResult {
    let Some(tx) = &st.quit else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "this instance cannot be quit remotely; stop it on its own machine".into(),
        ));
    };
    let _ = tx.send(()).await;
    Ok(Json(
        json!({"ok": true, "message": "Speedcraft is shutting down"}),
    ))
}

#[derive(Deserialize)]
struct RecordReq {
    on: bool,
}

async fn record_toggle(State(st): State<AppState>, Json(req): Json<RecordReq>) -> ApiResult {
    let status = if req.on {
        st.sup.start_recording()?
    } else {
        st.sup.stop_recording()?
    };
    Ok(Json(serde_json::to_value(status).unwrap_or(Value::Null)))
}

#[derive(Deserialize, Default)]
struct ScanReq {
    #[serde(default)]
    file: Option<String>,
}

async fn scan_run(State(st): State<AppState>, body: Option<Json<ScanReq>>) -> ApiResult {
    let req = body.map(|b| b.0).unwrap_or_default();
    let sup = st.sup.clone();
    let file = req.file.clone();
    // Scanning a long recording can take a second or two; keep it off the runtime threads.
    let result = tokio::task::spawn_blocking(move || sup.scan(file.as_deref()))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(serde_json::to_value(result).unwrap_or(Value::Null)))
}

fn registry(st: &AppState) -> Result<&Arc<AgentRegistry>, ApiError> {
    st.agents.as_ref().ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            "this is a headless agent; remote agents are managed from the UI host".into(),
        )
    })
}

async fn agents_list(State(st): State<AppState>) -> ApiResult {
    let r = registry(&st)?;
    Ok(Json(json!({"agents": r.list()})))
}

#[derive(Deserialize, Serialize)]
struct AddAgentReq {
    name: String,
    url: String,
    #[serde(default)]
    token: String,
}

async fn agents_add(State(st): State<AppState>, Json(req): Json<AddAgentReq>) -> ApiResult {
    let r = registry(&st)?;
    let v = r.add(&req.name, &req.url, &req.token)?;
    Ok(Json(json!({"ok": true, "agent": v})))
}

async fn agents_remove(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let r = registry(&st)?;
    r.remove(&id)?;
    Ok(Json(json!({"ok": true})))
}

/// Forward `/api/agents/{id}/{rest}` to `{agent.url}/api/{rest}` with the
/// agent's token. The remote's status code and JSON body are returned as-is.
async fn agents_proxy(
    State(st): State<AppState>,
    Path((id, rest)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> Response {
    let r = match registry(&st) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    let Some(agent) = r.get(&id) else {
        return ApiError(StatusCode::NOT_FOUND, format!("no agent '{id}'")).into_response();
    };
    // Never let a proxied request manage the remote's own agent list.
    if rest.starts_with("agents") {
        return ApiError(
            StatusCode::FORBIDDEN,
            "nested agent management is not allowed".into(),
        )
        .into_response();
    }
    let mut url = format!("{}/api/{}", agent.url, rest);
    if let Some(q) = uri.query() {
        url.push('?');
        url.push_str(q);
    }
    let mut req = st
        .http
        .request(method, &url)
        .timeout(std::time::Duration::from_secs(8));
    if !agent.token.is_empty() {
        req = req.bearer_auth(&agent.token);
    }
    if !body.is_empty() {
        req = req
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.to_vec());
    }
    match req.send().await {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let text = resp.text().await.unwrap_or_default();
            (status, [(header::CONTENT_TYPE, "application/json")], text).into_response()
        }
        Err(e) => ApiError(
            StatusCode::BAD_GATEWAY,
            format!("agent '{}' unreachable at {}: {e}", agent.name, agent.url),
        )
        .into_response(),
    }
}

/// Generate a random URL-safe token for agents.
pub fn generate_token() -> String {
    use base64::Engine;
    let bytes: [u8; 32] = rand::random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EXAMPLE_TOML;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn state(token: Option<&str>, with_registry: bool) -> (AppState, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "speedcraft-api-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        std::fs::write(&cfg, EXAMPLE_TOML).unwrap();
        let sup = Supervisor::new("test-agent", cfg);
        let agents = if with_registry {
            Some(Arc::new(
                AgentRegistry::open(&dir.join("agents.json")).unwrap(),
            ))
        } else {
            None
        };
        (
            AppState {
                sup,
                agents,
                token: token.map(String::from),
                http: reqwest::Client::new(),
                quit: None,
            },
            dir,
        )
    }

    async fn call(
        app: Router,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(path);
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        let req = match body {
            Some(v) => b
                .header("content-type", "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: Value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into()))
        };
        (status, v)
    }

    #[tokio::test]
    async fn token_is_enforced_on_api_but_not_static() {
        let (st, _d) = state(Some("s3cret"), true);
        let app = router(st);
        let (s, _) = call(app.clone(), "GET", "/api/status", None, None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let (s, _) = call(app.clone(), "GET", "/api/status", Some("wrong"), None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let (s, v) = call(app.clone(), "GET", "/api/status", Some("s3cret"), None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["agent"], "test-agent");
        assert_eq!(v["engine"]["state"], "stopped");
        let (s, _) = call(app.clone(), "GET", "/", None, None).await;
        assert_eq!(s, StatusCode::OK);
        let (s, _) = call(app, "GET", "/healthz", None, None).await;
        assert_eq!(s, StatusCode::OK);
    }

    #[tokio::test]
    async fn live_start_is_refused_and_stop_when_stopped_is_an_error() {
        let (st, _d) = state(None, true);
        let app = router(st);
        let (s, v) = call(
            app.clone(),
            "POST",
            "/api/engine/start",
            None,
            Some(json!({"mode": "live"})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"].as_str().unwrap().contains("confirmation phrase"));
        let (s, v) = call(
            app.clone(),
            "POST",
            "/api/engine/start",
            None,
            Some(json!({"mode": "live", "confirm": "I ACCEPT LOSSES"})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"]
            .as_str()
            .unwrap()
            .contains("live = true in config"));
        let (s, _) = call(
            app.clone(),
            "POST",
            "/api/engine/start",
            None,
            Some(json!({"mode": "bogus"})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _) = call(app.clone(), "POST", "/api/engine/stop", None, None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _) = call(app, "POST", "/api/engine/resume", None, None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn stopfile_config_and_events_endpoints() {
        let (st, _d) = state(None, true);
        let app = router(st);
        let (s, v) = call(
            app.clone(),
            "POST",
            "/api/stopfile",
            None,
            Some(json!({"present": true})),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["present"], true);
        let (_, v) = call(app.clone(), "GET", "/api/status", None, None).await;
        assert_eq!(v["stop_file_present"], true);
        let (_, v) = call(
            app.clone(),
            "POST",
            "/api/stopfile",
            None,
            Some(json!({"present": false})),
        )
        .await;
        assert_eq!(v["present"], false);
        let (s, v) = call(app.clone(), "GET", "/api/config", None, None).await;
        assert_eq!(s, StatusCode::OK);
        let toml = v["toml"].as_str().unwrap().to_string();
        let (s, v) = call(
            app.clone(),
            "PUT",
            "/api/config",
            None,
            Some(json!({"toml": toml.replace("min_edge_bps = 5", "min_edge_bps = 9")})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["restart_required"], false);
        assert_eq!(v["summary"]["min_edge_bps"], "9");
        let (s, _) = call(
            app.clone(),
            "PUT",
            "/api/config",
            None,
            Some(json!({"toml": "nope ="})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, v) = call(app, "GET", "/api/events?since=0", None, None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["latest_seq"], 0);
        assert_eq!(v["events"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn record_and_scan_endpoints_fail_cleanly_when_idle() {
        let (st, _d) = state(None, true);
        let app = router(st);
        let (s, v) = call(
            app.clone(),
            "POST",
            "/api/record",
            None,
            Some(json!({"on": true})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"].as_str().unwrap().contains("start the engine"));
        let (s, v) = call(app.clone(), "POST", "/api/scan", None, Some(json!({}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"].as_str().unwrap().contains("no recordings"));
        let (_, v) = call(app, "GET", "/api/status", None, None).await;
        assert_eq!(v["recorder"]["active"], false);
    }

    #[tokio::test]
    async fn agent_registry_endpoints_and_proxy_errors() {
        let (st, _d) = state(None, true);
        let app = router(st);
        let (s, v) = call(
            app.clone(),
            "POST",
            "/api/agents",
            None,
            Some(json!({"name": "VM", "url": "http://127.0.0.1:1", "token": "t"})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["agent"]["id"], "vm");
        assert!(
            v["agent"].get("token").is_none(),
            "token must not be echoed"
        );
        let (_, v) = call(app.clone(), "GET", "/api/agents", None, None).await;
        assert_eq!(v["agents"].as_array().unwrap().len(), 1);
        // Unreachable remote → 502 with a clear message.
        let (s, v) = call(app.clone(), "GET", "/api/agents/vm/status", None, None).await;
        assert_eq!(s, StatusCode::BAD_GATEWAY);
        assert!(v["error"].as_str().unwrap().contains("unreachable"));
        // Nested agent management is blocked.
        let (s, _) = call(app.clone(), "GET", "/api/agents/vm/agents", None, None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _) = call(app.clone(), "GET", "/api/agents/nope/status", None, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _) = call(app.clone(), "DELETE", "/api/agents/vm", None, None).await;
        assert_eq!(s, StatusCode::OK);
        let (_, v) = call(app, "GET", "/api/agents", None, None).await;
        assert_eq!(v["agents"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn quit_is_refused_without_a_quit_channel_and_signals_with_one() {
        let (st, _d) = state(None, true);
        let app = router(st.clone());
        let (s, _) = call(app, "POST", "/api/quit", None, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let st2 = AppState {
            quit: Some(tx),
            ..st
        };
        let app = router(st2);
        let (s, v) = call(app, "POST", "/api/quit", None, None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["ok"], true);
        assert!(rx.try_recv().is_ok(), "quit signal delivered");
    }

    #[tokio::test]
    async fn coins_endpoints_rewrite_the_config() {
        let (st, _d) = state(None, true);
        let app = router(st);
        let (s, v) = call(app.clone(), "GET", "/api/coins", None, None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(v["catalog"].as_array().unwrap().len() >= 24);
        assert!(v["selected"].as_array().unwrap().iter().any(|b| b == "BTC"));
        let (s, v) = call(
            app.clone(),
            "PUT",
            "/api/coins",
            None,
            Some(json!({"bases": ["SOL", "XRP"]})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["coins"]["selected"], json!(["SOL", "XRP"]));
        assert_eq!(v["restart_required"], false);
        let (_, v) = call(app.clone(), "GET", "/api/status", None, None).await;
        assert_eq!(v["config"]["pairs"], json!(["SOL/USD", "XRP/USD"]));
        let (s, _) = call(app, "PUT", "/api/coins", None, Some(json!({"bases": []}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn settings_endpoints_patch_config_and_secrets() {
        let (st, _d) = state(None, true);
        let app = router(st);
        let (s, v) = call(app.clone(), "GET", "/api/settings", None, None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["config"]["live"], false);
        assert_eq!(v["secrets"]["accept_losses"], false);
        let (s, v) = call(app.clone(), "PUT", "/api/settings", None, Some(json!({"config": {"strategy": {"min_edge_bps": "9"}}, "secrets": {"kraken_key": "ABCDEFGHIJKL", "kraken_secret": "c2VjcmV0", "accept_losses": true}}))).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["config"]["strategy"]["min_edge_bps"], "9");
        assert_eq!(v["secrets"]["kraken_secret_set"], true);
        assert_eq!(v["secrets"]["accept_losses"], true);
        assert_eq!(
            v["summary"]["live_env_set"], true,
            "acknowledgement counts as the second live gate"
        );
        let (s, v) = call(
            app.clone(),
            "PUT",
            "/api/settings",
            None,
            Some(json!({"config": {"strategy": {"depth_fraction": "7"}}})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"].as_str().unwrap().contains("depth_fraction"));
        let (s, v) = call(
            app.clone(),
            "POST",
            "/api/exchanges/coinbase/test",
            None,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["ok"], false);
        assert!(v["message"]
            .as_str()
            .unwrap()
            .contains("no API credentials"));
        let (s, _) = call(app, "POST", "/api/exchanges/mtgox/test", None, None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn headless_agent_has_no_registry() {
        let (st, _d) = state(None, false);
        let app = router(st);
        let (s, _) = call(app, "GET", "/api/agents", None, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[test]
    fn tokens_are_long_and_unique() {
        let a = generate_token();
        let b = generate_token();
        assert!(a.len() >= 40);
        assert_ne!(a, b);
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
