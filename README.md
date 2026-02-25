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
- `orderbook_decode_duration_seconds{exchange}` — Histogram: JSON decode + parse latency (1μs–10ms buckets)
- `orderbook_merge_duration_seconds` — Histogram: order book merge latency (1μs–10ms buckets)

## Sorting Logic

The merged order book puts **best deals first**:

- **Bids** (buy orders): highest price first — a seller gets the best price from the highest bidder
- **Asks** (sell orders): lowest price first — a buyer gets the best price from the lowest seller
- **Tiebreaker**: at the same price, the larger amount comes first (more liquidity)
- **Spread**: `best_ask - best_bid`

## Design Decisions

### Channel Architecture

- **`tokio::broadcast`** (exchange → merger): Multiple exchanges fan into a single merger. Handles backpressure by dropping old messages — stale order book snapshots are worthless.
- **`tokio::watch`** (merger → gRPC): Latest-value semantics. Clients always get the most recent state. Intermediate states between two reads are irrelevant for order book data.

### Low-Latency WebSocket

- **`TCP_NODELAY`**: Disables Nagle's algorithm on all WebSocket connections via `connect_async_tls_with_config(..., disable_nagle: true)`.
- **`write_buffer_size: 0`**: Flushes every WebSocket frame immediately instead of buffering up to 128KB.
- **`&'static str` exchange names**: Zero heap allocation for the most frequently created type (`Level`).

### Pre-Allocated Merge Buffers

Working vectors for the merge sort are allocated once at startup (`Vec::with_capacity(40)`) and reused via `clear()` + `extend()` on every merge cycle. Only the final 10-element result vectors are freshly allocated per cycle (unavoidable since they cross the watch channel).

### Reconnection

Exponential backoff (1s → 30s) with random jitter to prevent thundering herd on network recovery. Each exchange adapter handles its own reconnection loop independently.

## Running Tests

```bash
cargo test
```

Tests cover merger logic: cross-exchange merging, truncation to top-10, empty book handling, sort tiebreaking, and buffer reuse verification.
