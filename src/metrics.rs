use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::PrometheusBuilder;
use std::sync::OnceLock;
use std::time::Instant;

/// Re-exported so callers can name the handle type without depending on the
/// third-party exporter crate directly.
pub use metrics_exporter_prometheus::PrometheusHandle;

/// The global Prometheus recorder can only be installed once per process.
/// Cache the handle so repeated calls (e.g. every proxy (re)start from the GUI)
/// return the same recorder instead of panicking.
static PROMETHEUS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

pub fn install() -> PrometheusHandle {
    PROMETHEUS_HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .install_recorder()
                .expect("failed to install Prometheus recorder")
        })
        .clone()
}

pub fn request_started(streaming: bool) {
    let mode = if streaming { "streaming" } else { "batch" };
    counter!("proxy_requests_total", "mode" => mode).increment(1);
    gauge!("proxy_requests_in_flight").increment(1.0);
}

pub fn request_finished(start: Instant, status: u16, streaming: bool) {
    let mode = if streaming { "streaming" } else { "batch" };
    let status_str = status.to_string();

    histogram!("proxy_request_duration_seconds", "mode" => mode, "status" => status_str.clone())
        .record(start.elapsed().as_secs_f64());
    counter!("proxy_responses_total", "mode" => mode, "status" => status_str).increment(1);
    gauge!("proxy_requests_in_flight").decrement(1.0);
}

pub fn upstream_latency(seconds: f64, endpoint: &'static str) {
    histogram!("proxy_upstream_latency_seconds", "endpoint" => endpoint).record(seconds);
}

pub fn tokens(input: u32, output: u32, model: &str) {
    let model = model.to_string();
    counter!("proxy_tokens_total", "type" => "input", "model" => model.clone())
        .increment(input as u64);
    counter!("proxy_tokens_total", "type" => "output", "model" => model).increment(output as u64);
}

pub fn upstream_error(endpoint: &'static str) {
    counter!("proxy_upstream_errors_total", "endpoint" => endpoint).increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The GUI re-installs the recorder on every proxy (re)start. This must be
    /// idempotent: previously a second call panicked, which silently aborted the
    /// server task so the port was never bound and nothing was logged.
    #[test]
    fn install_is_idempotent() {
        let a = install();
        let b = install();
        // Both calls must return a usable handle rather than panicking.
        let _ = a.render();
        let _ = b.render();
    }
}
