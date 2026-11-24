use crate::config::{Account, Config, Provider};
use crate::gemini;
use crate::state::{stat_key, AppState};
use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rand::seq::SliceRandom;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub struct Attempt {
    pub provider: Provider,
    pub account: Account,
    pub model: String,
}

pub struct Failure {
    pub status: Option<u16>,
    pub message: String,
    pub cooldown: bool,
}

pub struct Success {
    pub status: u16,
    pub response: Response,
}

fn split_forced(cfg: &Config, requested: &str) -> Option<(usize, String)> {
    // 1. Khớp trực tiếp với provider id: vd "nvidia", "groq", "gemini"
    for (i, p) in cfg.providers.iter().enumerate() {
        if p.enabled && p.id.eq_ignore_ascii_case(requested) {
            return Some((i, p.model.clone()));
        }
    }

    // 2. Khớp chính xác với model mặc định của bất kỳ provider nào:
    // vd: "nvidia/nemotron-3.5-bolt-e4b" -> giữ nguyên model đầy đủ, để plan() xử lý trực tiếp
    // (tránh việc strip nhầm prefix 'nvidia/' làm sai tên model của NVIDIA NIM)
    let is_exact_model = cfg
        .providers
        .iter()
        .any(|p| p.enabled && !requested.is_empty() && requested != "auto" && requested != "*" && p.model == requested);
    if is_exact_model {
        return None;
    }

    // 3. Cú pháp chỉ định provider: <provider>/<model> hoặc <provider>:<model>
    for (i, p) in cfg.providers.iter().enumerate() {
        if !p.enabled {
            continue;
        }
        for sep in [':', '/'] {
            let prefix = format!("{}{}", p.id, sep);
            if let Some(rest) = requested.strip_prefix(&prefix) {
                let rest = rest.trim();
                let model = if rest.is_empty() {
                    p.model.clone()
                } else if p.id == "nvidia" && !rest.contains('/') {
                    // Với NVIDIA NIM, mọi model đều có prefix namespace tổ chức (vd: nvidia/nemotron-3.5-bolt-e4b)
                    format!("nvidia/{}", rest)
                } else {
                    rest.to_string()
                };
                return Some((i, model));
            }
        }
    }
    None
}

fn model_for(provider: &Provider, requested: &str, forced: Option<&str>) -> String {
    if let Some(m) = forced {
        return if m.trim().is_empty() { provider.model.clone() } else { m.to_string() };
    }
    if requested.is_empty() || requested == "auto" || requested == "*" {
        provider.model.clone()
    } else {
        requested.to_string()
    }
}

pub fn plan(state: &AppState, cfg: &Config, requested: &str) -> Vec<Attempt> {
    let requested = requested.trim();
    let forced = split_forced(cfg, requested);

    let mut indices: Vec<usize> = match &forced {
        Some((i, _)) => vec![*i],
        None => {
            let exact: Vec<usize> = cfg
                .providers
                .iter()
                .enumerate()
                .filter(|(_, p)| p.enabled && !requested.is_empty() && requested != "auto" && requested != "*" && p.model == requested)
                .map(|(i, _)| i)
                .collect();
            if exact.is_empty() {
                cfg.providers
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| p.enabled)
                    .map(|(i, _)| i)
                    .collect()
            } else {
                exact
            }
        }
    };

    // Bỏ qua các provider đã bị tạm ngưng trong phiên làm việc nếu không phải request ép buộc (forced)
    if forced.is_none() && cfg.session_ban_on_failure {
        let unbanned: Vec<usize> = indices
            .iter()
            .copied()
            .filter(|&i| state.is_provider_banned(&cfg.providers[i].id).is_none())
            .collect();
        if !unbanned.is_empty() {
            indices = unbanned;
        } else {
            tracing::warn!("Tất cả provider trong danh sách đều đang bị ban trong phiên làm việc, tạm thời thử lại...");
        }
    }

    match cfg.strategy.as_str() {
        "fastest" if forced.is_none() => {
            indices.sort_by_key(|&i| {
                state
                    .provider_latency_ms(&cfg.providers[i].id)
                    .unwrap_or(u64::MAX)
            });
        }
        "random" => indices.shuffle(&mut rand::thread_rng()),
        "round_robin" if forced.is_none() => {
            if indices.len() > 1 {
                let off = state.next_global(indices.len());
                indices.rotate_left(off);
            }
        }
        _ => {}
    }

    let mut attempts = Vec::new();
    for i in indices {
        let p = &cfg.providers[i];
        if !p.enabled {
            continue;
        }
        let forced_model = forced.as_ref().map(|(_, m)| m.as_str());
        let model = model_for(p, requested, forced_model);

        let mut acc_idx: Vec<usize> = p
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.enabled && !a.api_key.trim().is_empty())
            .map(|(j, _)| j)
            .collect();
        if acc_idx.is_empty() {
            continue;
        }
        if cfg.strategy != "sequential" {
            let start = state.next_cursor(&format!("acc:{}", p.id), acc_idx.len());
            acc_idx.rotate_left(start);
        }

        for j in acc_idx {
            let a = &p.accounts[j];
            let key = stat_key(&p.id, &a.name);
            if state.cooldown_remaining(&key) > 0 {
                continue;
            }
            attempts.push(Attempt {
                provider: p.clone(),
                account: a.clone(),
                model: model.clone(),
            });
            if attempts.len() >= cfg.max_attempts {
                return attempts;
            }
        }
    }
    attempts
}

