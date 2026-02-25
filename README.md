# Order Book Aggregator

Real-time order book aggregator that connects to **Binance** and **Bitstamp** WebSocket feeds, merges their order books, and streams the top-10 bid/ask levels with spread via **gRPC**.

## Architecture

```
WebSocket Feeds            Merger              gRPC Server
┌──────────┐              ┌───────┐           ┌──────────┐
│ Binance  │─broadcast──▶│       │──watch──▶ │          │──stream──▶ Client
│ depth20  │              │ Merge │           │  tonic   │──stream──▶ Client
│          │              │ Top10 │           │          │
│ Bitstamp │─broadcast──▶│       │           │          │
│ order_book│             └───────┘           └──────────┘
└──────────┘
          ┌──────────────────────────────┐
          │  HTTP :9090                  │
          │  GET /health → OK/DEGRADED   │
          │  GET /metrics → Prometheus   │
          └──────────────────────────────┘
```

**Data flow:**
1. Exchange adapters connect via WebSocket with `TCP_NODELAY` and `write_buffer_size: 0`
2. Each adapter parses updates into `OrderBook` snapshots and sends via `broadcast`
3. Merger receives from all exchanges, maintains latest book per exchange, merges using pre-allocated buffers
4. Merged top-10 + spread published via `tokio::watch` (latest-value semantics)
5. gRPC clients subscribe and receive the stream in real-time

## Quick Start (Docker)

```bash
docker compose up
```

