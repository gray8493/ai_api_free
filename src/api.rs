use crate::config::{Config, KINDS, STRATEGIES};
use crate::proxy::{self, Attempt};
use crate::state::{stat_key, AppState};
use axum::body::to_bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;

pub async fn ui() -> Response {
    (
        [(header::CACHE_CONTROL, "no-cache, no-store, must-revalidate")],
        Html(include_str!("ui.html")),
    )
        .into_response()
}

fn extract_key(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return Some(v.trim().to_string());
    }
    if let Some(v) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        let token = match v.split_once(' ') {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("bearer") => rest.trim(),
            _ => v,
        };
        return Some(token.to_string());
    }
    None
}

pub async fn authorized(st: &AppState, headers: &HeaderMap) -> bool {
    let cfg = st.config.read().await;
    if !cfg.has_auth() {
        return true;
    }
    matches!(extract_key(headers), Some(k) if k == cfg.gateway_api_key.trim())
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": { "message": "sai hoặc thiếu API key", "type": "unauthorized" } })),
    )
        .into_response()
}

fn bad_request(msg: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": { "message": msg.into(), "type": "bad_request" } })),
    )
        .into_response()
}

fn server_error(msg: impl Into<String>) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": { "message": msg.into(), "type": "server_error" } })),
    )
        .into_response()
}

pub async fn health() -> Response {
    Json(json!({ "ok": true, "service": "ai-gateway" })).into_response()
}

pub async fn chat(State(st): State<Arc<AppState>>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if !authorized(&st, &headers).await {
        println!("--> [TỪ CHỐI] Request bị từ chối do sai hoặc thiếu Gateway API Key");
        return unauthorized();
    }
    let cfg = st.config.read().await.clone();
    let requested = body.get("model").and_then(|m| m.as_str()).unwrap_or("auto").to_string();
    let wants_stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    println!("--> [NHẬN REQUEST] Model: '{}' | Stream: {}", requested, wants_stream);
    let attempts = proxy::plan(&st, &cfg, &requested);
    if attempts.is_empty() {
        println!("<-- [CẢNH BÁO] Không có provider/account nào khả dụng cho model '{}'", requested);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": {
                    "message": "không có provider/account khả dụng (bật provider, thêm key, hoặc chờ hết cooldown)",
                    "type": "no_account",
                }
            })),
        )
            .into_response();
    }
    proxy::execute(st.clone(), cfg, body, wants_stream, attempts).await
}

fn model_entry(id: String, owner: &str) -> Value {
    json!({ "id": id, "object": "model", "created": 0, "owned_by": owner })
}

pub async fn models(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !authorized(&st, &headers).await {
        return unauthorized();
    }
    let cfg = st.config.read().await.clone();
    let mut data = vec![model_entry("auto".into(), "ai-gateway")];
    let mut seen = std::collections::HashSet::new();
    seen.insert("auto".to_string());
    for p in cfg.providers.iter().filter(|p| p.enabled) {
        if seen.insert(p.id.clone()) {
            data.push(model_entry(p.id.clone(), &p.id));
        }
        if seen.insert(p.model.clone()) {
            data.push(model_entry(p.model.clone(), &p.id));
        }
        let prefix = format!("{}/", p.id);
        if !p.model.starts_with(&prefix) {
            let prefixed = format!("{}{}", prefix, p.model);
            if seen.insert(prefixed.clone()) {
                data.push(model_entry(prefixed, &p.id));
            }
        }
    }
    Json(json!({ "object": "list", "data": data })).into_response()
}

pub async fn get_config(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !authorized(&st, &headers).await {
        return unauthorized();
    }
    let cfg = st.config.read().await.clone();
    Json(json!({
        "config": cfg,
        "strategies": STRATEGIES,
        "kinds": KINDS,
        "config_path": st.config_path.display().to_string(),
        "requires_key": cfg.has_auth(),
    }))
    .into_response()
}

