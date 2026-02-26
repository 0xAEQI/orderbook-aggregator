# Order Book Aggregator

[![CI](https://github.com/0xAEQI/orderbook-aggregator/actions/workflows/ci.yml/badge.svg)](https://github.com/0xAEQI/orderbook-aggregator/actions/workflows/ci.yml)

Real-time order book aggregator that connects to **Binance** and **Bitstamp** WebSocket feeds, merges their order books, and streams the top-10 bid/ask levels with spread via **gRPC**.

Built for latency: **6μs P50, sub-20μs P99** end-to-end (WS frame received → merged summary published). Zero-allocation hot path, SIMD-accelerated JSON parsing, dedicated OS threads with core pinning, and a busy-poll merger.

## Documentation

| Document | Description |
|----------|-------------|
| [Architecture](docs/ARCHITECTURE.md) | Thread model, data flow, latency budget, memory layout, core pinning |
| [Benchmarks](docs/BENCHMARKS.md) | Criterion results, production latency, hardware sensitivity, how to reproduce |
| [Design Tradeoffs](docs/TRADEOFFS.md) | Every major decision with alternatives considered and rationale |

## Quick Start

### Docker (recommended)

```bash
docker compose up
```

Starts the server + monitoring stack:
- **Server** — gRPC stream on `:50051`
- **Prometheus** — metrics scraping at [localhost:9091](http://localhost:9091)
- **Grafana** — pre-built dashboard at [localhost:3000](http://localhost:3000) (anonymous access)

TUI client (interactive terminal):

```bash
docker compose run --rm client
```

### Cargo

```bash
cargo build --release
cargo run --release --bin orderbook-aggregator
cargo run --release --bin client  # TUI client (separate terminal)
```

## Configuration

| Flag | Default | Description |
|------|---------|-------------|
| `-s, --symbol` | `ethbtc` | Trading pair (alphanumeric, both exchanges) |
| `-p, --port` | `50051` | gRPC server port |
| `-m, --metrics-port` | `9090` | Metrics/health HTTP port |

```bash
# Custom pair and ports
cargo run --release --bin orderbook-aggregator -- --symbol btcusdt --port 50052

# grpcurl
grpcurl -plaintext -import-path proto -proto orderbook.proto \
  localhost:50051 orderbook.OrderbookAggregator/BookSummary
```

## Monitoring

Grafana dashboard shows: exchange connectivity, messages/sec, errors/sec, decode/merge/e2e latency histograms (P50/P99/P99.9), uptime.

```bash
curl localhost:9090/health    # OK, DEGRADED, or DOWN
curl localhost:9090/metrics   # Prometheus text format
```

Exposed metrics:

| Metric | Type | Description |
|--------|------|-------------|
| `orderbook_messages_total{exchange}` | Counter | WebSocket messages received |
| `orderbook_errors_total{exchange}` | Counter | Parse/connection errors |
| `orderbook_reconnections_total{exchange}` | Counter | Reconnection attempts |
| `orderbook_merges_total` | Counter | Merge operations |
| `orderbook_exchange_up{exchange}` | Gauge | Connection status (1=up) |
| `orderbook_uptime_seconds` | Gauge | Process uptime |
| `orderbook_decode_duration_seconds{exchange}` | Histogram | JSON decode latency |
| `orderbook_merge_duration_seconds` | Histogram | Merge latency |
| `orderbook_e2e_duration_seconds` | Histogram | End-to-end latency |

## Performance Summary

| Stage | Latency | Method |
|-------|---------|--------|
| JSON decode (20 levels) | 1.85 μs | SIMD byte walker (`memchr::memmem`) + `FixedPoint::parse` |
| Merge (2×20 → top 10) | 223 ns | K-way merge with stack-allocated cursors |
| E2E (parse + merge) | 3.34 μs | Criterion benchmark, synthetic data |
| **Production E2E P50** | **6 μs** | Live exchange data |
| **Production E2E P99** | **< 20 μs** | Live exchange data |

See [docs/BENCHMARKS.md](docs/BENCHMARKS.md) for methodology, full results, and reproduction instructions.

## Key Design Choices

- **SPSC ring buffers** — `store(Release)` / `load(Acquire)` only, no CAS, no contention
- **Busy-poll merger** — `core::hint::spin_loop()` (PAUSE), sub-microsecond wake-up
- **SIMD JSON walker** — `memchr::memmem` pattern search, zero-copy `&str` borrowing
- **Fixed-point prices** — `FixedPoint(u64)` with 10^8 scaling, integer `cmp` in merger
- **Dedicated OS threads** — no work-stealing jitter, no async overhead on merger
- **Core pinning** — merger auto-pins to last CPU core via `core_affinity`

See [docs/TRADEOFFS.md](docs/TRADEOFFS.md) for alternatives considered and full rationale.

## Testing

```bash
cargo test     # 52 tests
cargo clippy   # 0 warnings, unsafe_code = "forbid"
cargo bench    # Criterion benchmarks
```

52 tests covering:
- **Integration**: end-to-end gRPC stream — mock exchange data through SPSC merger to gRPC client
- **Merger**: cross-exchange merging, single-exchange degraded mode, crossed book (negative spread), truncation to top-10, empty book handling, bid/ask tiebreaking by amount, k-way interleave correctness, stale book eviction
- **BookStore**: insert-overwrites-existing, overflow beyond MAX_EXCHANGES silently dropped, evict_stale keeps fresh books
- **SPSC ring**: ring-full drops snapshot (returns true), consumer-abandoned returns false
- **Health endpoint**: all-connected → OK, partial → DEGRADED, none → DOWN (503)
- **Histogram**: record + render produces correct cumulative bucket counts, sum, and count
- **FixedPoint**: parse formats (integer, fractional, leading dot, trailing dot, dot-only, leading zeros, zero, truncation), rejection of invalid input and overflow, f64 roundtrip, ordering, display
- **JSON walker**: Binance/Bitstamp happy path, unknown field tolerance, empty levels, non-data events, level capping at 20, malformed JSON rejection
- **Exchange parsers**: realistic payloads, unknown field tolerance, level capping
- **Config**: symbol validation (empty, special chars, case normalization)

## Project Structure

```
src/
  main.rs              Entry point — thread spawning, signal handling, gRPC server
  lib.rs               Library root — module declarations
  config.rs            CLI configuration (clap)
  types.rs             Core types: FixedPoint, Level, OrderBook, Summary
  json_walker.rs       SIMD byte walker for zero-copy JSON parsing
  merger.rs            Busy-poll merger with k-way merge and stale eviction
  metrics.rs           Lock-free Prometheus metrics and health endpoint
  server.rs            gRPC service (tonic) with protobuf conversion
  client.rs            Ratatui TUI client
  error.rs             Error types (thiserror)
  exchange/
    mod.rs             Exchange trait, shared utilities (parse, backoff, SPSC push)
    binance.rs         Binance depth20@100ms WebSocket adapter
    bitstamp.rs        Bitstamp order_book WebSocket adapter
proto/
  orderbook.proto      gRPC service definition
docs/
  ARCHITECTURE.md      Thread model, data flow, latency budget
  BENCHMARKS.md        Criterion results, production latency, reproduction
  TRADEOFFS.md         Design decisions with alternatives and rationale
```
