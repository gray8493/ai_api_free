use crate::state::now_ms;
use axum::body::{Body, Bytes};
use futures_util::StreamExt;
use serde_json::{json, Value};

fn map_finish(reason: &str) -> &'static str {
    match reason {
        "STOP" => "stop",
        "MAX_TOKENS" => "length",
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" => "content_filter",
        _ => "stop",
    }
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

pub fn build_payload(body: &Value) -> Value {
    let mut contents: Vec<Value> = Vec::new();
    let mut system: Vec<Value> = Vec::new();

    if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
        for m in messages {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let text = text_of(m.get("content").unwrap_or(&Value::Null));
            if text.trim().is_empty() {
                continue;
            }
            match role {
                "system" | "developer" => system.push(json!({ "text": text })),
                "assistant" => contents.push(json!({ "role": "model", "parts": [{ "text": text }] })),
                _ => contents.push(json!({ "role": "user", "parts": [{ "text": text }] })),
            }
        }
    }

    let mut generation_config = serde_json::Map::new();
    if let Some(t) = body.get("temperature").and_then(|v| v.as_f64()) {
        generation_config.insert("temperature".into(), json!(t));
    }
    if let Some(p) = body.get("top_p").and_then(|v| v.as_f64()) {
        generation_config.insert("topP".into(), json!(p));
    }
    if let Some(n) = body
        .get("max_completion_tokens")
        .or_else(|| body.get("max_tokens"))
        .and_then(|v| v.as_i64())
    {
        generation_config.insert("maxOutputTokens".into(), json!(n));
    }
    if let Some(stop) = body.get("stop") {
        let seqs = match stop {
            Value::String(s) => vec![json!(s)],
            Value::Array(a) => a.iter().filter(|v| v.is_string()).cloned().collect(),
            _ => Vec::new(),
        };
        if !seqs.is_empty() {
            generation_config.insert("stopSequences".into(), json!(seqs));
        }
    }

    let mut payload = json!({ "contents": contents });
    if !system.is_empty() {
        payload["systemInstruction"] = json!({ "parts": system });
    }
    if !generation_config.is_empty() {
        payload["generationConfig"] = Value::Object(generation_config);
    }
    payload
}

pub fn to_openai_response(gem: &Value, model: &str) -> Value {
    let text = gem
        .pointer("/candidates/0/content/parts")
        .and_then(|p| p.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();

    let finish = gem
        .pointer("/candidates/0/finishReason")
        .and_then(|f| f.as_str())
        .map(map_finish)
        .unwrap_or("stop");

    let prompt_tokens = gem
        .pointer("/usageMetadata/promptTokenCount")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let completion_tokens = gem
        .pointer("/usageMetadata/candidatesTokenCount")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    json!({
        "id": format!("chatcmpl-{}", now_ms()),
        "object": "chat.completion",
        "created": (now_ms() / 1000) as i64,
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": finish,
        }],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
        }
    })
}

pub fn convert_stream(resp: reqwest::Response, model: String) -> Body {
    let stream = async_stream::stream! {
        let id = format!("chatcmpl-{}", now_ms());
        let created = (now_ms() / 1000) as i64;
        let mut buf = String::new();
        let mut sent_role = false;
        let mut upstream = resp.bytes_stream();

        while let Some(item) = upstream.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(_) => break,
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].trim_end_matches('\r').to_string();
                buf.drain(..=pos);
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                let text = v
                    .pointer("/candidates/0/content/parts")
                    .and_then(|p| p.as_array())
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .unwrap_or_default();
                let finish = v
                    .pointer("/candidates/0/finishReason")
                    .and_then(|f| f.as_str())
                    .map(map_finish);

                let mut delta = serde_json::Map::new();
                if !sent_role {
                    delta.insert("role".into(), json!("assistant"));
                    sent_role = true;
                }
                if !text.is_empty() {
                    delta.insert("content".into(), json!(text));
                }
                if delta.is_empty() && finish.is_none() {
                    continue;
                }
                let out = json!({
                    "id": id.clone(),
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model.clone(),
                    "choices": [{
                        "index": 0,
                        "delta": Value::Object(delta),
                        "finish_reason": finish,
                    }]
                });
                yield Ok::<Bytes, std::io::Error>(Bytes::from(format!("data: {}\n\n", out)));
            }
        }
        yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
    };
    Body::from_stream(stream)
}
