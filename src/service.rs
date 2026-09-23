use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

/// Shared handle to the proxy service (the `/v1/*` listener).
///
/// The console is served on the *same* listener as the proxy API, so stopping
/// the service from the GUI must not take the console offline: the listener is
/// torn down and re-bound by the caller, while this handle tracks the state the
/// UI reads.
pub struct ServiceController {
    running: AtomicBool,
    port: AtomicU16,
    preferred_port: AtomicU16,
    /// Whether to retry `preferred + 1` when the preferred port is taken.
    ///
    /// Off in the desktop app: the client CLIs are configured against one fixed
    /// gateway URL, so silently binding a different port breaks them with no
    /// visible cause. A busy port is surfaced as an error instead, and the
    /// single-instance guard is what actually prevents the conflict (see
    /// `src-tauri/src/main.rs`).
    allow_port_fallback: AtomicBool,
}

impl ServiceController {
    pub fn new(preferred_port: u16, allow_port_fallback: bool) -> Arc<Self> {
        Arc::new(Self {
            running: AtomicBool::new(false),
            port: AtomicU16::new(0),
            preferred_port: AtomicU16::new(preferred_port),
            allow_port_fallback: AtomicBool::new(allow_port_fallback),
        })
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Port the service listens on, or 0 while stopped.
    pub fn port(&self) -> u16 {
        self.port.load(Ordering::SeqCst)
    }

    pub fn set_preferred_port(&self, port: u16) {
        self.preferred_port.store(port, Ordering::SeqCst);
    }

    pub fn allow_port_fallback(&self) -> bool {
        self.allow_port_fallback.load(Ordering::SeqCst)
    }

    /// Retry `preferred + 1` when the preferred port is busy. The desktop app
    /// leaves this off so the configured port is the only one ever used.
    pub fn set_allow_port_fallback(&self, allow: bool) {
        self.allow_port_fallback.store(allow, Ordering::SeqCst);
    }

    pub fn mark_running(&self, port: u16) {
        self.port.store(port, Ordering::SeqCst);
        self.running.store(true, Ordering::SeqCst);
    }

    /// Mark the service as stopped: clears the bound port and the running flag
    /// so the GUI reflects reality.
    pub fn mark_stopped(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.port.store(0, Ordering::SeqCst);
    }

    /// Bind the preferred port, falling back to `preferred + 1` when allowed.
    ///
    /// `allow_fallback` is passed in rather than read from the controller so
    /// the caller decides per bind; the desktop app passes `false`, which makes
    /// a busy port a hard error instead of a silent retarget.
    pub async fn bind(
        bind_addr: &str,
        preferred: u16,
        allow_fallback: bool,
    ) -> std::io::Result<(tokio::net::TcpListener, u16)> {
        match tokio::net::TcpListener::bind(format!("{bind_addr}:{preferred}")).await {
            Ok(l) => Ok((l, preferred)),
            Err(e) if allow_fallback => {
                let fallback = preferred.saturating_add(1);
                eprintln!("⚠️  Port {preferred} is busy ({e}), falling back to {fallback}");
                let l = tokio::net::TcpListener::bind(format!("{bind_addr}:{fallback}")).await?;
                Ok((l, fallback))
            }
            Err(e) => Err(e),
        }
    }
}

/// The JSON body returned while the service is stopped, so a client talking to
/// the shared listener gets a clear 503 rather than a connection reset.
pub fn service_unavailable_response() -> Response {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "error": {
                    "type": "service_unavailable",
                    "message": "Proxy service is stopped. Start it from the console to continue.",
                    "code": "service_unavailable"
                }
            })
            .to_string(),
        )
        .expect("static response builds")
        .into_response()
}