This starts everything:
- **Server** — Binance + Bitstamp WS → merged gRPC stream on `:50051`
- **Client** — prints the live merged order book in the terminal
- **Prometheus** — scrapes metrics every 5s, UI at [localhost:9091](http://localhost:9091)
- **Grafana** — pre-built dashboard at [localhost:3000](http://localhost:3000) (anonymous access, no login needed)

No Rust toolchain required.

## Build & Run (Cargo)

```bash
# Build
cargo build --release

# Run with default settings (ethbtc, gRPC :50051, metrics :9090)
cargo run --release --bin orderbook-aggregator

# Custom pair and ports
cargo run --release --bin orderbook-aggregator -- --symbol btcusdt --port 50052 --metrics-port 9091

# Enable debug logging
RUST_LOG=debug cargo run --release --bin orderbook-aggregator
```

## Configuration

| Flag | Default | Description |
|------|---------|-------------|
| `-s, --symbol` | `ethbtc` | Trading pair (must exist on both exchanges) |
| `-p, --port` | `50051` | gRPC server listen port |
| `-m, --metrics-port` | `9090` | Metrics/health HTTP port |

## Testing the gRPC Stream

Built-in client for testing:

```bash
# Terminal 1: start the server
cargo run --release --bin orderbook-aggregator

# Terminal 2: start the client
cargo run --release --bin client

# Or connect to a different address
cargo run --release --bin client -- http://localhost:50052
```

Or with `grpcurl`:

```bash
grpcurl -plaintext -import-path proto -proto orderbook.proto \
  localhost:50051 orderbook.OrderbookAggregator/BookSummary
```

## Monitoring

With `docker compose up`, Grafana is pre-provisioned at [localhost:3000](http://localhost:3000) with a dashboard showing:

- Exchange connectivity status (UP/DOWN)
- Messages/sec per exchange
- Errors/sec per exchange
- Merge operations/sec
- Decode latency P50/P99/P99.9 per exchange
- Merge latency P50/P99/P99.9
- **End-to-end latency P50/P99/P99.9** (WS frame received → merged summary published)
- Uptime

The raw endpoints are also available for direct use:

```bash
curl localhost:9090/health    # OK, DEGRADED, or DOWN (inside container network)
curl localhost:9090/metrics   # Prometheus text format
```

Exposed metrics:
- `orderbook_messages_total{exchange}` — WebSocket messages received
- `orderbook_errors_total{exchange}` — Parse/connection errors
- `orderbook_merges_total` — Order book merge operations
- `orderbook_exchange_up{exchange}` — Connection status gauge (1=connected)
- `orderbook_uptime_seconds` — Process uptime
- `orderbook_decode_duration_seconds{exchange}` — Histogram: JSON decode + parse latency (100ns–100ms buckets)
- `orderbook_merge_duration_seconds` — Histogram: order book merge latency (100ns–100ms buckets)
- `orderbook_e2e_duration_seconds` — Histogram: end-to-end latency, WS receive to merged publish (100ns–100ms buckets)

## Sorting Logic

The merged order book puts **best deals first**:

- **Bids** (buy orders): highest price first — a seller gets the best price from the highest bidder
- **Asks** (sell orders): lowest price first — a buyer gets the best price from the lowest seller
- **Tiebreaker**: at the same price, the larger amount comes first (more liquidity)
- **Spread**: `best_ask - best_bid`

## Performance Engineering

### Build Configuration

`.cargo/config.toml` sets `target-cpu=native`, enabling AVX2/SSE4.2 instructions for `simd-json`'s vectorized JSON tokenizer. The release profile uses `lto = "fat"` for whole-program link-time optimization across all crates, `codegen-units = 1` for maximum inlining across compilation units, and `strip = true` to reduce binary size.

### SIMD-Accelerated Float Parsing

Price/quantity strings are parsed with [`fast-float`](https://github.com/fastfloat/fast-float-rust) instead of `str::parse::<f64>()`. `fast-float` uses SIMD-accelerated algorithms matching the Eisel-Lemire number parsing technique — 2-3x faster than the standard library. With 40 levels × 2 values = 80 float parses per WebSocket message, this is measurable at message rates of 10-20 Hz per exchange.

### O(1) Histogram Record

The Prometheus histogram stores per-bucket (non-cumulative) counts across 16 logarithmic buckets (100ns–100ms). Each `record()` call does a single `fetch_add` on the matching bucket — O(1) regardless of bucket count. Cumulative sums required by the Prometheus exposition format are computed lazily on the `/metrics` scrape path (~every 5-15s). This moves O(k) atomic operations off the hot path and onto the cold path.

### Inline Annotations

`#[inline]` hints on all per-message functions (`parse_levels`, `parse_snapshot`, `parse_book`, `merge_top_n`, `merge`, `PromHistogram::record`) ensure the compiler can inline across module boundaries. Without this, Rust's default compilation model may prevent cross-module inlining for sub-microsecond functions where call overhead is proportionally significant.

## Design Decisions

### Floating-Point Prices

Prices use `f64` to match the gRPC proto's `double` type. For a production system handling order matching or PnL accounting, a fixed-point or decimal representation would be preferred to avoid IEEE 754 rounding artifacts — but for aggregation and display of best-of-book levels, `f64`'s 15-16 significant digits exceed the precision of any crypto exchange.

### Channel Architecture

- **`tokio::broadcast`** (exchange → merger): Multiple exchanges fan into a single merger. Handles backpressure by dropping old messages — stale order book snapshots are worthless.
- **`tokio::watch`** (merger → gRPC): Latest-value semantics. Clients always get the most recent state. Intermediate states between two reads are irrelevant for order book data.

### Low-Latency Parse Path

- **simd-json**: AVX2/SSE4.2 vectorized JSON tokenizer — processes 32 bytes per CPU cycle vs 1 byte for scalar `serde_json`. Drop-in replacement via serde `Deserialize` trait.
- **Zero-copy `#[serde(borrow)]`**: Deserializes `[&str; 2]` price/qty pairs borrowed directly from the WS frame buffer. Eliminates 80+ `String` heap allocations per Binance message.
- **`&'static str` exchange names**: Zero heap allocation for the most frequently created type (`Level`).

### Low-Latency WebSocket

- **`TCP_NODELAY`**: Disables Nagle's algorithm on all WebSocket connections via `connect_async_tls_with_config(..., disable_nagle: true)`.
- **`write_buffer_size: 0`**: Flushes every WebSocket frame immediately instead of buffering up to 128KB.

### K-Way Merge

Both exchanges send pre-sorted order books. Instead of concatenating and sorting (O(n log n) ≈ 212 comparisons), a k-way merge interleaves them in O(TOP_N × k) ≈ 20 comparisons with stack-allocated cursors. Only the final 10-element result vectors are heap-allocated (unavoidable since they cross the watch channel).

### Reconnection

Exponential backoff (1s → 30s) with random jitter to prevent thundering herd on network recovery. Each exchange adapter handles its own reconnection loop independently.

## Running Tests

```bash
cargo test
```

Tests cover merger logic: cross-exchange merging, truncation to top-10, empty book handling, tiebreaking across exchanges, and k-way merge interleave correctness.