fn upstream_error_message(text: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        if let Some(m) = v.pointer("/error/message").and_then(|x| x.as_str()) {
            return m.to_string();
        }
        if let Some(m) = v.pointer("/error").and_then(|x| x.as_str()) {
            return m.to_string();
        }
        if let Some(m) = v.get("message").and_then(|x| x.as_str()) {
            return m.to_string();
        }
    }
    text.chars().take(300).collect()
}

fn is_fatal(status: u16) -> bool {
    status == 400 || status == 404 || status == 422
}

async fn send(st: &Arc<AppState>, a: &Attempt, body: &Value, wants_stream: bool) -> Result<reqwest::Response, Failure> {
    let base = a.provider.base_url.trim_end_matches('/');
    let key = a.account.api_key.trim().to_string();
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));

    let (url, payload) = if a.provider.kind == "gemini" {
        let action = if wants_stream { "streamGenerateContent" } else { "generateContent" };
        let model = a.model.trim_start_matches("models/");
        let mut url = format!("{}/models/{}:{}", base, model, action);
        if wants_stream {
            url.push_str("?alt=sse");
        }
        match HeaderValue::from_str(&key) {
            Ok(v) => {
                headers.insert("x-goog-api-key", v);
            }
            Err(_) => {
                return Err(Failure {
                    status: None,
                    message: "API key chứa ký tự không hợp lệ".into(),
                    cooldown: false,
                })
            }
        }
        (url, gemini::build_payload(body))
    } else {
        let url = format!("{}/chat/completions", base).replace("{account_id}", a.account.account_id.trim());
        match HeaderValue::from_str(&format!("Bearer {}", key)) {
            Ok(v) => {
                headers.insert(header::AUTHORIZATION, v);
            }
            Err(_) => {
                return Err(Failure {
                    status: None,
                    message: "API key chứa ký tự không hợp lệ".into(),
                    cooldown: false,
                })
            }
        }
        if a.provider.id == "openrouter" {
            headers.insert("HTTP-Referer", HeaderValue::from_static("http://localhost"));
            headers.insert("X-Title", HeaderValue::from_static("ai-gateway"));
        }
        let mut payload = body.clone();
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("model".to_string(), json!(a.model));
        }
        (url, payload)
    };

    for (k, v) in &a.provider.extra_headers {
        if k.trim().is_empty() {
            continue;
        }
        let (Ok(name), Ok(value)) = (k.parse::<axum::http::HeaderName>(), HeaderValue::from_str(v)) else {
            continue;
        };
        headers.insert(name, value);
    }

    match st.client.post(&url).headers(headers).json(&payload).send().await {
        Ok(r) => Ok(r),
        Err(e) => Err(Failure {
            status: None,
            message: format!("network error: {}", e),
            cooldown: true,
        }),
    }
}

fn tag(mut r: Response, a: &Attempt) -> Response {
    if let Ok(v) = HeaderValue::from_str(&a.provider.id) {
        r.headers_mut().insert("x-ai-provider", v);
    }
    if let Ok(v) = HeaderValue::from_str(&a.account.name) {
        r.headers_mut().insert("x-ai-account", v);
    }
    r
}