pub async fn put_config(State(st): State<Arc<AppState>>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if !authorized(&st, &headers).await {
        return unauthorized();
    }
    let raw = body.get("config").cloned().unwrap_or(body);
    let mut cfg: Config = match serde_json::from_value(raw) {
        Ok(c) => c,
        Err(e) => return bad_request(format!("config không hợp lệ: {}", e)),
    };
    if let Err(e) = cfg.validate() {
        return bad_request(e);
    }
    let cfg = cfg.sanitized();
    if let Err(e) = cfg.save(&st.config_path) {
        return server_error(format!("không ghi được {}: {}", st.config_path.display(), e));
    }
    st.prune_stats(&cfg);
    *st.config.write().await = cfg.clone();
    tracing::info!("config saved");
    Json(json!({ "ok": true, "config": cfg })).into_response()
}

pub async fn reload(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !authorized(&st, &headers).await {
        return unauthorized();
    }
    match Config::load(&st.config_path) {
        Ok(cfg) => {
            *st.config.write().await = cfg.clone();
            Json(json!({ "ok": true, "config": cfg })).into_response()
        }
        Err(e) => bad_request(format!("không đọc được config: {}", e)),
    }
}

pub async fn status(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !authorized(&st, &headers).await {
        return unauthorized();
    }
    let cfg = st.config.read().await.clone();
    Json(st.status_json(&cfg)).into_response()
}

fn reply_preview(v: &Value) -> String {
    if let Some(c) = v.pointer("/choices/0/message/content").and_then(|c| c.as_str()) {
        return c.chars().take(160).collect();
    }
    v.to_string().chars().take(160).collect()
}

pub async fn test_account(State(st): State<Arc<AppState>>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if !authorized(&st, &headers).await {
        return unauthorized();
    }
    let cfg = st.config.read().await.clone();
    let provider_id = body.get("provider").and_then(|v| v.as_str()).unwrap_or("");
    let account_name = body.get("account").and_then(|v| v.as_str());
    let Some(p) = cfg.provider(provider_id).cloned() else {
        return bad_request("provider không tồn tại");
    };
    let accounts: Vec<_> = p
        .accounts
        .iter()
        .filter(|a| account_name.is_none() || account_name == Some(a.name.as_str()))
        .filter(|a| a.enabled && !a.api_key.trim().is_empty())
        .cloned()
        .collect();
    if accounts.is_empty() {
        return bad_request("không có account khả dụng để test");
    }

    let mut results = Vec::new();
    for a in accounts {
        let key = stat_key(&p.id, &a.name);
        let attempt = Attempt {
            provider: p.clone(),
            account: a.clone(),
            model: p.model.clone(),
        };
        let started = Instant::now();
        match proxy::attempt_once(&st, &attempt, &proxy::test_body(), false).await {
            Ok(s) => {
                let ms = started.elapsed().as_millis();
                let bytes = to_bytes(s.response.into_body(), 1024 * 512).await.unwrap_or_default();
                let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                st.record_success(&key, s.status);
                st.record_latency(&p.id, &a.name, ms as u64);
                st.unban_provider(&p.id);
                results.push(json!({ "account": a.name, "ok": true, "ms": ms, "reply": reply_preview(&v) }));
            }
            Err(f) => {
                let ms = started.elapsed().as_millis();
                st.record_error(&key, f.status, f.message.clone());
                results.push(json!({
                    "account": a.name,
                    "ok": false,
                    "ms": ms,
                    "status": f.status,
                    "error": f.message,
                }));
            }
        }
    }
    Json(json!({ "provider": p.id, "model": p.model, "results": results })).into_response()
}

pub async fn benchmark(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !authorized(&st, &headers).await {
        return unauthorized();
    }
    let summary = proxy::benchmark_providers(st).await;
    Json(summary).into_response()
}

pub async fn unban(State(st): State<Arc<AppState>>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if !authorized(&st, &headers).await {
        return unauthorized();
    }
    let provider_id = body.get("provider").and_then(|v| v.as_str()).unwrap_or("");
    if provider_id == "all" || provider_id.is_empty() {
        st.unban_all_providers();
        Json(json!({ "ok": true, "message": "Đã gỡ ban tất cả provider trong phiên" })).into_response()
    } else {
        let unbanned = st.unban_provider(provider_id);
        Json(json!({ "ok": true, "provider": provider_id, "unbanned": unbanned })).into_response()
    }
}
