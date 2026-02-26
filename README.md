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
| [Monitoring](docs/MONITORING.md) | Prometheus metrics, Grafana dashboard, health endpoint |
| [Testing](docs/TESTING.md) | 52-test suite, coverage breakdown, CI |

## Quick Start

### Docker (recommended)

```bash
docker compose up
```

Starts the server + monitoring stack:
- **Server** -- gRPC stream on `:50051`
- **Prometheus** -- metrics scraping at [localhost:9091](http://localhost:9091)
- **Grafana** -- pre-built dashboard at [localhost:3000](http://localhost:3000) (anonymous access)

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

Health and Prometheus metrics on the metrics port. Grafana dashboard included in Docker stack.

```bash
curl localhost:9090/health    # OK, DEGRADED, or DOWN
curl localhost:9090/metrics   # Prometheus text format
```

See [docs/MONITORING.md](docs/MONITORING.md) for the full metrics table and dashboard details.

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

- **SPSC ring buffers** -- `store(Release)` / `load(Acquire)` only, no CAS, no contention
- **Busy-poll merger** -- `core::hint::spin_loop()` (PAUSE), sub-microsecond wake-up
- **SIMD JSON walker** -- `memchr::memmem` pattern search, zero-copy `&str` borrowing
- **Fixed-point prices** -- `FixedPoint(u64)` with 10^8 scaling, integer `cmp` in merger
- **Dedicated OS threads** -- no work-stealing jitter, no async overhead on merger
- **Core pinning** -- merger auto-pins to last CPU core via `core_affinity`

See [docs/TRADEOFFS.md](docs/TRADEOFFS.md) for alternatives considered and full rationale.

## Testing

```bash
cargo test     # 52 tests
cargo clippy   # 0 warnings, unsafe_code = "forbid"
cargo bench    # Criterion benchmarks
```

See [docs/TESTING.md](docs/TESTING.md) for the full coverage breakdown and CI details.

## Project Structure

```
src/
  main.rs              Entry point -- thread spawning, signal handling, gRPC server
  lib.rs               Library root -- module declarations
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
  MONITORING.md        Prometheus metrics, Grafana dashboard, health endpoint
  TESTING.md           Test suite coverage breakdown, CI
```
