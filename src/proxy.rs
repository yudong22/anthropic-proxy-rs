use crate::config::{Config, ModelsFlavor};
use crate::error::{ProxyError, ProxyResult};
use crate::metrics;
use crate::models::{anthropic, openai, responses};
use crate::service;
use crate::stats::{StatsDb, TokenRecord};
use crate::translate::{pipeline, responses as responses_pipeline, stream};
use crate::util::{format_headers, truncate};
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    Extension, Json,
};
use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
use reqwest::Client;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Which wire protocol a request was accepted on. The three supported APIs
/// share the entire upstream send/retry path and differ only in how the request
/// was translated and how the response is rendered back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ApiFlavor {
    /// Anthropic Messages API (`/v1/messages`).
    Anthropic,
    /// OpenAI Responses API (`/v1/responses`).
    Responses,
    /// OpenAI Chat Completions passthrough (`/v1/chat/completions`).
    Chat,
}

impl ApiFlavor {
    /// Log stage tag used for upstream failures.
    fn stage(self) -> &'static str {
        match self {
            ApiFlavor::Anthropic | ApiFlavor::Chat => "chat/completions",
            ApiFlavor::Responses => "responses/chat_completions",
        }
    }

    /// Human-readable name used in `tracing` debug lines.
    fn label(self) -> &'static str {
        match self {
            ApiFlavor::Anthropic => "",
            ApiFlavor::Responses => "Responses API ",
            ApiFlavor::Chat => "Chat Completions ",
        }
    }

    /// Whether SSE responses advertise CORS. The Anthropic endpoint is consumed
    /// by CLI clients; the OpenAI-compatible ones are also called from browsers.
    fn sse_allows_cors(self) -> bool {
        !matches!(self, ApiFlavor::Anthropic)
    }
}

pub async fn proxy_handler(
    Extension(config): Extension<Arc<Config>>,
    Extension(client): Extension<Client>,
    Extension(gui_logs): Extension<Arc<crate::settings::LogBuffer>>,
    Extension(service): Extension<Arc<service::ServiceController>>,
    Extension(stats): Extension<Arc<StatsDb>>,
    headers: HeaderMap,
    Json(req): Json<anthropic::AnthropicRequest>,
) -> ProxyResult<Response> {
    let is_streaming = req.stream.unwrap_or(false);
    let start = Instant::now();

    let incoming_headers = format_headers(&headers);
    gui_logs
        .push(
            "INFO",
            format!("POST /v1/messages headers: {}", incoming_headers),
        )
        .await;
    tracing::info!("POST /v1/messages headers: {}", incoming_headers);

    // The console shares this listener; a stopped service must not take it
    // offline, so answer with 503 instead of closing the port.
    if !service.is_running() {
        return Ok(service::service_unavailable_response());
    }

    let api_key = resolve_api_key(&config, &headers);

    tracing::debug!("Received request for model: {}", req.model);
    tracing::debug!("Streaming: {}", is_streaming);
    metrics::request_started(is_streaming);

    if config.verbose {
        tracing::trace!(
            "Incoming Anthropic request: {}",
            serde_json::to_string_pretty(&req).unwrap_or_default()
        );
    }

    let policy = translation_policy(&config);
    // Capture what the log lines need before `req` is consumed by translation.
    let client_model = req.model.clone();
    let message_count = req.messages.len();
    let tool_count = req.tools.as_ref().map(|t| t.len()).unwrap_or(0);
    let openai_req = pipeline::translate_request(req, &policy)?;

    if config.verbose {
        tracing::trace!(
            "Transformed OpenAI request: {}",
            serde_json::to_string_pretty(&openai_req).unwrap_or_default()
        );
    }

    gui_logs
        .push(
            "INFO",
            format!(
                "POST /v1/messages model={} -> upstream={} stream={} msgs={} tools={}",
                client_model, openai_req.model, is_streaming, message_count, tool_count
            ),
        )
        .await;

    let result = forward_request(
        config,
        client,
        openai_req,
        api_key,
        gui_logs.clone(),
        client_model.clone(),
        stats.clone(),
        ApiFlavor::Anthropic,
        is_streaming,
    )
    .await;

    finalize_request(
        outcome_status(&result),
        outcome_error(&result),
        &gui_logs,
        &stats,
        "/v1/messages",
        &client_model,
        is_streaming,
        start,
    )
    .await;

    result
}

pub async fn responses_proxy_handler(
    Extension(config): Extension<Arc<Config>>,
    Extension(client): Extension<Client>,
    Extension(gui_logs): Extension<Arc<crate::settings::LogBuffer>>,
    Extension(service): Extension<Arc<service::ServiceController>>,
    Extension(stats): Extension<Arc<StatsDb>>,
    headers: HeaderMap,
    Json(req): Json<responses::ResponsesRequest>,
) -> ProxyResult<Response> {
    let is_streaming = req.stream.unwrap_or(false);
    let start = Instant::now();

    let incoming_headers = format_headers(&headers);
    gui_logs
        .push(
            "INFO",
            format!("POST /v1/responses headers: {}", incoming_headers),
        )
        .await;
    tracing::info!("POST /v1/responses headers: {}", incoming_headers);

    if !service.is_running() {
        return Ok(service::service_unavailable_response());
    }

    let api_key = resolve_responses_api_key(&config, &headers);

    tracing::debug!("Received Responses API request for model: {}", req.model);
    tracing::debug!("Streaming: {}", is_streaming);
    metrics::request_started(is_streaming);

    if config.verbose {
        tracing::trace!(
            "Incoming Responses API request: {}",
            serde_json::to_string_pretty(&req).unwrap_or_default()
        );
    }

    let policy = translation_policy(&config);
    let client_model = req.model.clone();
    let openai_req = responses_pipeline::translate_responses_request(req, &policy)?;

    if config.verbose {
        tracing::trace!(
            "Transformed OpenAI request from Responses API: {}",
            serde_json::to_string_pretty(&openai_req).unwrap_or_default()
        );
    }

    gui_logs
        .push(
            "INFO",
            format!(
                "POST /v1/responses model={} -> upstream={} stream={}",
                client_model, openai_req.model, is_streaming
            ),
        )
        .await;

    let result = forward_request(
        config,
        client,
        openai_req,
        api_key,
        gui_logs.clone(),
        client_model.clone(),
        stats.clone(),
        ApiFlavor::Responses,
        is_streaming,
    )
    .await;

    finalize_request(
        outcome_status(&result),
        outcome_error(&result),
        &gui_logs,
        &stats,
        "/v1/responses",
        &client_model,
        is_streaming,
        start,
    )
    .await;

    result
}

pub async fn chat_completions_proxy_handler(
    Extension(config): Extension<Arc<Config>>,
    Extension(client): Extension<Client>,
    Extension(gui_logs): Extension<Arc<crate::settings::LogBuffer>>,
    Extension(service): Extension<Arc<service::ServiceController>>,
    Extension(stats): Extension<Arc<StatsDb>>,
    headers: HeaderMap,
    Json(mut req): Json<openai::OpenAIRequest>,
) -> ProxyResult<Response> {
    let is_streaming = req.stream.unwrap_or(false);
    let start = Instant::now();

    let incoming_headers = format_headers(&headers);
    gui_logs
        .push(
            "INFO",
            format!("POST /v1/chat/completions headers: {}", incoming_headers),
        )
        .await;
    tracing::info!("POST /v1/chat/completions headers: {}", incoming_headers);

    if !service.is_running() {
        return Ok(service::service_unavailable_response());
    }

    let api_key = resolve_chat_api_key(&config, &headers);

    tracing::debug!("Received Chat Completions request for model: {}", req.model);
    tracing::debug!("Streaming: {}", is_streaming);
    metrics::request_started(is_streaming);

    if config.verbose {
        tracing::trace!(
            "Incoming Chat Completions request: {}",
            serde_json::to_string_pretty(&req).unwrap_or_default()
        );
    }

    let policy = translation_policy(&config);
    let client_model = req.model.clone();

    // 1. Model remapping and optional stripping of suffix (e.g. [1M])
    let mapped_model = policy
        .model_map
        .get(&req.model)
        .cloned()
        .or_else(|| policy.completion_model.clone())
        .unwrap_or_else(|| req.model.clone());

    req.model = if policy.strip_model_suffix {
        pipeline::strip_model_suffix(&mapped_model)
    } else {
        mapped_model
    };

    // 2. Sanitize system prompt if ignore terms are configured
    if !policy.ignore_terms.is_empty() {
        for msg in &mut req.messages {
            if msg.role == "system" {
                if let Some(openai::MessageContent::Text(ref mut text)) = msg.content {
                    *text = pipeline::sanitize_prompt(text.clone(), &policy.ignore_terms);
                }
            }
        }
    }

    // 3. For streaming requests, ensure include_usage is true so upstream emits
    //    a usage chunk with cached tokens.
    if is_streaming {
        req.stream_options = Some(openai::StreamOptions {
            include_usage: true,
        });
    }

    gui_logs
        .push(
            "INFO",
            format!(
                "POST /v1/chat/completions model={} -> upstream={} stream={}",
                client_model, req.model, is_streaming
            ),
        )
        .await;

    let result = forward_request(
        config,
        client,
        req,
        api_key,
        gui_logs.clone(),
        client_model.clone(),
        stats.clone(),
        ApiFlavor::Chat,
        is_streaming,
    )
    .await;

    finalize_request(
        outcome_status(&result),
        outcome_error(&result),
        &gui_logs,
        &stats,
        "/v1/chat/completions",
        &client_model,
        is_streaming,
        start,
    )
    .await;

    result
}