pub async fn attempt_once(
    st: &Arc<AppState>,
    a: &Attempt,
    body: &Value,
    wants_stream: bool,
) -> Result<Success, Failure> {
    let resp = send(st, a, body, wants_stream).await?;
    let status = resp.status();
    let code = status.as_u16();

    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(Failure {
            status: Some(code),
            message: upstream_error_message(&text),
            cooldown: !is_fatal(code),
        });
    }

    if a.provider.kind == "gemini" {
        if wants_stream {
            let body = gemini::convert_stream(resp, a.model.clone());
            let r = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .header(header::CACHE_CONTROL, "no-cache")
                .body(body)
                .unwrap();
            return Ok(Success { status: 200, response: tag(r, a) });
        }
        let bytes = match tokio::time::timeout(Duration::from_secs(180), resp.bytes()).await {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => {
                return Err(Failure { status: None, message: format!("đọc response lỗi: {}", e), cooldown: true })
            }
            Err(_) => {
                return Err(Failure { status: None, message: "upstream timeout".into(), cooldown: true })
            }
        };
        let v: Value = serde_json::from_slice(&bytes).map_err(|e| Failure {
            status: None,
            message: format!("response gemini không phải JSON: {}", e),
            cooldown: true,
        })?;
        if let Some(err) = v.get("error") {
            return Err(Failure {
                status: Some(400),
                message: upstream_error_message(&err.to_string()),
                cooldown: false,
            });
        }
        let out = gemini::to_openai_response(&v, &a.model);
        return Ok(Success {
            status: 200,
            response: tag(Json(out).into_response(), a),
        });
    }

    if wants_stream {
        let stream = resp.bytes_stream();
        let r = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from_stream(stream))
            .unwrap();
        return Ok(Success { status: 200, response: tag(r, a) });
    }

    let bytes = match tokio::time::timeout(Duration::from_secs(180), resp.bytes()).await {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            return Err(Failure { status: None, message: format!("đọc response lỗi: {}", e), cooldown: true })
        }
        Err(_) => return Err(Failure { status: None, message: "upstream timeout".into(), cooldown: true }),
    };
    let r = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .unwrap();
    Ok(Success { status: 200, response: tag(r, a) })
}

pub async fn execute(st: Arc<AppState>, cfg: Config, body: Value, wants_stream: bool, attempts: Vec<Attempt>) -> Response {
    let mut errors: Vec<Value> = Vec::new();
    let t0 = std::time::Instant::now();

    // Thống kê số account dự kiến thử cho từng provider trong lượt request này
    let mut provider_total: HashMap<String, usize> = HashMap::new();
    for a in &attempts {
        *provider_total.entry(a.provider.id.clone()).or_insert(0) += 1;
    }
    let mut provider_failed: HashMap<String, usize> = HashMap::new();

    for a in attempts {
        let key = stat_key(&a.provider.id, &a.account.name);
        match attempt_once(&st, &a, &body, wants_stream).await {
            Ok(s) => {
                let ms = t0.elapsed().as_millis();
                st.record_success(&key, s.status);
                // Nếu provider này từng bị tạm ngưng trong phiên thì tự động gỡ ban
                st.unban_provider(&a.provider.id);
                println!("<-- [THÀNH CÔNG] Provider: [{}] | Account: [{}] | Model: [{}] ({}ms)", a.provider.id, a.account.name, a.model, ms);
                tracing::info!(provider = %a.provider.id, account = %a.account.name, model = %a.model, "request ok");
                return s.response;
            }
            Err(f) => {
                st.record_error(&key, f.status, f.message.clone());
                if f.cooldown {
                    st.cooldown(&key, cfg.cooldown_secs);
                }
                println!("<-- [CHUYỂN TIẾP] Provider: [{}] ({}) gặp lỗi: {} -> Đang thử provider kế tiếp...", a.provider.id, a.account.name, f.message);
                tracing::warn!(provider = %a.provider.id, account = %a.account.name, status = ?f.status, error = %f.message, "upstream failed");
                errors.push(json!({
                    "provider": a.provider.id,
                    "account": a.account.name,
                    "model": a.model,
                    "status": f.status,
                    "error": f.message,
                }));

                let failed_count = provider_failed.entry(a.provider.id.clone()).or_insert(0);
                *failed_count += 1;
                let total_accs = provider_total.get(&a.provider.id).copied().unwrap_or(1);

                // Khi TẤT CẢ các account của provider này đều thất bại trong request:
                // Lập tức tạm ngưng provider trong phiên làm việc hiện tại để các request sau không bị delay!
                if *failed_count >= total_accs && cfg.session_ban_on_failure {
                    st.ban_provider(&a.provider.id, &f.message);
                }
            }
        }
    }

    println!("<-- [THẤT BẠI] Tất cả provider khả dụng đều không thể hoàn thành request!");
    let message = if errors.is_empty() {
        "không có provider/account khả dụng (bật provider, thêm key, hoặc chờ hết cooldown)"
    } else {
        "tất cả provider/account đều thất bại"
    };
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({
            "error": {
                "message": message,
                "type": "upstream_error",
                "attempts": errors,
            }
        })),
    )
        .into_response()
}

