//! Lightweight metrics collection with Prometheus text exposition and health endpoint.
//!
//! Histograms use logarithmic 1-2-5 buckets in the microsecond range,
//! matching the resolution needed for HFT latency profiling. No external
//! metrics crate — atomic counters rendered directly as Prometheus text format.

use std::fmt::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use tokio_util::sync::CancellationToken;
use tracing::info;

// ---------------------------------------------------------------------------
// Prometheus histogram with microsecond-resolution logarithmic buckets
// ---------------------------------------------------------------------------

const NUM_BUCKETS: usize = 12;

/// Upper bounds in nanoseconds + Prometheus `le` label strings.
/// 1-2-5 logarithmic progression from 1μs to 10ms.
const BUCKETS: [(u64, &str); NUM_BUCKETS] = [
    (1_000, "0.000001"),       // 1μs
    (2_000, "0.000002"),       // 2μs
    (5_000, "0.000005"),       // 5μs
    (10_000, "0.00001"),       // 10μs
    (25_000, "0.000025"),      // 25μs
    (50_000, "0.00005"),       // 50μs
    (100_000, "0.0001"),       // 100μs
    (250_000, "0.00025"),      // 250μs
    (500_000, "0.0005"),       // 500μs
    (1_000_000, "0.001"),      // 1ms
    (5_000_000, "0.005"),      // 5ms
    (10_000_000, "0.01"),      // 10ms
];

pub struct PromHistogram {
    /// Cumulative bucket counters. Index i counts observations <= BUCKETS[i].
    buckets: [AtomicU64; NUM_BUCKETS],
    /// Sum of all observed values in nanoseconds.
    sum_ns: AtomicU64,
    /// Total number of observations.
    count: AtomicU64,
}

impl PromHistogram {
    pub fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            sum_ns: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    /// Record a duration observation. Increments all cumulative buckets
    /// whose upper bound >= the observed value.
    pub fn record(&self, duration: Duration) {
        let nanos = duration.as_nanos() as u64;

        for (i, &(bound_ns, _)) in BUCKETS.iter().enumerate() {
            if nanos <= bound_ns {
                for bucket in &self.buckets[i..] {
                    bucket.fetch_add(1, Relaxed);
                }
                break;
            }
        }

        self.sum_ns.fetch_add(nanos, Relaxed);
        self.count.fetch_add(1, Relaxed);
    }

    /// Render as Prometheus histogram lines. `labels` is empty or e.g. `exchange="binance"`.
    fn render(&self, name: &str, labels: &str, out: &mut String) {
        for (i, &(_, le)) in BUCKETS.iter().enumerate() {
            let count = self.buckets[i].load(Relaxed);
            if labels.is_empty() {
                writeln!(out, "{name}_bucket{{le=\"{le}\"}} {count}").unwrap();
            } else {
                writeln!(out, "{name}_bucket{{{labels},le=\"{le}\"}} {count}").unwrap();
            }
        }

        let total = self.count.load(Relaxed);
        if labels.is_empty() {
            writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {total}").unwrap();
        } else {
            writeln!(out, "{name}_bucket{{{labels},le=\"+Inf\"}} {total}").unwrap();
        }

        let sum_secs = self.sum_ns.load(Relaxed) as f64 / 1_000_000_000.0;
        if labels.is_empty() {
            writeln!(out, "{name}_sum {sum_secs}").unwrap();
            writeln!(out, "{name}_count {total}").unwrap();
        } else {
            writeln!(out, "{name}_sum{{{labels}}} {sum_secs}").unwrap();
            writeln!(out, "{name}_count{{{labels}}} {total}").unwrap();
        }
    }
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

pub struct Metrics {
    // Counters
    pub binance_msgs: AtomicU64,
    pub bitstamp_msgs: AtomicU64,
    pub binance_errors: AtomicU64,
    pub bitstamp_errors: AtomicU64,
    pub merges: AtomicU64,