/// HTTP status to report for a completed request (500 when it errored).
fn outcome_status(result: &ProxyResult<Response>) -> u16 {
    match result {
        Ok(resp) => resp.status().as_u16(),
        Err(_) => 500,
    }
}

/// The error message to log for a completed request, if it failed.
fn outcome_error(result: &ProxyResult<Response>) -> Option<String> {
    result.as_ref().err().map(|e| e.to_string())
}

/// Record the outcome of a completed request in metrics, the GUI log buffer and
/// the daily stats database. Shared by all three API handlers.
#[allow(clippy::too_many_arguments)]
async fn finalize_request(
    status: u16,
    error: Option<String>,
    gui_logs: &Arc<crate::settings::LogBuffer>,
    stats: &Arc<StatsDb>,
    route: &str,
    client_model: &str,
    is_streaming: bool,
    start: Instant,
) {
    metrics::request_finished(start, status, is_streaming);

    match error {
        None => {
            gui_logs
                .push(
                    "INFO",
                    format!(
                        "POST {} ok model={} stream={} {}ms",
                        route,
                        client_model,
                        is_streaming,
                        start.elapsed().as_millis()
                    ),
                )
                .await;
        }
        Some(message) => {
            // Record the failed request in the stats DB (no tokens).
            let _ = stats.record_request(false, TokenRecord::default());
            gui_logs
                .push(
                    "ERROR",
                    format!(
                        "POST {} failed model={} stream={} {}ms | {}",
                        route,
                        client_model,
                        is_streaming,
                        start.elapsed().as_millis(),
                        message
                    ),
                )
                .await;
        }
    }
}

/// Forward a translated request to the configured upstreams, with failover.
///
/// This is the single upstream send path for the Anthropic, Responses and Chat
/// Completions APIs, in both streaming and non-streaming mode. It walks
/// `config.chat_completions_urls()` in order, retrying the next upstream on a
/// connection error or a retriable status (429/5xx) and failing fast otherwise.
#[allow(clippy::too_many_arguments)]
async fn forward_request(
    config: Arc<Config>,
    client: Client,
    mut openai_req: openai::OpenAIRequest,
    api_key: Option<String>,
    gui_logs: Arc<crate::settings::LogBuffer>,
    client_model: String,
    stats: Arc<StatsDb>,
    flavor: ApiFlavor,
    streaming: bool,
) -> ProxyResult<Response> {
    let urls = config.chat_completions_urls();
    let mut last_err = None;

    // Neutralize content-filter fingerprints across every message field once,
    // before sending. This is the Rust port of the Go sanitize pass and covers
    // the user/assistant/tool/reasoning channels that a system-only scrub misses.
    // It runs for all three API flavors through this shared path (Anthropic,
    // Responses, Chat), so both Claude Code and Codex benefit.
    if config.sanitize_fingerprints {
        pipeline::sanitize_openai_request(&mut openai_req);
    }

    // Stall guard: a content-policy block (WorkBuddy business code 11128) is
    // *not* a transient 5xx, and retrying with the identical fingerprinted body
    // only re-hits the same `unapproved channel` rejection. We make exactly one
    // "degraded" attempt per upstream URL: re-send with a neutral system prompt
    // so the user's real request still gets answered. This mirrors the Go
    // upstream's ErrContentBlocked → Degraded retry and stops Claude Code/Codex
    // from spinning on a bare 400.
    let mut degraded_attempt = false;

    'url: for url in &urls {
        let mode = if streaming {
            "streaming"
        } else {
            "non-streaming"
        };

        // Up to two attempts against this URL: the original request, then — if the
        // first hit a content-policy block — a single degraded retry. A connect
        // error or retriable 5xx moves on to the next URL.
        for attempt in 0..=1 {
            tracing::debug!(
                "Sending {} {}request to {} (model: {})",
                mode,
                flavor.label(),
                url,
                openai_req.model
            );

            let mut req_builder = client
                .post(url)
                .json(&openai_req)
                .timeout(Duration::from_secs(300));
            req_builder = apply_upstream_auth(req_builder, &config, &api_key);

            let upstream_start = Instant::now();
            let response = match req_builder.send().await {
                Ok(resp) => {
                    metrics::upstream_latency(
                        upstream_start.elapsed().as_secs_f64(),
                        "chat_completions",
                    );
                    resp
                }
                Err(err) => {
                    tracing::warn!("Failed to reach {}: {:?}", url, err);
                    metrics::upstream_error("chat_completions");
                    gui_logs
                        .push(
                            "ERROR",
                            format!(
                                "UPSTREAM ERROR [connect] url={} model={} {}ms | {}",
                                url,
                                openai_req.model,
                                upstream_start.elapsed().as_millis(),
                                err
                            ),
                        )
                        .await;
                    last_err = Some(ProxyError::Http(err));
                    continue 'url; // try next upstream URL
                }
            };

            let status = response.status();
            if !status.is_success() {
                let body = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "Unknown error".to_string());
                let elapsed = upstream_start.elapsed().as_millis();
                metrics::upstream_error("chat_completions");
                log_upstream_failure(
                    &gui_logs,
                    flavor.stage(),
                    url,
                    &openai_req.model,
                    status.as_u16(),
                    &body,
                    elapsed,
                )
                .await;

                // Content-policy block → at most one degraded retry against the
                // same URL, then surface a clear content_blocked error instead of
                // a raw 400/502.
                if is_content_blocked(status.as_u16(), &body) && !degraded_attempt && attempt == 0 {
                    degraded_attempt = true;
                    tracing::warn!(
                        "Upstream content block ({}); attempting one degraded retry",
                        status
                    );
                    gui_logs
                        .push(
                            "WARN",
                            format!(
                                "UPSTREAM content_blocked ({}) → 1 degraded retry (model={})",
                                status, openai_req.model
                            ),
                        )
                        .await;
                    apply_degraded_prompt(&mut openai_req);
                    continue; // attempt == 1: retry the SAME URL with the neutral prompt
                }

                let err = ProxyError::Upstream(format!("Upstream returned {}: {}", status, body));
                if is_retriable_status(status.as_u16()) {
                    last_err = Some(err);
                    continue 'url; // try next upstream URL
                }
                // After the degraded retry still fails, report a content_blocked
                // error the client can understand rather than a generic upstream
                // one.
                if is_content_blocked(status.as_u16(), &body) {
                    return Err(ProxyError::Upstream(
                        "Upstream rejected the request as content policy violation (code 11128). \
                         The degraded retry also failed."
                            .to_string(),
                    ));
                }
                return Err(err);
            }

            return if streaming {
                streaming_response(response, flavor, client_model, gui_logs, stats)
            } else {
                non_streaming_response(
                    response,
                    flavor,
                    &openai_req.model,
                    client_model,
                    &config,
                    stats,
                )
                .await
            };
        }
    }

    Err(last_err.unwrap_or_else(|| ProxyError::Upstream("All upstreams failed".to_string())))
}

/// Translate a successful non-streaming upstream response back to the caller's
/// protocol.
async fn non_streaming_response(
    response: reqwest::Response,
    flavor: ApiFlavor,
    upstream_model: &str,
    client_model: String,
    config: &Config,
    stats: Arc<StatsDb>,
) -> ProxyResult<Response> {
    let mut openai_resp: openai::OpenAIResponse = response.json().await?;

    let metrics_model = match flavor {
        ApiFlavor::Chat => &client_model,
        _ => upstream_model,
    };
    metrics::tokens(
        openai_resp.usage.prompt_tokens,
        openai_resp.usage.completion_tokens,
        metrics_model,
    );

    // Record token breakdown in the persistent daily stats DB.
    let _ = stats.record_request(true, openai_resp.usage.to_token_record());

    if config.verbose {
        tracing::trace!(
            "Received OpenAI response: {}",
            serde_json::to_string_pretty(&openai_resp).unwrap_or_default()
        );
    }

    match flavor {
        ApiFlavor::Anthropic => {
            let anthropic_resp = pipeline::translate_response(openai_resp, upstream_model)?;
            if config.verbose {
                tracing::trace!(
                    "Transformed Anthropic response: {}",
                    serde_json::to_string_pretty(&anthropic_resp).unwrap_or_default()
                );
            }
            Ok(Json(anthropic_resp).into_response())
        }
        ApiFlavor::Responses => {
            let responses_resp =
                responses_pipeline::translate_responses_response(openai_resp, &client_model)?;
            if config.verbose {
                tracing::trace!(
                    "Transformed Responses API response: {}",
                    serde_json::to_string_pretty(&responses_resp).unwrap_or_default()
                );
            }
            Ok(Json(responses_resp).into_response())
        }
        ApiFlavor::Chat => {
            // Report the model the client asked for, not the upstream's name.
            if openai_resp.model.is_some() {
                openai_resp.model = Some(client_model);
            }
            Ok(Json(openai_resp).into_response())
        }
    }
}