pub fn test_body() -> Value {
    json!({
        "model": "auto",
        "messages": [{ "role": "user", "content": "Reply with exactly: pong" }],
        "max_tokens": 16,
        "temperature": 0,
        "stream": false,
    })
}

pub async fn benchmark_providers(st: Arc<AppState>) -> Value {
    let cfg = st.config.read().await.clone();
    let enabled_providers: Vec<Provider> = cfg
        .providers
        .iter()
        .filter(|p| p.enabled && p.accounts.iter().any(|a| a.enabled && !a.api_key.trim().is_empty()))
        .cloned()
        .collect();

    if enabled_providers.is_empty() {
        return json!({ "ok": true, "results": [] });
    }

    println!("--> [BENCHMARK] Đang đo tốc độ {} provider khả dụng...", enabled_providers.len());

    let mut set = tokio::task::JoinSet::new();
    for p in enabled_providers {
        let st = st.clone();
        let ban_on_fail = cfg.session_ban_on_failure;
        let cooldown_secs = cfg.cooldown_secs;
        set.spawn(async move {
            let accounts: Vec<Account> = p
                .accounts
                .iter()
                .filter(|a| a.enabled && !a.api_key.trim().is_empty())
                .cloned()
                .collect();
            let mut last_err = String::new();
            for a in accounts {
                let key = stat_key(&p.id, &a.name);
                let attempt = Attempt {
                    provider: p.clone(),
                    account: a.clone(),
                    model: p.model.clone(),
                };
                let t0 = std::time::Instant::now();
                match tokio::time::timeout(Duration::from_secs(20), attempt_once(&st, &attempt, &test_body(), false)).await {
                    Ok(Ok(_s)) => {
                        let ms = t0.elapsed().as_millis() as u64;
                        st.record_latency(&p.id, &a.name, ms);
                        st.unban_provider(&p.id);
                        println!("    ⚡ [{}] ({}) -> {}ms", p.id, p.model, ms);
                        return json!({ "provider": p.id, "model": p.model, "account": a.name, "ok": true, "ms": ms });
                    }
                    Ok(Err(f)) => {
                        if f.cooldown {
                            st.cooldown(&key, cooldown_secs);
                        }
                        last_err = f.message;
                    }
                    Err(_) => {
                        // Quá 20s lúc đo benchmark: coi như model chậm, xếp xuống cuối chứ không ban phiên
                        let ms = 20_000;
                        st.record_latency(&p.id, &a.name, ms);
                        println!("    🐢 [{}] ({}) -> >{}ms (chậm)", p.id, p.model, ms);
                        return json!({ "provider": p.id, "model": p.model, "account": a.name, "ok": true, "ms": ms, "slow": true });
                    }
                }
            }
            if ban_on_fail && !last_err.is_empty() {
                st.ban_provider(&p.id, &last_err);
            }
            println!("    ✗ [{}] ({}) -> Lỗi: {}", p.id, p.model, last_err);
            json!({ "provider": p.id, "model": p.model, "ok": false, "error": last_err })
        });
    }

    let mut results = Vec::new();
    while let Some(res) = set.join_next().await {
        if let Ok(v) = res {
            results.push(v);
        }
    }
    results.sort_by_key(|v| v.get("ms").and_then(|x| x.as_u64()).unwrap_or(u64::MAX));
    println!("<-- [BENCHMARK] Hoàn tất đo tốc độ {} provider.", results.len());
    json!({ "ok": true, "results": results })
}
