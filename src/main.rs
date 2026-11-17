mod api;
mod config;
mod gemini;
mod proxy;
mod state;

use crate::config::Config;
use crate::state::AppState;
use axum::routing::{get, post};
use axum::Router;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ai_gateway=info".into()),
        )
        .init();

    let path = std::env::var("AI_GATEWAY_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("config.json"));
    let cfg = Config::load(&path)?;
    let listen = cfg.listen.clone();
    let requires_key = cfg.has_auth();
    let state = Arc::new(AppState::new(cfg, path.clone()));

    let app = Router::new()
        .route("/", get(api::ui))
        .route("/ui", get(api::ui))
        .route("/health", get(api::health))
        .route("/v1/chat/completions", post(api::chat))
        .route("/v1/models", get(api::models))
        .route("/api/config", get(api::get_config).put(api::put_config))
        .route("/api/config/reload", post(api::reload))
        .route("/api/status", get(api::status))
        .route("/api/test", post(api::test_account))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr: SocketAddr = listen
        .parse()
        .map_err(|e| anyhow::anyhow!("listen '{}' không hợp lệ: {}", listen, e))?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    println!("  AI Gateway is running");
    println!("  API     http://{}", addr);
    println!("  UI      http://{}/", addr);
    println!("  Config  {}", path.display());
    if !requires_key {
        println!("  Note: gateway_api_key is empty, so the API is open to any client");
    }

    axum::serve(listener, app).await?;
    Ok(())
}