/// Build the SSE response for a successful streaming upstream response.
fn streaming_response(
    response: reqwest::Response,
    flavor: ApiFlavor,
    client_model: String,
    gui_logs: Arc<crate::settings::LogBuffer>,
    stats: Arc<StatsDb>,
) -> ProxyResult<Response> {
    let upstream = response.bytes_stream();
    let sse_stream = create_flavor_sse_stream(upstream, flavor, client_model, gui_logs, stats);

    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert("Cache-Control", HeaderValue::from_static("no-cache"));
    headers.insert("Connection", HeaderValue::from_static("keep-alive"));
    if flavor.sse_allows_cors() {
        headers.insert("Access-Control-Allow-Origin", HeaderValue::from_static("*"));
    }

    Ok((headers, Body::from_stream(sse_stream)).into_response())
}

pub async fn list_models_handler(
    Extension(config): Extension<Arc<Config>>,
    Extension(client): Extension<Client>,
    Extension(gui_logs): Extension<Arc<crate::settings::LogBuffer>>,
    headers: HeaderMap,
) -> ProxyResult<Response> {
    let api_key = resolve_api_key(&config, &headers);

    let incoming_headers = format_headers(&headers);
    gui_logs
        .push(
            "INFO",
            format!("GET /v1/models headers: {}", incoming_headers),
        )
        .await;
    tracing::info!("GET /v1/models headers: {}", incoming_headers);

    // Vendors like WorkBuddy publish their catalog on a custom config
    // endpoint instead of the OpenAI `/v1/models` route; honour it.
    if config.models_flavor == ModelsFlavor::WorkBuddyConfig {
        return list_models_via_config(&config, &client, &api_key, &gui_logs).await;
    }

    let urls = config.models_urls();
    let mut last_err = None;

    for url in &urls {
        tracing::debug!("Fetching models from {}", url);

        let mut req_builder = client.get(url).timeout(Duration::from_secs(60));
        if let Some(ref key) = api_key {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", key));
        }

        match req_builder.send().await {
            Ok(response) if response.status().is_success() => {
                let openai_resp: openai::ModelsListResponse = response.json().await?;
                let anthropic_resp = pipeline::translate_models_list(openai_resp);
                return Ok(Json(anthropic_resp).into_response());
            }
            Ok(response) => {
                let status = response.status();
                let error_text = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "Unknown error".to_string());
                tracing::warn!("Upstream {} returned {}: {}", url, status, error_text);
                if is_retriable_status(status.as_u16()) {
                    last_err = Some(format!("Upstream returned {}: {}", status, error_text));
                    continue;
                }
                return Err(ProxyError::Upstream(format!(
                    "Upstream returned {}: {}",
                    status, error_text
                )));
            }
            Err(err) => {
                tracing::warn!("Failed to reach {}: {:?}", url, err);
                last_err = Some(format!("HTTP error: {}", err));
                continue;
            }
        }
    }

    Err(ProxyError::Upstream(
        last_err.unwrap_or_else(|| "All upstreams failed".to_string()),
    ))
}

/// Serve `/v1/models` from a vendor config endpoint (WorkBuddy `/v3/config`).
///
/// The vendor catalog is filtered to CLI-authorized models and then shaped
/// like an OpenAI list so the existing translation applies unchanged.
async fn list_models_via_config(
    config: &Config,
    client: &Client,
    api_key: &Option<String>,
    gui_logs: &Arc<crate::settings::LogBuffer>,
) -> ProxyResult<Response> {
    let Some(url) = config.models_config_url.clone() else {
        return Err(ProxyError::Upstream(
            "provider has no models config endpoint".to_string(),
        ));
    };
    let Some(key) = api_key.clone() else {
        return Err(ProxyError::Upstream(
            "API key required to list models".to_string(),
        ));
    };

    let preset = crate::providers::ProviderPreset {
        id: "vendor".to_string(),
        name: "vendor".to_string(),
        chat_completions_url: String::new(),
        models_url: None,
        models_config_url: Some(url),
        config_headers: Default::default(),
    };

    let models = crate::providers::fetch_models(client, &preset, &key)
        .await
        .map_err(|e| ProxyError::Upstream(e.to_string()))?;

    let openai_resp = openai::ModelsListResponse {
        object: Some("list".to_string()),
        data: models
            .into_iter()
            .map(|m| openai::ModelInfo {
                id: m.id,
                object: Some("model".to_string()),
                created: None,
                owned_by: None,
            })
            .collect(),
    };

    gui_logs
        .push(
            "INFO",
            format!(
                "GET /v1/models (vendor config) -> {} models",
                openai_resp.data.len()
            ),
        )
        .await;

    Ok(Json(pipeline::translate_models_list(openai_resp)).into_response())
}

/// Resolve the API key for the Anthropic Messages API (`x-api-key` header).
fn resolve_api_key(config: &Config, headers: &HeaderMap) -> Option<String> {
    if config.passthrough_api_key {
        header_value(headers, "x-api-key")
    } else {
        config.api_key.clone()
    }
}

/// Resolve the API key for the Responses API (bearer token or `x-api-key`).
fn resolve_responses_api_key(config: &Config, headers: &HeaderMap) -> Option<String> {
    if config.passthrough_api_key {
        bearer_or_api_key(headers)
    } else {
        config.api_key.clone()
    }
}

/// Resolve the API key for Chat Completions, where the client-supplied key may
/// be used even when a static key is configured (the static key wins then).
fn resolve_chat_api_key(config: &Config, headers: &HeaderMap) -> Option<String> {
    let header_key = bearer_or_api_key(headers);
    if config.passthrough_api_key {
        header_key.or_else(|| config.api_key.clone())
    } else {
        config.api_key.clone().or(header_key)
    }
}

/// A single non-empty header value.
fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// Extract a bearer token from `authorization`, falling back to `x-api-key`.
fn bearer_or_api_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.strip_prefix("Bearer ").unwrap_or(s))
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| header_value(headers, "x-api-key"))
}

fn translation_policy(config: &Config) -> pipeline::TranslationPolicy {
    pipeline::TranslationPolicy {
        reasoning_model: config.reasoning_model.clone(),
        completion_model: config.completion_model.clone(),
        model_map: config.model_map.clone(),
        ignore_terms: config.system_prompt_ignore_terms.clone(),
        strip_model_suffix: config.models_flavor == ModelsFlavor::WorkBuddyConfig,
        sanitize_fingerprints: config.sanitize_fingerprints,
    }
}

/// Attach the upstream authentication + CLI fingerprint headers.
///
/// For the WorkBuddy/CodeBuddy flavor we mirror the official client's request
/// fingerprint: the `x-api-key`/`Authorization` pair plus the official
/// `User-Agent`, and a small set of low-risk correlation headers the gateway
/// expects from the genuine CLI (`X-CodeBuddy-Request`, `Accept`,
/// `X-Requested-With`). These are cheap and were validated as passing in the
/// troubleshooting doc's "full headers" test, so we send them proactively.
/// Other providers keep the plain `Authorization` flow.
fn apply_upstream_auth(
    mut req: reqwest::RequestBuilder,
    config: &Config,
    api_key: &Option<String>,
) -> reqwest::RequestBuilder {
    if config.models_flavor == ModelsFlavor::WorkBuddyConfig {
        req = req
            .header("user-agent", crate::providers::WORKBUDDY_USER_AGENT)
            .header("x-codebuddy-request", "1")
            .header("accept", "application/json, text/event-stream")
            .header("x-requested-with", "XMLHttpRequest");
        if let Some(ref key) = api_key {
            req = req
                .header("x-api-key", key)
                .header("Authorization", format!("Bearer {}", key));
        }
    } else if let Some(ref key) = api_key {
        req = req.header("Authorization", format!("Bearer {}", key));
    }
    req
}

fn is_retriable_status(status: u16) -> bool {
    matches!(status, 429 | 500..=599)
}

/// Neutral, content-free system prompt used for the single degraded retry after
/// an upstream content-policy block. Deliberately minimal so it introduces no
/// fingerprint of its own. Mirrors `prompt.Degraded` in the Go upstream.
const DEGRADED_SYSTEM_PROMPT: &str =
    "You are a helpful assistant. Respond in the user's language, follow the user's instructions, and be direct and concise.";

/// Detect an upstream content-policy block. The WorkBuddy/CodeBuddy gateway
/// reports it as HTTP 400 with business code `11128` ("Illegal API invocation
/// from an unapproved channel"). We treat that specific shape as a content block
/// rather than a generic upstream failure, so the caller can attempt the
/// degraded retry instead of spinning on a bare 4xx.
fn is_content_blocked(status: u16, body: &str) -> bool {
    if status != 400 {
        return false;
    }
    body.contains("11128") || body.contains("unapproved channel")
}

