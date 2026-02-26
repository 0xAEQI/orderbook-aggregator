# Order Book Aggregator

[![CI](https://github.com/0xAEQI/orderbook-aggregator/actions/workflows/ci.yml/badge.svg)](https://github.com/0xAEQI/orderbook-aggregator/actions/workflows/ci.yml)

Real-time order book aggregator that connects to **Binance** and **Bitstamp** WebSocket feeds, merges their order books, and streams the top-10 bid/ask levels with spread via **gRPC**.

## Architecture

```
WebSocket Feeds              Merger               gRPC Server
┌──────────┐                ┌───────┐            ┌──────────┐
│ Binance  │──mpsc(move)──▶│       │──watch──▶  │          │──stream──▶ Client
│ depth20  │                │ Merge │            │  tonic   │──stream──▶ Client
│          │                │ Top10 │            │          │
│ Bitstamp │──mpsc(move)──▶│       │            │          │
│ order_book│               └───────┘            └──────────┘
└──────────┘
          ┌──────────────────────────────┐
          │  HTTP :9090                  │
          │  GET /health → OK/DEGRADED   │
          │  GET /metrics → Prometheus   │
          └──────────────────────────────┘
```

**Data flow:**
1. Exchange adapters connect via WebSocket with `TCP_NODELAY` and `write_buffer_size: 0`
2. Each adapter parses updates into `OrderBook` snapshots (stack-allocated `ArrayVec`) and sends via `mpsc` (move semantics — zero clone overhead)
3. Merger receives from all exchanges, maintains latest book per exchange in a fixed-size array (no `HashMap`), merges using a k-way cursor algorithm
4. Merged top-10 + spread published via `tokio::watch` (latest-value semantics — clients always get the most recent state)
5. gRPC clients subscribe and receive the stream; Protobuf conversion happens on the client handler task, not the merger

## Quick Start (Docker)

```bash
docker compose up
```

This starts the server and monitoring stack:
- **Server** — Binance + Bitstamp WS → merged gRPC stream on `:50051`
- **Prometheus** — scrapes metrics every 5s, UI at [localhost:9091](http://localhost:9091)
- **Grafana** — pre-built dashboard at [localhost:3000](http://localhost:3000) (anonymous access, no login needed)

To launch the TUI client (requires an interactive terminal):

```bash
docker compose run --rm client
```

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
| `-s, --symbol` | `ethbtc` | Trading pair (alphanumeric, must exist on both exchanges) |
| `-p, --port` | `50051` | gRPC server listen port |
| `-m, --metrics-port` | `9090` | Metrics/health HTTP port |

## Testing the gRPC Stream

Built-in client for testing:

```bash
# Terminal 1: start the server
cargo run --release --bin orderbook-aggregator

# Terminal 2: start the TUI client
cargo run --release --bin client

# Custom address and/or symbol display
cargo run --release --bin client -- http://localhost:50051 btcusdt
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
curl localhost:9090/health    # OK, DEGRADED, or DOWN
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

### Zero-Allocation Hot Path

The entire path from WebSocket frame receipt through merge output is zero-allocation:

- **`Level`** is `Copy` (`&'static str` + `FixedPoint` + `FixedPoint`) — no heap, no clone
- **`OrderBook`** bids/asks use `ArrayVec<Level, 20>` — stack-allocated, fixed capacity
- **`Summary`** bids/asks use `ArrayVec<Level, 10>` — stack-allocated merged output
- **Exchange → merger channel** uses `tokio::sync::mpsc` with move semantics — the `OrderBook` value is transferred, not cloned
- **JSON parsing** borrows `[&str; 2]` pairs via byte-offset tracking into stack-allocated `ArrayVec` — no `String` or `Vec` allocation
- **Exchange book store** uses a fixed `[Option<OrderBook>; 2]` array with linear scan — no `HashMap` hashing or heap allocation

The only remaining allocations are the WebSocket frame `String` (from tokio-tungstenite, unavoidable without kernel bypass) and Protobuf encoding on the gRPC egress path (cold, per-client).

### Custom Byte Walker + Fixed-Point Integer Parsing

- **Custom byte walker**: Hand-written JSON scanner (`json_walker.rs`) that walks directly to `bids`/`asks` arrays by key name, skipping envelope fields. Eliminates simd-json + serde overhead (~30-40% from visitor dispatch, field matching, and drain loops). Zero-copy `&str` borrowing via byte offset tracking — no buffer mutation needed.
- **`FixedPoint::parse`**: Direct string-to-integer byte scanning — parses decimal strings into `u64` with 10^8 scaling. No intermediate `f64`, no IEEE 754 rounding. Both Binance and Bitstamp use 8 decimal places, aligning perfectly with the 10^8 scale factor.
- **Bitstamp level cap**: `read_levels::<MAX_LEVELS>()` keeps the first 20 of 100 levels and skips the rest in a tight byte-scanning loop — no serde `IgnoredAny` overhead.

### K-Way Merge

Both exchanges send pre-sorted order books. Instead of concatenating and sorting (O(n log n) ~ 212 comparisons), a k-way merge with stack-allocated cursors interleaves them in O(TOP_N x k) ~ 20 comparisons. For k=2 exchanges, this degenerates to a two-pointer merge.

### O(1) Histogram Record

The Prometheus histogram stores per-bucket (non-cumulative) counts across 16 logarithmic buckets (100ns–100ms). Each `record()` does one `fetch_add` — O(1). Cumulative sums are computed lazily on the cold `/metrics` scrape path.

### Build Configuration

`.cargo/config.toml` sets `target-cpu=native` for optimal instruction selection. The release profile uses `lto = "fat"` for whole-program link-time optimization across all crates, `codegen-units = 1` for maximum inlining across compilation units, and `strip = true` to reduce binary size.

## Design Decisions

### Channel Architecture

- **`tokio::sync::mpsc`** (exchange → merger): Move semantics — the `OrderBook` is transferred without cloning. Combined with `ArrayVec`-based types, the entire transfer is a stack-value move. Adapters use `try_send` to avoid blocking the WebSocket read loop when the channel is full — for order book data, dropping a stale snapshot is correct since the next frame contains a newer one.
- **`tokio::watch`** (merger → gRPC): Latest-value semantics. Clients always get the most recent merged state. For order book data, intermediate states between reads are stale and irrelevant — only the current top-of-book matters.

### Fixed-Point Integer Prices

Prices and amounts use `FixedPoint(u64)` — a 10^8-scaled integer — on the entire hot path (parse → merge → publish). This gives:

- **Integer `cmp` in the merger** — 1 cycle vs ~5 for `f64::total_cmp`, no NaN/Inf edge cases
- **Direct string → integer parse** — byte scanning, no intermediate `f64`
- **Exact arithmetic** — no IEEE 754 floating-point rounding
- **f64 only at boundaries** — proto serialization and TUI display (cold paths)

Both Binance and Bitstamp use 8 decimal places, aligning perfectly with the 10^8 scale factor. The proto wire format stays `double` — conversion via `to_f64()` happens once per level at the gRPC egress (already a cold path).

### Protobuf Conversion at the Edge

The merger publishes the internal `Summary` (stack-allocated `ArrayVec`) via the watch channel. The conversion to Protobuf types (which require heap-allocated `Vec` and `String`) happens inside the gRPC request handler, on the tokio worker thread serving that client connection. This keeps the merger task free of any allocation or serialization overhead.

### Reconnection

Exponential backoff (1s → 30s) with random jitter to prevent thundering herd on network recovery. Each exchange adapter handles its own reconnection loop independently. The system continues operating with partial data if one exchange is down.

## Production Considerations

For a production-grade system at Keyrock-level latency requirements, the following changes would move the needle beyond what's appropriate for a take-home:

- **SPSC ring buffers** — One lock-free ring per exchange (e.g., `rtrb`), replacing `mpsc`. Single-producer single-consumer requires only `store(Release)` / `load(Acquire)` — no CAS, no contention.
- **Core pinning + `isolcpus`** — Pin the merger to an isolated CPU core. Combined with SPSC, this enables a busy-wait spin loop (`try_recv` + `PAUSE`) that eliminates both OS thread wake-up latency and tokio scheduler jitter.
- **Kernel bypass I/O** — `io_uring` or DPDK for the WebSocket path, eliminating ~10-50μs of kernel network stack overhead per frame.

## Running Tests

```bash
cargo test
```

25 tests covering:
- **Integration**: end-to-end gRPC stream — mock exchange data through merger to gRPC client
- **Merger**: cross-exchange merging, single-exchange degraded mode, crossed book (negative spread), truncation to top-10, empty book handling, bid and ask tiebreaking by amount, k-way merge interleave correctness
- **FixedPoint**: parse formats (integer, fractional, leading dot, truncation), rejection of invalid input, f64 roundtrip, ordering, display
- **Binance parser**: realistic depth20 JSON payload, unknown field tolerance
- **Bitstamp parser**: data message parsing, non-data event handling, custom deserializer cap at 20 levels

## Benchmarks

Criterion micro-benchmarks for every stage of the hot path:

```bash
cargo bench
```

| Benchmark | What it measures | Typical |
|-----------|-----------------|---------|
| `binance_decode_20` | Byte walker + fixed-point for 20-level Binance depth snapshot | ~1.8μs |
| `bitstamp_decode_20` | Byte walker + fixed-point for 20-level Bitstamp order book | ~1.7μs |
| `bitstamp_decode_100` | Same, but 100 levels (production Bitstamp payload) — keeps 20, skips 80 | ~3.3μs |
| `fixed_point_parse` | Byte-scan decimal string to `FixedPoint(u64)` — price + quantity pair | ~13ns |
| `merge_2x20` | K-way merge of 2×20 levels into top-10 output | ~250ns |
| `e2e_parse_merge` | Full pipeline: Binance 20 + Bitstamp 100 → decode → merge → Summary | ~4.4μs |