    // Gauges
    pub binance_connected: AtomicBool,
    pub bitstamp_connected: AtomicBool,
    start_time: Instant,

    // Latency histograms
    pub binance_decode: PromHistogram,
    pub bitstamp_decode: PromHistogram,
    pub merge_latency: PromHistogram,
    pub e2e_latency: PromHistogram,
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
            binance_decode: PromHistogram::new(),
            bitstamp_decode: PromHistogram::new(),
            merge_latency: PromHistogram::new(),
            e2e_latency: PromHistogram::new(),
        }
    }
}

impl Metrics {
    /// Render all metrics in Prometheus text exposition format.
    pub fn to_prometheus(&self) -> String {
        let mut out = String::with_capacity(4096);

        // -- Counters --
        writeln!(out, "# HELP orderbook_messages_total WebSocket messages received").unwrap();
        writeln!(out, "# TYPE orderbook_messages_total counter").unwrap();
        writeln!(out, "orderbook_messages_total{{exchange=\"binance\"}} {}", self.binance_msgs.load(Relaxed)).unwrap();
        writeln!(out, "orderbook_messages_total{{exchange=\"bitstamp\"}} {}", self.bitstamp_msgs.load(Relaxed)).unwrap();

        writeln!(out, "# HELP orderbook_errors_total Parse/connection errors").unwrap();
        writeln!(out, "# TYPE orderbook_errors_total counter").unwrap();
        writeln!(out, "orderbook_errors_total{{exchange=\"binance\"}} {}", self.binance_errors.load(Relaxed)).unwrap();
        writeln!(out, "orderbook_errors_total{{exchange=\"bitstamp\"}} {}", self.bitstamp_errors.load(Relaxed)).unwrap();

        writeln!(out, "# HELP orderbook_merges_total Order book merge operations").unwrap();
        writeln!(out, "# TYPE orderbook_merges_total counter").unwrap();
        writeln!(out, "orderbook_merges_total {}", self.merges.load(Relaxed)).unwrap();

        // -- Gauges --
        writeln!(out, "# HELP orderbook_exchange_up Exchange connection status (1=connected)").unwrap();
        writeln!(out, "# TYPE orderbook_exchange_up gauge").unwrap();
        writeln!(out, "orderbook_exchange_up{{exchange=\"binance\"}} {}", self.binance_connected.load(Relaxed) as u8).unwrap();
        writeln!(out, "orderbook_exchange_up{{exchange=\"bitstamp\"}} {}", self.bitstamp_connected.load(Relaxed) as u8).unwrap();

        writeln!(out, "# HELP orderbook_uptime_seconds Seconds since process start").unwrap();
        writeln!(out, "# TYPE orderbook_uptime_seconds gauge").unwrap();
        writeln!(out, "orderbook_uptime_seconds {}", self.start_time.elapsed().as_secs()).unwrap();

        // -- Histograms --
        writeln!(out, "# HELP orderbook_decode_duration_seconds WebSocket message decode latency").unwrap();
        writeln!(out, "# TYPE orderbook_decode_duration_seconds histogram").unwrap();
        self.binance_decode.render("orderbook_decode_duration_seconds", "exchange=\"binance\"", &mut out);
        self.bitstamp_decode.render("orderbook_decode_duration_seconds", "exchange=\"bitstamp\"", &mut out);

        writeln!(out, "# HELP orderbook_merge_duration_seconds Order book merge latency").unwrap();
        writeln!(out, "# TYPE orderbook_merge_duration_seconds histogram").unwrap();
        self.merge_latency.render("orderbook_merge_duration_seconds", "", &mut out);

        writeln!(out, "# HELP orderbook_e2e_duration_seconds End-to-end latency: WS receive to merged summary published").unwrap();
        writeln!(out, "# TYPE orderbook_e2e_duration_seconds histogram").unwrap();
        self.e2e_latency.render("orderbook_e2e_duration_seconds", "", &mut out);

        out
    }
}

// ---------------------------------------------------------------------------
// HTTP server
// ---------------------------------------------------------------------------

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
