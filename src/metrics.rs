//! Lock-free metrics with Prometheus text exposition and health endpoint.
//!
//! All hot-path counters are bare atomics — no mutex, no allocation, no crate.
//! Each exchange adapter receives its own `Arc<ExchangeMetrics>` at startup;
//! adding a new exchange is a one-line change in `main.rs`, zero changes here.
//!
//! Histograms use 1-2-5 logarithmic buckets from 100ns to 100ms, covering
//! both sub-microsecond decode/merge operations and tail-latency spikes from
//! context switches, reconnection bursts, and channel backlog.

use std::fmt::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use tokio_util::sync::CancellationToken;
use tracing::info;

// ---------------------------------------------------------------------------
// Prometheus histogram with logarithmic buckets (100ns – 100ms)
// ---------------------------------------------------------------------------

const NUM_BUCKETS: usize = 16;

/// Upper bounds in nanoseconds + Prometheus `le` label strings.
/// 1-2-5 logarithmic progression from 100ns to 100ms — covers both
/// sub-microsecond decode/merge and tail-latency spikes on e2e.
const BUCKETS: [(u64, &str); NUM_BUCKETS] = [
    (100, "0.0000001"),           // 100ns
    (500, "0.0000005"),           // 500ns
    (1_000, "0.000001"),          // 1μs
    (5_000, "0.000005"),          // 5μs
    (10_000, "0.00001"),          // 10μs
    (25_000, "0.000025"),         // 25μs
    (50_000, "0.00005"),          // 50μs
    (100_000, "0.0001"),          // 100μs
    (250_000, "0.00025"),         // 250μs
    (500_000, "0.0005"),          // 500μs
    (1_000_000, "0.001"),         // 1ms
    (5_000_000, "0.005"),         // 5ms
    (10_000_000, "0.01"),         // 10ms
    (25_000_000, "0.025"),        // 25ms
    (50_000_000, "0.05"),         // 50ms
    (100_000_000, "0.1"),         // 100ms
];

pub struct PromHistogram {
    /// Per-bucket (non-cumulative) counters. Index i counts observations where
    /// BUCKETS[i-1] < value <= BUCKETS[i]. Last slot is the +Inf overflow bucket.
    /// This gives O(1) record (single `fetch_add`) vs O(k) for cumulative buckets.
    buckets: [AtomicU64; NUM_BUCKETS + 1],
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

    /// Record a duration observation — O(1): single atomic increment.
    ///
    /// Finds the matching bucket via linear scan of the 16-element boundary
    /// table (fits in L1, branch-predicted after warmup) and does one `fetch_add`.
    /// Cumulative sums are computed lazily on the cold `/metrics` render path.
    #[inline]
    pub fn record(&self, duration: Duration) {
        let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);

        let mut idx = NUM_BUCKETS; // overflow slot
        for (i, &(bound_ns, _)) in BUCKETS.iter().enumerate() {
            if nanos <= bound_ns {
                idx = i;
                break;
            }
        }

