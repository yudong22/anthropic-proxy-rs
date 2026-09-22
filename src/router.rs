//! Shared axum router construction for the proxy API.
//!
//! Both the headless binary (`src/main.rs`) and the Tauri GUI
//! (`src-tauri/src/main.rs`) build their HTTP listener from this single
//! function so that every route — including `GET /v1/credits` — is registered
//! in exactly one place. Adding a route here makes it available in both
//! front-ends; neither binary should hand-roll the router.

use crate::credits;
use crate::metrics;
use crate::proxy;
use crate::service::ServiceController;
use crate::settings::LogBuffer;
use crate::stats::StatsDb;
use crate::Config;
use axum::{
    routing::{get, post},
    Extension, Router,
};
use reqwest::Client;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

/// Build the proxy API router with all extensions already attached.
///
/// `metrics_handle` is the handle returned by [`metrics::install`]; the
/// `/metrics` route clones it per request.
pub fn build_app_router(
    service_ctrl: Arc<ServiceController>,
    logs: Arc<LogBuffer>,
    config: Arc<Config>,
    client: Client,
    stats: Arc<StatsDb>,
    metrics_handle: metrics::PrometheusHandle,
) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route("/v1/messages", post(proxy::proxy_handler))
        .route("/v1/responses", post(proxy::responses_proxy_handler))
        .route("/responses", post(proxy::responses_proxy_handler))
        .route(
            "/backend-api/codex/responses",
            post(proxy::responses_proxy_handler),
        )
        .route(
            "/v1/chat/completions",
            post(proxy::chat_completions_proxy_handler),
        )
        .route(
            "/chat/completions",
            post(proxy::chat_completions_proxy_handler),
        )
        .route("/v1/models", get(proxy::list_models_handler))
        .route("/models", get(proxy::list_models_handler))
        .route("/v1/credits", get(credits::credits_handler))
        .route("/credits", get(credits::credits_handler))
        .route("/health", get(|| async { "OK" }))
        .route(
            "/metrics",
            get(move || {
                let handle = metrics_handle.clone();
                async move { handle.render() }
            }),
        )
        .layer(Extension(service_ctrl))
        .layer(Extension(logs))
        .layer(Extension(config))
        .layer(Extension(client))
        .layer(Extension(stats))
        .layer(TraceLayer::new_for_http())
        .layer(cors)
}
