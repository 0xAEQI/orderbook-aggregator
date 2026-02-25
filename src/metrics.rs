//! Lightweight metrics collection with Prometheus text exposition and health endpoint.
//!
//! No external metrics crate needed — atomic counters rendered directly
//! in Prometheus text format. Served via HTTP on a configurable port.

use std::fmt::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub struct Metrics {
    pub binance_msgs: AtomicU64,
    pub bitstamp_msgs: AtomicU64,
    pub binance_errors: AtomicU64,
    pub bitstamp_errors: AtomicU64,
    pub merges: AtomicU64,
    pub binance_connected: AtomicBool,
    pub bitstamp_connected: AtomicBool,
    start_time: Instant,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            binance_msgs: AtomicU64::new(0),
            bitstamp_msgs: AtomicU64::new(0),
            binance_errors: AtomicU64::new(0),
            bitstamp_errors: AtomicU64::new(0),
            merges: AtomicU64::new(0),
            binance_connected: AtomicBool::new(false),
            bitstamp_connected: AtomicBool::new(false),
            start_time: Instant::now(),
        }
    }
}

impl Metrics {
    /// Render all metrics in Prometheus text exposition format.
    pub fn to_prometheus(&self) -> String {
        let mut out = String::with_capacity(1024);

        writeln!(out, "# HELP orderbook_messages_total WebSocket messages received").unwrap();
        writeln!(out, "# TYPE orderbook_messages_total counter").unwrap();
        writeln!(
            out,
            "orderbook_messages_total{{exchange=\"binance\"}} {}",
            self.binance_msgs.load(Relaxed)
        )
        .unwrap();
        writeln!(
            out,
            "orderbook_messages_total{{exchange=\"bitstamp\"}} {}",
            self.bitstamp_msgs.load(Relaxed)
        )
        .unwrap();

        writeln!(
            out,
            "# HELP orderbook_errors_total Parse/connection errors"
        )
        .unwrap();
        writeln!(out, "# TYPE orderbook_errors_total counter").unwrap();
        writeln!(
            out,
            "orderbook_errors_total{{exchange=\"binance\"}} {}",
            self.binance_errors.load(Relaxed)
        )
        .unwrap();
        writeln!(
            out,
            "orderbook_errors_total{{exchange=\"bitstamp\"}} {}",
            self.bitstamp_errors.load(Relaxed)
        )
        .unwrap();

        writeln!(
            out,
            "# HELP orderbook_merges_total Order book merge operations"
        )
        .unwrap();
        writeln!(out, "# TYPE orderbook_merges_total counter").unwrap();
        writeln!(out, "orderbook_merges_total {}", self.merges.load(Relaxed)).unwrap();

        writeln!(
            out,
            "# HELP orderbook_exchange_up Exchange connection status (1=connected)"
        )
        .unwrap();
        writeln!(out, "# TYPE orderbook_exchange_up gauge").unwrap();
        writeln!(
            out,
            "orderbook_exchange_up{{exchange=\"binance\"}} {}",
            self.binance_connected.load(Relaxed) as u8
        )
        .unwrap();
        writeln!(
            out,
            "orderbook_exchange_up{{exchange=\"bitstamp\"}} {}",
            self.bitstamp_connected.load(Relaxed) as u8
        )
        .unwrap();

        writeln!(
            out,
            "# HELP orderbook_uptime_seconds Seconds since process start"
        )
        .unwrap();
        writeln!(out, "# TYPE orderbook_uptime_seconds gauge").unwrap();
        writeln!(
            out,
            "orderbook_uptime_seconds {}",
            self.start_time.elapsed().as_secs()
        )
        .unwrap();

        out
    }
}

/// Serve `/health` and `/metrics` on the given port.
pub async fn serve_http(port: u16, metrics: Arc<Metrics>, cancel: CancellationToken) {
    let app = Router::new()
        .route("/health", get(health))
        .route("/metrics", get(prom_metrics))
        .with_state(metrics);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("failed to bind metrics port");

    info!(port, "metrics/health HTTP server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await
        .ok();
}

async fn health(State(m): State<Arc<Metrics>>) -> (StatusCode, &'static str) {
    let b = m.binance_connected.load(Relaxed);
    let s = m.bitstamp_connected.load(Relaxed);
    match (b, s) {
        (true, true) => (StatusCode::OK, "OK\n"),
        (true, false) | (false, true) => (StatusCode::OK, "DEGRADED\n"),
        (false, false) => (StatusCode::SERVICE_UNAVAILABLE, "DOWN\n"),
    }
}

async fn prom_metrics(State(m): State<Arc<Metrics>>) -> String {
    m.to_prometheus()
}