        self.buckets[idx].fetch_add(1, Relaxed);
        self.sum_ns.fetch_add(nanos, Relaxed);
        self.count.fetch_add(1, Relaxed);
    }

    /// Render as Prometheus histogram lines.
    ///
    /// Computes cumulative sums from per-bucket counts — O(k) work on the cold
    /// scrape path (~every 5-15s) instead of on every hot-path `record()`.
    fn render(&self, name: &str, labels: &str, out: &mut String) {
        let mut cumulative = 0u64;
        for (i, &(_, le)) in BUCKETS.iter().enumerate() {
            cumulative += self.buckets[i].load(Relaxed);
            if labels.is_empty() {
                writeln!(out, "{name}_bucket{{le=\"{le}\"}} {cumulative}").unwrap();
            } else {
                writeln!(out, "{name}_bucket{{{labels},le=\"{le}\"}} {cumulative}").unwrap();
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
// Per-exchange hot-path metrics handle
// ---------------------------------------------------------------------------

/// Atomic counters for a single exchange. Allocated once at startup, handed
/// directly to the adapter — no map lookup, no string matching on the hot path.
pub struct ExchangeMetrics {
    pub name: &'static str,
    pub messages: AtomicU64,
    pub errors: AtomicU64,
    pub connected: AtomicBool,
    pub decode_latency: PromHistogram,
}

impl ExchangeMetrics {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            messages: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            connected: AtomicBool::new(false),
            decode_latency: PromHistogram::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Global metrics registry
// ---------------------------------------------------------------------------

pub struct Metrics {
    /// Per-exchange handles, built once at startup, iterated only on /metrics scrape.
    exchanges: Vec<Arc<ExchangeMetrics>>,

    // Global counters
    pub merges: AtomicU64,

    // Global histograms
    pub merge_latency: PromHistogram,
    pub e2e_latency: PromHistogram,

    start_time: Instant,
}

impl Metrics {
    /// Create the metrics registry and per-exchange handles.
    ///
    /// Adding a new exchange = adding one name here. No struct changes needed.
    pub fn register(exchange_names: &[&'static str]) -> Self {
        let exchanges = exchange_names
            .iter()
            .map(|&name| Arc::new(ExchangeMetrics::new(name)))
            .collect();

        Self {
            exchanges,
            merges: AtomicU64::new(0),
            merge_latency: PromHistogram::new(),
            e2e_latency: PromHistogram::new(),
            start_time: Instant::now(),
        }
    }

    /// Get the per-exchange handle by name. Called once at startup, not on the hot path.
    pub fn exchange(&self, name: &str) -> Arc<ExchangeMetrics> {
        self.exchanges
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("unknown exchange: {name}"))
            .clone()
    }

    /// Render all metrics in Prometheus text exposition format.
    pub fn to_prometheus(&self) -> String {
        let mut out = String::with_capacity(4096);

        // -- Per-exchange counters (dynamic) --
        writeln!(
            out,
            "# HELP orderbook_messages_total WebSocket messages received"
        )
        .unwrap();
        writeln!(out, "# TYPE orderbook_messages_total counter").unwrap();
        for ex in &self.exchanges {
            writeln!(
                out,
                "orderbook_messages_total{{exchange=\"{}\"}} {}",
                ex.name,
                ex.messages.load(Relaxed)
            )
            .unwrap();
        }

        writeln!(out, "# HELP orderbook_errors_total Parse/connection errors").unwrap();
        writeln!(out, "# TYPE orderbook_errors_total counter").unwrap();
        for ex in &self.exchanges {
            writeln!(
                out,
                "orderbook_errors_total{{exchange=\"{}\"}} {}",
                ex.name,
                ex.errors.load(Relaxed)
            )
            .unwrap();
        }

        writeln!(
            out,
            "# HELP orderbook_merges_total Order book merge operations"
        )
        .unwrap();
        writeln!(out, "# TYPE orderbook_merges_total counter").unwrap();
        writeln!(out, "orderbook_merges_total {}", self.merges.load(Relaxed)).unwrap();

        // -- Per-exchange gauges (dynamic) --
        writeln!(
            out,
            "# HELP orderbook_exchange_up Exchange connection status (1=connected)"
        )
        .unwrap();
        writeln!(out, "# TYPE orderbook_exchange_up gauge").unwrap();
        for ex in &self.exchanges {
            writeln!(
                out,
                "orderbook_exchange_up{{exchange=\"{}\"}} {}",
                ex.name,
                u8::from(ex.connected.load(Relaxed))
            )
            .unwrap();
        }

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

        // -- Per-exchange histograms (dynamic) --
        writeln!(
            out,
            "# HELP orderbook_decode_duration_seconds WebSocket message decode latency"
        )
        .unwrap();
        writeln!(out, "# TYPE orderbook_decode_duration_seconds histogram").unwrap();
        for ex in &self.exchanges {
            let labels = format!("exchange=\"{}\"", ex.name);
            ex.decode_latency
                .render("orderbook_decode_duration_seconds", &labels, &mut out);
        }

        // -- Global histograms --
        writeln!(
            out,
            "# HELP orderbook_merge_duration_seconds Order book merge latency"
        )
        .unwrap();
        writeln!(out, "# TYPE orderbook_merge_duration_seconds histogram").unwrap();
        self.merge_latency
            .render("orderbook_merge_duration_seconds", "", &mut out);

        writeln!(out, "# HELP orderbook_e2e_duration_seconds End-to-end latency: WS receive to merged summary published").unwrap();
        writeln!(out, "# TYPE orderbook_e2e_duration_seconds histogram").unwrap();
        self.e2e_latency
            .render("orderbook_e2e_duration_seconds", "", &mut out);

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

/// `GET /health` — returns `OK`, `DEGRADED`, or `DOWN` based on exchange connectivity.
async fn health(State(m): State<Arc<Metrics>>) -> (StatusCode, &'static str) {
    let connected = m
        .exchanges
        .iter()
        .filter(|e| e.connected.load(Relaxed))
        .count();
    let total = m.exchanges.len();

    if connected == total {
        (StatusCode::OK, "OK\n")
    } else if connected > 0 {
        (StatusCode::OK, "DEGRADED\n")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "DOWN\n")
    }
}

/// `GET /metrics` — Prometheus text exposition format.
async fn prom_metrics(State(m): State<Arc<Metrics>>) -> String {
    m.to_prometheus()
}