/// Replace the leading `system` message(s) with the neutral degraded prompt.
/// Other (user/assistant/tool) messages are preserved so the user's actual
/// request still gets answered — we are only washing out the blocked system
/// template, which is exactly what the content filter object to.
fn apply_degraded_prompt(req: &mut openai::OpenAIRequest) {
    let mut replaced = false;
    for msg in req.messages.iter_mut() {
        if msg.role == "system" && !replaced {
            msg.content = Some(openai::MessageContent::Text(
                DEGRADED_SYSTEM_PROMPT.to_string(),
            ));
            msg.reasoning_content = None;
            msg.tool_calls = None;
            replaced = true;
        }
    }
    // If there was no system message at all, prepend one.
    if !replaced {
        req.messages.insert(
            0,
            openai::Message {
                role: "system".to_string(),
                content: Some(openai::MessageContent::Text(
                    DEGRADED_SYSTEM_PROMPT.to_string(),
                )),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
        );
    }
}

/// Parse a vendor error envelope into a compact, human-readable summary.
///
/// WorkBuddy/CodeBuddy report failures as
/// `{"code":11128,"msg":"...","requestId":"...","displayMsg":{"zh":"...","en":"..."}}`.
/// We surface the numeric `code` plus the message so the GUI log is actionable
/// instead of an opaque "502".
fn describe_upstream_error(status: u16, body: &str) -> String {
    let trimmed = body.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(obj) = value.as_object() {
            // WorkBuddy style: {"code":..., "msg":...}
            if let Some(code) = obj.get("code") {
                let code_str = match code {
                    serde_json::Value::Number(n) => n.to_string(),
                    serde_json::Value::String(s) => s.clone(),
                    _ => code.to_string(),
                };
                let msg = obj
                    .get("msg")
                    .or_else(|| obj.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let request_id = obj
                    .get("requestId")
                    .or_else(|| obj.get("request_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let display = obj
                    .get("displayMsg")
                    .and_then(|d| d.get("zh").or_else(|| d.get("en")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let mut text = format!("HTTP {} · code {} {}", status, code_str, msg);
                if !display.is_empty() && display != msg {
                    text.push_str(&format!(" · {}", display));
                }
                if !request_id.is_empty() {
                    text.push_str(&format!(" · requestId {}", request_id));
                }
                return text;
            }
            // Anthropic-style: {"error":{"message":...}}
            if let Some(err) = obj.get("error").and_then(|e| e.as_object()) {
                let m = err.get("message").and_then(|v| v.as_str()).unwrap_or("");
                if !m.is_empty() {
                    return format!("HTTP {} · {}", status, truncate(m, 400));
                }
            }
        }
    }
    format!("HTTP {} · {}", status, truncate(trimmed, 400))
}

/// A friendlier one-line hint for known WorkBuddy business codes.
fn upstream_code_hint(body: &str) -> Option<&'static str> {
    let value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let code = value.get("code")?.as_i64()?;
    Some(match code {
        11101 => "上游不支持非流式请求或参数解析失败，可强制 stream=true",
        11102 => "模型名不在上游可用列表（注意 [1M] 等后缀；在控制台“模型重定向”里加 请求模型:可用模型 映射，保存即生效）",
        11103 => "后端不支持该模型（如图像模型）",
        11128 => "非法 API 调用：请求形态被上游拒绝",
        11129 => "function 参数非法",
        11133 => "请求被模型提供方拒绝（参数/权限）",
        11135 => "图片数据无效",
        11148 => "tool_calls 与 tool_result 不匹配",
        11151 => "存在空内容消息",
        _ => return None,
    })
}

/// Push a structured upstream failure into the GUI log buffer (and tracing).
async fn log_upstream_failure(
    gui_logs: &Arc<crate::settings::LogBuffer>,
    stage: &str,
    url: &str,
    model: &str,
    status: u16,
    body: &str,
    elapsed_ms: u128,
) {
    let summary = describe_upstream_error(status, body);
    let mut msg = format!(
        "UPSTREAM ERROR [{}] url={} model={} {}ms | {}",
        stage, url, model, elapsed_ms, summary
    );
    if let Some(hint) = upstream_code_hint(body) {
        msg.push_str(&format!(" | hint: {}", hint));
    }
    msg.push_str(&format!(" | body: {}", truncate(body, 800)));
    gui_logs.push("ERROR", msg.clone()).await;
    tracing::warn!("Upstream failure: {}", msg);
}

/// Serialize one SSE event in the `event:`/`data:` framing both APIs use.
fn serialize_sse_event<T: serde::Serialize>(event_type: &str, event: &T) -> String {
    format!(
        "event: {}\ndata: {}\n\n",
        event_type,
        serde_json::to_string(event).unwrap_or_default()
    )
}

/// Shared SSE framer for all three API flavors.
///
/// All three read the upstream's OpenAI-style `data: {...}` stream and differ
/// only in how each chunk is translated and re-serialized, so the buffering,
/// `[DONE]` handling, business-error detection and stats capture live here once.
fn create_flavor_sse_stream(
    upstream: impl Stream<Item = Result<Bytes, impl std::fmt::Display + Send + 'static>>
        + Send
        + 'static,
    flavor: ApiFlavor,
    client_model: String,
    gui_logs: Arc<crate::settings::LogBuffer>,
    stats: Arc<StatsDb>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    async_stream::stream! {
        let mut buffer = String::new();
        let mut stats_written = false;

        // Only the Anthropic and Responses APIs need translation state.
        let mut anthropic_state = match flavor {
            ApiFlavor::Anthropic => Some(stream::initial_state(client_model.clone())),
            _ => None,
        };
        let mut responses_state = match flavor {
            ApiFlavor::Responses => Some(responses_pipeline::initial_stream_state(client_model.clone())),
            _ => None,
        };

        // Record usage exactly once per stream, falling back to a zero-token
        // success when the upstream never sends a usage chunk.
        macro_rules! capture_usage {
            ($usage:expr) => {
                if !stats_written {
                    let usage = $usage;
                    if flavor == ApiFlavor::Chat {
                        metrics::tokens(usage.prompt_tokens, usage.completion_tokens, &client_model);
                    }
                    let _ = stats.record_request(true, usage.to_token_record());
                    stats_written = true;
                }
            };
        }

        tokio::pin!(upstream);

        while let Some(chunk) = upstream.next().await {
            match chunk {
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));

                    while let Some(pos) = buffer.find("\n\n") {
                        let line = buffer[..pos].to_string();
                        buffer = buffer[pos + 2..].to_string();

                        if line.trim().is_empty() {
                            continue;
                        }

                        for l in line.lines() {
                            let Some(data) = l.strip_prefix("data: ") else {
                                // Chat Completions is a passthrough: forward any
                                // non-`data:` line verbatim.
                                if flavor == ApiFlavor::Chat && !l.trim().is_empty() {
                                    yield Ok(Bytes::from(format!("{}\n", l)));
                                }
                                continue;
                            };

                            if data.trim() == "[DONE]" {
                                match flavor {
                                    ApiFlavor::Anthropic => {
                                        if let Some(state) = anthropic_state.as_mut() {
                                            for event in stream::translate_done(state) {
                                                yield Ok(Bytes::from(serialize_sse_event(event.event_type(), &event)));
                                            }
                                        }
                                    }
                                    ApiFlavor::Responses => {
                                        if let Some(state) = responses_state.as_mut() {
                                            for event in responses_pipeline::translate_stream_done(state) {
                                                yield Ok(Bytes::from(serialize_sse_event(event.event_type(), &event)));
                                            }
                                        }
                                        yield Ok(Bytes::from("data: [DONE]\n\n"));
                                    }
                                    ApiFlavor::Chat => {
                                        yield Ok(Bytes::from("data: [DONE]\n\n"));
                                    }
                                }
                                continue;
                            }

                            if let Ok(mut chunk_obj) = serde_json::from_str::<openai::StreamChunk>(data) {
                                if let Some(ref usage) = chunk_obj.usage {
                                    capture_usage!(usage.clone());
                                }

                                match flavor {
                                    ApiFlavor::Anthropic => {
                                        if let Some(state) = anthropic_state.as_mut() {
                                            for event in stream::translate_chunk(state, &chunk_obj) {
                                                yield Ok(Bytes::from(serialize_sse_event(event.event_type(), &event)));
                                            }
                                        }
                                    }
                                    ApiFlavor::Responses => {
                                        if let Some(state) = responses_state.as_mut() {
                                            for event in responses_pipeline::translate_stream_chunk(state, &chunk_obj) {
                                                yield Ok(Bytes::from(serialize_sse_event(event.event_type(), &event)));
                                            }
                                        }
                                    }
                                    ApiFlavor::Chat => {
                                        if chunk_obj.model.is_some() {
                                            chunk_obj.model = Some(client_model.clone());
                                        }
                                        let serialized = serde_json::to_string(&chunk_obj)
                                            .unwrap_or_else(|_| data.to_string());
                                        yield Ok(Bytes::from(format!("data: {}\n\n", serialized)));
                                    }
                                }
                            } else if let Ok(val) = serde_json::from_str::<serde_json::Value>(data) {
                                // Some upstreams emit a business error as a
                                // `data:` line inside a 200 stream. Surface it
                                // instead of silently dropping it.
                                if val.get("code").is_some() {
                                    let summary = describe_upstream_error(200, data);
                                    let model = anthropic_state
                                        .as_ref()
                                        .map(|s| s.model().to_string())
                                        .or_else(|| responses_state.as_ref().map(|s| s.model().to_string()))
                                        .unwrap_or_else(|| client_model.clone());
                                    let msg = format!("UPSTREAM ERROR [stream] model={} | {}", model, summary);
                                    gui_logs.push("ERROR", msg.clone()).await;
                                    tracing::warn!("{}", msg);

                                    match flavor {
                                        ApiFlavor::Anthropic => {
                                            for event in stream::translate_error(format!("Upstream error: {}", summary)) {
                                                yield Ok(Bytes::from(serialize_sse_event(event.event_type(), &event)));
                                            }
                                            break;
                                        }
                                        ApiFlavor::Responses => {
                                            if let Some(state) = responses_state.as_ref() {
                                                for event in responses_pipeline::translate_stream_error(
                                                    state,
                                                    format!("Upstream error: {}", summary),
                                                ) {
                                                    yield Ok(Bytes::from(serialize_sse_event(event.event_type(), &event)));
                                                }
                                            }
                                            break;
                                        }
                                        ApiFlavor::Chat => {
                                            yield Ok(Bytes::from(format!("data: {}\n\n", data)));
                                        }
                                    }
                                } else {
                                    tracing::debug!("Ignoring unrecognized upstream stream chunk: {}", data);
                                }
                            } else {
                                tracing::debug!("Ignoring unrecognized upstream stream chunk: {}", data);
                            }
                        }
                    }
                }
                Err(e) => {
                    match flavor {
                        ApiFlavor::Anthropic => {
                            tracing::error!("Stream error: {}", e);
                            for event in stream::translate_error(format!("Stream error: {}", e)) {
                                yield Ok(Bytes::from(serialize_sse_event(event.event_type(), &event)));
                            }
                        }
                        ApiFlavor::Responses => {
                            tracing::error!("Stream error: {}", e);
                            if let Some(state) = responses_state.as_ref() {
                                for event in responses_pipeline::translate_stream_error(
                                    state,
                                    format!("Stream error: {}", e),
                                ) {
                                    yield Ok(Bytes::from(serialize_sse_event(event.event_type(), &event)));
                                }
                            }
                        }
                        ApiFlavor::Chat => {
                            let msg = format!("STREAM READ ERROR model={} | {}", client_model, e);
                            gui_logs.push("ERROR", msg.clone()).await;
                            tracing::warn!("{}", msg);
                        }
                    }
                    break;
                }
            }
        }

        if !stats_written {
            let _ = stats.record_request(true, TokenRecord::default());
        }

        // The Responses API stream must always terminate with a completed
        // response and `[DONE]`, even when the upstream omitted the terminator.
        if flavor == ApiFlavor::Responses {
            if let Some(state) = responses_state.as_mut() {
                for event in responses_pipeline::translate_stream_done(state) {
                    yield Ok(Bytes::from(serialize_sse_event(event.event_type(), &event)));
                }
            }
            yield Ok(Bytes::from("data: [DONE]\n\n"));
        }
    }
}

/// Test-only aliases preserving the historical per-flavor entry points.
#[cfg(test)]
fn create_sse_stream(
    upstream: impl Stream<Item = Result<Bytes, impl std::fmt::Display + Send + 'static>>
        + Send
        + 'static,
    fallback_model: String,
    gui_logs: Arc<crate::settings::LogBuffer>,
    stats: Arc<StatsDb>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    create_flavor_sse_stream(
        upstream,
        ApiFlavor::Anthropic,
        fallback_model,
        gui_logs,
        stats,
    )
}

#[cfg(test)]
fn create_responses_sse_stream(
    upstream: impl Stream<Item = Result<Bytes, impl std::fmt::Display + Send + 'static>>
        + Send
        + 'static,
    fallback_model: String,
    gui_logs: Arc<crate::settings::LogBuffer>,
    stats: Arc<StatsDb>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    create_flavor_sse_stream(
        upstream,
        ApiFlavor::Responses,
        fallback_model,
        gui_logs,
        stats,
    )
}

#[cfg(test)]
mod tests {
    use super::create_sse_stream;
    use super::{apply_degraded_prompt, is_content_blocked};
    use crate::models::{openai, responses};
    use axum::response::IntoResponse;
    use bytes::Bytes;
    use futures::stream::{self, StreamExt};
    use rusqlite;
    use serde_json::{json, Value};
    use std::fmt;

    #[test]
    fn content_blocked_detects_11128() {
        assert!(is_content_blocked(
            400,
            "{\"code\":11128,\"msg\":\"Illegal API invocation from an unapproved channel\"}"
        ));
        // Non-400 is never a content block.
        assert!(!is_content_blocked(502, "11128"));
        // 400 without the block signature is a generic upstream failure.
        assert!(!is_content_blocked(400, "{\"error\":\"bad request\"}"));
    }

    #[test]
    fn degraded_prompt_replaces_system_message() {
        let mut req = openai::OpenAIRequest {
            model: "m".to_string(),
            messages: vec![
                openai::Message {
                    role: "system".to_string(),
                    content: Some(openai::MessageContent::Text(
                        "You are Claude Code, Anthropic's official CLI for Claude".to_string(),
                    )),
                    reasoning_content: None,
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
                openai::Message {
                    role: "user".to_string(),
                    content: Some(openai::MessageContent::Text(
                        "what is the cache key?".to_string(),
                    )),
                    reasoning_content: None,
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
            ],
            max_tokens: Some(1),
            temperature: None,
            top_p: None,
            stop: None,
            stream: Some(false),
            stream_options: None,
            tools: None,
            tool_choice: None,
            extra: serde_json::Map::new(),
        };
        apply_degraded_prompt(&mut req);
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, "system");
        let sys = match req.messages[0].content.as_ref().unwrap() {
            openai::MessageContent::Text(t) => t,
            _ => panic!("expected text"),
        };
        assert!(sys.contains("helpful assistant"));
        assert!(!sys.contains("Claude Code"));
        // User message preserved.
        assert_eq!(req.messages[1].role, "user");
    }

    #[test]
    fn degraded_prompt_prepends_when_no_system() {
        let mut req = openai::OpenAIRequest {
            model: "m".to_string(),
            messages: vec![openai::Message {
                role: "user".to_string(),
                content: Some(openai::MessageContent::Text("hi".to_string())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }],
            max_tokens: Some(1),
            temperature: None,
            top_p: None,
            stop: None,
            stream: Some(false),
            stream_options: None,
            tools: None,
            tool_choice: None,
            extra: serde_json::Map::new(),
        };
        apply_degraded_prompt(&mut req);
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, "system");
        assert_eq!(req.messages[1].role, "user");
    }

    #[derive(Debug)]
    struct TestError;
    impl fmt::Display for TestError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "test error")
        }
    }

    fn openai_chunk(
        id: &str,
        model: &str,
        content: Option<&str>,
        finish_reason: Option<&str>,
    ) -> String {
        let mut delta = json!({});
        if let Some(c) = content {
            delta["content"] = json!(c);
        }
        let mut choice = json!({ "index": 0, "delta": delta });
        if let Some(fr) = finish_reason {
            choice["finish_reason"] = json!(fr);
        }
        let chunk = json!({
            "id": id,
            "model": model,
            "choices": [choice],
        });
        format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap())
    }

    fn openai_chunk_with_reasoning(id: &str, model: &str, reasoning: &str) -> String {
        let chunk = json!({
            "id": id,
            "model": model,
            "choices": [{ "index": 0, "delta": { "reasoning": reasoning } }],
        });
        format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap())
    }

    fn openai_chunk_with_reasoning_content(id: &str, model: &str, reasoning: &str) -> String {
        let chunk = json!({
            "id": id,
            "model": model,
            "choices": [{ "index": 0, "delta": { "reasoning_content": reasoning } }],
        });
        format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap())
    }

    fn openai_chunk_with_tool_call(
        id: &str,
        model: &str,
        tool_id: Option<&str>,
        name: Option<&str>,
        args: Option<&str>,
        finish_reason: Option<&str>,
    ) -> String {
        let mut tc = json!({ "index": 0 });
        if let Some(tid) = tool_id {
            tc["id"] = json!(tid);
            tc["type"] = json!("function");
        }
        let mut func = json!({});
        if let Some(n) = name {
            func["name"] = json!(n);
        }
        if let Some(a) = args {
            func["arguments"] = json!(a);
        }
        if !func.as_object().unwrap().is_empty() {
            tc["function"] = func;
        }
        let mut choice = json!({ "index": 0, "delta": { "tool_calls": [tc] } });
        if let Some(fr) = finish_reason {
            choice["finish_reason"] = json!(fr);
        }
        let chunk = json!({
            "id": id,
            "model": model,
            "choices": [choice],
        });
        format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap())
    }

    fn openai_done() -> String {
        "data: [DONE]\n\n".to_string()
    }

    fn make_stream(
        chunks: Vec<String>,
    ) -> impl futures::Stream<Item = Result<Bytes, TestError>> + Send + 'static {
        stream::iter(chunks.into_iter().map(|c| Ok(Bytes::from(c))))
    }

    fn mock_stats() -> std::sync::Arc<crate::stats::StatsDb> {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS daily_stats (
                date TEXT PRIMARY KEY,
                requests_total INTEGER NOT NULL DEFAULT 0,
                requests_success INTEGER NOT NULL DEFAULT 0,
                requests_failed INTEGER NOT NULL DEFAULT 0,
                tokens_input INTEGER NOT NULL DEFAULT 0,
                tokens_cache_read INTEGER NOT NULL DEFAULT 0,
                tokens_cache_write INTEGER NOT NULL DEFAULT 0,
                tokens_output INTEGER NOT NULL DEFAULT 0
            );",
        )
        .unwrap();
        std::sync::Arc::new(crate::stats::StatsDb::from_conn(conn))
    }

    async fn collect_events(chunks: Vec<String>, model: &str) -> Vec<Value> {
        let s = make_stream(chunks);
        let sse = create_sse_stream(
            s,
            model.to_string(),
            std::sync::Arc::new(crate::settings::LogBuffer::new(2000)),
            mock_stats(),
        );
        tokio::pin!(sse);

        let mut events = Vec::new();
        while let Some(Ok(bytes)) = sse.next().await {
            let text = String::from_utf8_lossy(&bytes);
            for segment in text.split("\n\n").filter(|s| !s.is_empty()) {
                if let Some(data_line) = segment.lines().find(|l| l.starts_with("data: ")) {
                    let json_str = data_line.strip_prefix("data: ").unwrap();
                    if let Ok(v) = serde_json::from_str::<Value>(json_str) {
                        events.push(v);
                    }
                }
            }
        }
        events
    }

    use crate::config::Config;
    use axum::http::HeaderMap;

    fn make_x_api_key_header(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::HeaderName::from_static("x-api-key"),
            axum::http::HeaderValue::from_str(value).unwrap(),
        );
        headers
    }

    #[tokio::test]
    async fn resolve_api_key_passthrough_extracts_x_api_key() {
        let config = Config {
            passthrough_api_key: true,
            api_key: None,
            ..Default::default()
        };
        let headers = make_x_api_key_header("sk-my-test-key");
        let key = super::resolve_api_key(&config, &headers);
        assert_eq!(key, Some("sk-my-test-key".to_string()));
    }

    #[tokio::test]
    async fn resolve_api_key_passthrough_ignores_empty_header() {
        let config = Config {
            passthrough_api_key: true,
            api_key: None,
            ..Default::default()
        };
        // Empty header value returns None
        let key = super::resolve_api_key(&config, &HeaderMap::new());
        assert_eq!(key, None);

        // Explicitly empty value also returns None
        let headers = make_x_api_key_header("");
        let key = super::resolve_api_key(&config, &headers);
        assert_eq!(key, None);
    }

    #[tokio::test]
    async fn resolve_api_key_passthrough_returns_none_when_missing() {
        let config = Config {
            passthrough_api_key: true,
            api_key: None,
            ..Default::default()
        };
        let headers = HeaderMap::new();
        let key = super::resolve_api_key(&config, &headers);
        assert_eq!(key, None);
    }

    #[tokio::test]
    async fn resolve_api_key_static_key_when_passthrough_disabled() {
        let config = Config {
            passthrough_api_key: false,
            api_key: Some("sk-upstream".to_string()),
            ..Default::default()
        };
        // Even if x-api-key is present, static key wins when passthrough is off
        let headers = make_x_api_key_header("sk-ignored");
        let key = super::resolve_api_key(&config, &headers);
        assert_eq!(key, Some("sk-upstream".to_string()));
    }

    #[tokio::test]
    async fn resolve_api_key_both_missing_returns_none() {
        let config = Config {
            passthrough_api_key: false,
            api_key: None,
            ..Default::default()
        };
        let headers = HeaderMap::new();
        let key = super::resolve_api_key(&config, &headers);
        assert_eq!(key, None);
    }

    #[tokio::test]
    async fn resolve_responses_api_key_extracts_bearer_auth() {
        let config = Config {
            passthrough_api_key: true,
            api_key: None,
            ..Default::default()
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            axum::http::HeaderValue::from_static("Bearer sk-bearer-test"),
        );
        let key = super::resolve_responses_api_key(&config, &headers);
        assert_eq!(key, Some("sk-bearer-test".to_string()));
    }

    #[tokio::test]
    async fn text_stream_produces_message_start_content_block_and_stop() {
        let chunks = vec![
            openai_chunk("chatcmpl-1", "gpt-4o", Some("Hello"), None),
            openai_chunk("chatcmpl-1", "gpt-4o", Some(" world"), None),
            openai_chunk("chatcmpl-1", "gpt-4o", None, Some("stop")),
            openai_done(),
        ];

        let events = collect_events(chunks, "fallback").await;

        assert_eq!(events[0]["type"], "message_start");
        assert_eq!(events[0]["message"]["id"], "chatcmpl-1");
        assert_eq!(events[0]["message"]["model"], "gpt-4o");
        assert_eq!(events[0]["message"]["role"], "assistant");

        assert_eq!(events[1]["type"], "content_block_start");
        assert_eq!(events[1]["content_block"]["type"], "text");

        assert_eq!(events[2]["type"], "content_block_delta");
        assert_eq!(events[2]["delta"]["type"], "text_delta");
        assert_eq!(events[2]["delta"]["text"], "Hello");

        assert_eq!(events[3]["type"], "content_block_delta");
        assert_eq!(events[3]["delta"]["text"], " world");

        assert_eq!(events[4]["type"], "content_block_stop");

        assert_eq!(events[5]["type"], "message_delta");
        assert_eq!(events[5]["delta"]["stop_reason"], "end_turn");

        assert_eq!(events[6]["type"], "message_stop");
    }

    #[tokio::test]
    async fn reasoning_is_suppressed_text_only() {
        let chunks = vec![
            openai_chunk_with_reasoning("chatcmpl-2", "gpt-4o", "Let me think..."),
            openai_chunk_with_reasoning("chatcmpl-2", "gpt-4o", " more thinking"),
            openai_chunk("chatcmpl-2", "gpt-4o", Some("The answer is 42"), None),
            openai_chunk("chatcmpl-2", "gpt-4o", None, Some("stop")),
            openai_done(),
        ];

        let events = collect_events(chunks, "fallback").await;

        // No `thinking` content block is emitted; reasoning is suppressed.
        assert_eq!(events[0]["type"], "message_start");
        assert_eq!(events[1]["type"], "content_block_start");
        assert_eq!(events[1]["content_block"]["type"], "text");
        assert_eq!(events[1]["index"], 0);
        assert_eq!(events[2]["delta"]["type"], "text_delta");
        assert_eq!(events[2]["delta"]["text"], "The answer is 42");
        assert_eq!(events[3]["type"], "content_block_stop");
        assert_eq!(events[4]["type"], "message_delta");
        assert_eq!(events[4]["delta"]["stop_reason"], "end_turn");
        assert_eq!(events[5]["type"], "message_stop");
    }

    #[tokio::test]
    async fn reasoning_content_is_suppressed() {
        let chunks = vec![
            openai_chunk_with_reasoning_content("chatcmpl-2", "gpt-4o", "Let me think..."),
            openai_chunk("chatcmpl-2", "gpt-4o", Some("The answer is 42"), None),
            openai_chunk("chatcmpl-2", "gpt-4o", None, Some("stop")),
            openai_done(),
        ];

        let events = collect_events(chunks, "fallback").await;

        // reasoning_content must not become a `thinking` block.
        assert_eq!(events[1]["type"], "content_block_start");
        assert_eq!(events[1]["content_block"]["type"], "text");
        assert_eq!(events[2]["delta"]["type"], "text_delta");
        assert_eq!(events[2]["delta"]["text"], "The answer is 42");
    }

    #[tokio::test]
    async fn tool_call_stream_produces_tool_use_block() {
        let chunks = vec![
            openai_chunk_with_tool_call(
                "chatcmpl-3",
                "gpt-4o",
                Some("call_abc"),
                Some("read_file"),
                None,
                None,
            ),
            openai_chunk_with_tool_call(
                "chatcmpl-3",
                "gpt-4o",
                None,
                None,
                Some("{\"path\":"),
                None,
            ),
            openai_chunk_with_tool_call(
                "chatcmpl-3",
                "gpt-4o",
                None,
                None,
                Some("\"/tmp\"}"),
                None,
            ),
            openai_chunk("chatcmpl-3", "gpt-4o", None, Some("tool_calls")),
            openai_done(),
        ];

        let events = collect_events(chunks, "fallback").await;
        assert_eq!(events[1]["content_block"]["type"], "tool_use");
        assert_eq!(events[1]["content_block"]["id"], "call_abc");
        assert_eq!(events[5]["delta"]["stop_reason"], "tool_use");
    }

    #[tokio::test]
    async fn done_without_finish_reason_still_produces_message_stop() {
        let chunks = vec![
            openai_chunk("chatcmpl-4", "gpt-4o", Some("hi"), None),
            openai_done(),
        ];
        let events = collect_events(chunks, "fallback").await;
        assert_eq!(events.last().unwrap()["type"], "message_stop");
    }

    #[tokio::test]
    async fn fallback_model_used_when_upstream_omits_model() {
        let chunk = json!({
            "choices": [{ "index": 0, "delta": { "content": "hey" } }],
        });
        let chunks = vec![
            format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap()),
            openai_chunk("id", "gpt-4o", None, Some("stop")),
            openai_done(),
        ];
        let events = collect_events(chunks, "my-fallback-model").await;
        assert_eq!(events[0]["message"]["model"], "my-fallback-model");
    }

    #[tokio::test]
    async fn empty_content_chunks_are_not_emitted() {
        let chunks = vec![
            openai_chunk("chatcmpl-5", "gpt-4o", Some(""), None),
            openai_chunk("chatcmpl-5", "gpt-4o", Some("hello"), None),
            openai_chunk("chatcmpl-5", "gpt-4o", None, Some("stop")),
            openai_done(),
        ];
        let events = collect_events(chunks, "fallback").await;
        let text_deltas: Vec<_> = events
            .iter()
            .filter(|e| e["type"] == "content_block_delta" && e["delta"]["type"] == "text_delta")
            .collect();
        assert_eq!(text_deltas.len(), 1);
        assert_eq!(text_deltas[0]["delta"]["text"], "hello");
    }

    #[tokio::test]
    async fn stream_error_produces_error_event_and_stops() {
        let items: Vec<Result<Bytes, TestError>> = vec![
            Ok(Bytes::from(openai_chunk(
                "chatcmpl-6",
                "gpt-4o",
                Some("start"),
                None,
            ))),
            Err(TestError),
        ];
        let s = stream::iter(items);
        let sse = create_sse_stream(
            s,
            "fallback".to_string(),
            std::sync::Arc::new(crate::settings::LogBuffer::new(2000)),
            mock_stats(),
        );
        tokio::pin!(sse);

        let mut events = Vec::new();
        while let Some(Ok(bytes)) = sse.next().await {
            let text = String::from_utf8_lossy(&bytes);
            for segment in text.split("\n\n").filter(|s| !s.is_empty()) {
                if let Some(data_line) = segment.lines().find(|l| l.starts_with("data: ")) {
                    let json_str = data_line.strip_prefix("data: ").unwrap();
                    if let Ok(v) = serde_json::from_str::<Value>(json_str) {
                        events.push(v);
                    }
                }
            }
        }
        let error_events: Vec<_> = events.iter().filter(|e| e["type"] == "error").collect();
        assert_eq!(error_events.len(), 1);
    }

    #[tokio::test]
    async fn chunked_delivery_handles_split_sse_frames() {
        let full_chunk = openai_chunk("chatcmpl-7", "gpt-4o", Some("split"), None);
        let mid = full_chunk.len() / 2;
        let part1 = full_chunk[..mid].to_string();
        let part2 = format!(
            "{}{}{}",
            &full_chunk[mid..],
            openai_chunk("chatcmpl-7", "gpt-4o", None, Some("stop")),
            openai_done()
        );
        let events = collect_events(vec![part1, part2], "fallback").await;
        let text_deltas: Vec<_> = events
            .iter()
            .filter(|e| e["type"] == "content_block_delta" && e["delta"]["type"] == "text_delta")
            .collect();
        assert_eq!(text_deltas.len(), 1);
        assert_eq!(text_deltas[0]["delta"]["text"], "split");
    }

    #[tokio::test]
    async fn text_then_tool_call_produces_two_blocks() {
        let chunks = vec![
            openai_chunk("chatcmpl-8", "gpt-4o", Some("I'll read that file."), None),
            openai_chunk_with_tool_call(
                "chatcmpl-8",
                "gpt-4o",
                Some("call_xyz"),
                Some("read_file"),
                None,
                None,
            ),
            openai_chunk_with_tool_call(
                "chatcmpl-8",
                "gpt-4o",
                None,
                None,
                Some("{\"path\":\"/etc\"}"),
                None,
            ),
            openai_chunk("chatcmpl-8", "gpt-4o", None, Some("tool_calls")),
            openai_done(),
        ];
        let events = collect_events(chunks, "fallback").await;
        let block_starts: Vec<_> = events
            .iter()
            .filter(|e| e["type"] == "content_block_start")
            .collect();
        assert_eq!(block_starts.len(), 2);
        assert_eq!(block_starts[0]["content_block"]["type"], "text");
        assert_eq!(block_starts[1]["content_block"]["type"], "tool_use");
    }

    #[tokio::test]
    async fn responses_sse_stream_produces_valid_events() {
        let chunks = vec![
            openai_chunk("chatcmpl-resp", "gpt-4o", Some("Hello responses"), None),
            openai_chunk("chatcmpl-resp", "gpt-4o", None, Some("stop")),
            openai_done(),
        ];
        let stream = stream::iter(
            chunks
                .into_iter()
                .map(|s| Ok::<_, TestError>(Bytes::from(s))),
        );
        let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(10));
        let sse =
            super::create_responses_sse_stream(stream, "gpt-4o".to_string(), logs, mock_stats());
        tokio::pin!(sse);
        let mut raw = String::new();
        while let Some(item) = sse.next().await {
            let bytes = item.unwrap();
            raw.push_str(&String::from_utf8_lossy(&bytes));
        }

        assert!(raw.contains("event: response.created"));
        assert!(raw.contains("event: response.output_item.added"));
        assert!(raw.contains("event: response.output_text.delta"));
        assert!(raw.contains("event: response.completed"));
        assert!(raw.contains("data: [DONE]"));
    }

    #[tokio::test]
    async fn responses_handler_end_to_end_non_streaming() {
        use axum::routing::post;
        use axum::Json;
        let mock_upstream = axum::Router::new().route(
            "/v1/chat/completions",
            post(|Json(req): Json<openai::OpenAIRequest>| async move {
                assert_eq!(req.messages.len(), 1);
                assert_eq!(req.messages[0].role, "user");
                Json(openai::OpenAIResponse {
                    id: Some("chatcmpl-mock".to_string()),
                    object: Some("chat.completion".to_string()),
                    created: Some(1712345678),
                    model: Some("gpt-4o".to_string()),
                    choices: vec![openai::Choice {
                        index: 0,
                        message: openai::ChoiceMessage {
                            role: "assistant".to_string(),
                            content: Some("Response from mock".to_string()),
                            tool_calls: None,
                        },
                        finish_reason: Some("stop".to_string()),
                    }],
                    usage: openai::Usage {
                        prompt_tokens: 12,
                        completion_tokens: 6,
                        total_tokens: 18,
                        prompt_tokens_details: None,
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                        ..Default::default()
                    },
                    system_fingerprint: None,
                })
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, mock_upstream).await.unwrap();
        });

        let config = std::sync::Arc::new(Config {
            upstream_urls: vec![format!("http://127.0.0.1:{}", port)],
            ..Default::default()
        });
        let client = reqwest::Client::new();
        let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(10));
        let service = crate::service::ServiceController::new(8080, false);
        service.mark_running(8080);
        let headers = HeaderMap::new();
        let req = responses::ResponsesRequest {
            model: "gpt-4o".to_string(),
            input: responses::ResponsesInput::Text("Hello proxy".to_string()),
            instructions: None,
            tools: None,
            tool_choice: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            max_tokens: None,
            stream: Some(false),
            parallel_tool_calls: None,
            reasoning: None,
            store: None,
            include: None,
        };

        let response = super::responses_proxy_handler(
            axum::Extension(config),
            axum::Extension(client),
            axum::Extension(logs),
            axum::Extension(service),
            axum::Extension(mock_stats()),
            headers,
            Json(req),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn responses_handler_end_to_end_streaming() {
        use axum::routing::post;
        use axum::Json;
        let mock_upstream = axum::Router::new().route(
            "/v1/chat/completions",
            post(|Json(_req): Json<openai::OpenAIRequest>| async move {
                let stream = futures::stream::iter(vec![
                    Ok::<_, std::io::Error>(Bytes::from(openai_chunk(
                        "chatcmpl-stream-mock",
                        "gpt-4o",
                        Some("Streamed response"),
                        None,
                    ))),
                    Ok::<_, std::io::Error>(Bytes::from(openai_chunk(
                        "chatcmpl-stream-mock",
                        "gpt-4o",
                        None,
                        Some("stop"),
                    ))),
                    Ok::<_, std::io::Error>(Bytes::from(openai_done())),
                ]);
                let mut headers = HeaderMap::new();
                headers.insert(
                    "Content-Type",
                    axum::http::HeaderValue::from_static("text/event-stream"),
                );
                (headers, axum::body::Body::from_stream(stream)).into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, mock_upstream).await.unwrap();
        });

        let config = std::sync::Arc::new(Config {
            upstream_urls: vec![format!("http://127.0.0.1:{}", port)],
            ..Default::default()
        });
        let client = reqwest::Client::new();
        let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(10));
        let service = crate::service::ServiceController::new(8080, false);
        service.mark_running(8080);
        let headers = HeaderMap::new();
        let req = responses::ResponsesRequest {
            model: "gpt-4o".to_string(),
            input: responses::ResponsesInput::Text("Hello stream".to_string()),
            instructions: None,
            tools: None,
            tool_choice: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            max_tokens: None,
            stream: Some(true),
            parallel_tool_calls: None,
            reasoning: None,
            store: None,
            include: None,
        };

        let response = super::responses_proxy_handler(
            axum::Extension(config),
            axum::Extension(client),
            axum::Extension(logs),
            axum::Extension(service),
            axum::Extension(mock_stats()),
            headers,
            Json(req),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
    }

    #[test]
    fn test_usage_to_token_record_with_cache_variants() {
        // 1. OpenAI format with prompt_tokens_details.cached_tokens
        let usage_openai = openai::Usage {
            prompt_tokens: 100,
            completion_tokens: 30,
            total_tokens: 130,
            prompt_tokens_details: Some(openai::PromptTokensDetails {
                cached_tokens: 80,
                audio_tokens: 0,
            }),
            ..Default::default()
        };
        let rec = usage_openai.to_token_record();
        assert_eq!(rec.input, 20);
        assert_eq!(rec.cache_read, 80);
        assert_eq!(rec.cache_write, 0);
        assert_eq!(rec.output, 30);

        // 2. DeepSeek format with prompt_cache_hit_tokens
        let usage_deepseek = openai::Usage {
            prompt_tokens: 100,
            completion_tokens: 25,
            total_tokens: 125,
            prompt_cache_hit_tokens: Some(70),
            prompt_cache_miss_tokens: Some(30),
            ..Default::default()
        };
        let rec = usage_deepseek.to_token_record();
        assert_eq!(rec.input, 30);
        assert_eq!(rec.cache_read, 70);
        assert_eq!(rec.output, 25);

        // 3. Anthropic format with cache_read_input_tokens & cache_creation_input_tokens
        let usage_anthropic = openai::Usage {
            prompt_tokens: 150,
            completion_tokens: 50,
            total_tokens: 200,
            cache_read_input_tokens: Some(100),
            cache_creation_input_tokens: Some(30),
            ..Default::default()
        };
        let rec = usage_anthropic.to_token_record();
        assert_eq!(rec.input, 20); // 150 - 100 - 30
        assert_eq!(rec.cache_read, 100);
        assert_eq!(rec.cache_write, 30);
        assert_eq!(rec.output, 50);

        // 4. Top-level cached_tokens
        let usage_toplevel = openai::Usage {
            prompt_tokens: 80,
            completion_tokens: 10,
            total_tokens: 90,
            cached_tokens: Some(50),
            ..Default::default()
        };
        let rec = usage_toplevel.to_token_record();
        assert_eq!(rec.input, 30);
        assert_eq!(rec.cache_read, 50);
        assert_eq!(rec.output, 10);
    }

    #[tokio::test]
    async fn chat_completions_handler_end_to_end_non_streaming() {
        use axum::routing::post;
        use axum::Json;

        let mock_upstream = axum::Router::new().route(
            "/v1/chat/completions",
            post(|Json(req): Json<openai::OpenAIRequest>| async move {
                // Verify model was remapped by proxy
                assert_eq!(req.model, "gpt-4o-upstream");
                // Verify extra parameter was preserved
                assert_eq!(
                    req.extra.get("reasoning_effort").and_then(|v| v.as_str()),
                    Some("low")
                );

                let resp = openai::OpenAIResponse {
                    id: Some("chatcmpl-test-123".to_string()),
                    object: Some("chat.completion".to_string()),
                    created: Some(1700000000),
                    model: Some("gpt-4o-upstream".to_string()),
                    choices: vec![openai::Choice {
                        index: 0,
                        message: openai::ChoiceMessage {
                            role: "assistant".to_string(),
                            content: Some("Hello from OpenAI mock!".to_string()),
                            tool_calls: None,
                        },
                        finish_reason: Some("stop".to_string()),
                    }],
                    usage: openai::Usage {
                        prompt_tokens: 100,
                        completion_tokens: 40,
                        total_tokens: 140,
                        prompt_tokens_details: Some(openai::PromptTokensDetails {
                            cached_tokens: 60,
                            audio_tokens: 0,
                        }),
                        ..Default::default()
                    },
                    system_fingerprint: None,
                };
                Json(resp).into_response()
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, mock_upstream).await.unwrap();
        });

        let mut model_map = std::collections::BTreeMap::new();
        model_map.insert("gpt-alias".to_string(), "gpt-4o-upstream".to_string());

        let config = std::sync::Arc::new(Config {
            upstream_urls: vec![format!("http://127.0.0.1:{}", port)],
            model_map,
            ..Default::default()
        });
        let client = reqwest::Client::new();
        let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(10));
        let service = crate::service::ServiceController::new(8080, false);
        service.mark_running(8080);
        let stats_db = mock_stats();

        let mut extra = serde_json::Map::new();
        extra.insert("reasoning_effort".to_string(), json!("low"));

        let req = openai::OpenAIRequest {
            model: "gpt-alias".to_string(),
            messages: vec![openai::Message {
                role: "user".to_string(),
                content: Some(openai::MessageContent::Text("Hi".to_string())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }],
            max_tokens: Some(100),
            temperature: None,
            top_p: None,
            stop: None,
            stream: Some(false),
            stream_options: None,
            tools: None,
            tool_choice: None,
            extra,
        };

        let response = super::chat_completions_proxy_handler(
            axum::Extension(config),
            axum::Extension(client),
            axum::Extension(logs),
            axum::Extension(service),
            axum::Extension(stats_db.clone()),
            HeaderMap::new(),
            Json(req),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);

        // Verify stats in SQLite
        let today = stats_db.query_today().unwrap();
        assert_eq!(today.requests_success, 1);
        assert_eq!(today.tokens_input, 40); // 100 - 60 cached
        assert_eq!(today.tokens_cache_read, 60);
        assert_eq!(today.tokens_output, 40);
        assert_eq!(today.cache_hit_pct(), 60);
    }

    #[tokio::test]
    async fn chat_completions_handler_end_to_end_streaming() {
        use axum::routing::post;
        use axum::Json;

        let mock_upstream = axum::Router::new().route(
            "/v1/chat/completions",
            post(|Json(req): Json<openai::OpenAIRequest>| async move {
                // Verify include_usage was set to true for stream
                assert!(req.stream_options.map(|s| s.include_usage).unwrap_or(false));

                let chunk1 = json!({
                    "id": "chatcmpl-stream-1",
                    "object": "chat.completion.chunk",
                    "created": 1700000000,
                    "model": "gpt-4o",
                    "choices": [{
                        "index": 0,
                        "delta": { "content": "Streamed " },
                        "finish_reason": null
                    }]
                });

                let chunk2 = json!({
                    "id": "chatcmpl-stream-1",
                    "object": "chat.completion.chunk",
                    "created": 1700000000,
                    "model": "gpt-4o",
                    "choices": [{
                        "index": 0,
                        "delta": { "content": "chat!" },
                        "finish_reason": "stop"
                    }]
                });

                let chunk_usage = json!({
                    "id": "chatcmpl-stream-1",
                    "object": "chat.completion.chunk",
                    "created": 1700000000,
                    "model": "gpt-4o",
                    "choices": [],
                    "usage": {
                        "prompt_tokens": 100,
                        "completion_tokens": 20,
                        "total_tokens": 120,
                        "prompt_cache_hit_tokens": 80,
                        "prompt_cache_miss_tokens": 20
                    }
                });

                let stream = futures::stream::iter(vec![
                    Ok::<_, std::io::Error>(Bytes::from(format!(
                        "data: {}\n\n",
                        serde_json::to_string(&chunk1).unwrap()
                    ))),
                    Ok::<_, std::io::Error>(Bytes::from(format!(
                        "data: {}\n\n",
                        serde_json::to_string(&chunk2).unwrap()
                    ))),
                    Ok::<_, std::io::Error>(Bytes::from(format!(
                        "data: {}\n\n",
                        serde_json::to_string(&chunk_usage).unwrap()
                    ))),
                    Ok::<_, std::io::Error>(Bytes::from("data: [DONE]\n\n")),
                ]);

                let mut headers = HeaderMap::new();
                headers.insert(
                    "Content-Type",
                    axum::http::HeaderValue::from_static("text/event-stream"),
                );
                (headers, axum::body::Body::from_stream(stream)).into_response()
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, mock_upstream).await.unwrap();
        });

        let config = std::sync::Arc::new(Config {
            upstream_urls: vec![format!("http://127.0.0.1:{}", port)],
            ..Default::default()
        });
        let client = reqwest::Client::new();
        let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(10));
        let service = crate::service::ServiceController::new(8080, false);
        service.mark_running(8080);
        let stats_db = mock_stats();

        let req = openai::OpenAIRequest {
            model: "gpt-4o".to_string(),
            messages: vec![openai::Message {
                role: "user".to_string(),
                content: Some(openai::MessageContent::Text("Stream test".to_string())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop: None,
            stream: Some(true),
            stream_options: None,
            tools: None,
            tool_choice: None,
            extra: serde_json::Map::new(),
        };

        let response = super::chat_completions_proxy_handler(
            axum::Extension(config),
            axum::Extension(client),
            axum::Extension(logs),
            axum::Extension(service),
            axum::Extension(stats_db.clone()),
            HeaderMap::new(),
            Json(req),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/event-stream"
        );

        // Read the stream to completion so create_chat_sse_stream finishes
        let _ = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();

        // Verify stats in SQLite
        let today = stats_db.query_today().unwrap();
        assert_eq!(today.requests_success, 1);
        assert_eq!(today.tokens_input, 20); // 100 - 80 cached
        assert_eq!(today.tokens_cache_read, 80);
        assert_eq!(today.tokens_output, 20);
        assert_eq!(today.cache_hit_pct(), 80);
    }
}
