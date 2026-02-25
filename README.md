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
```

**Data flow:**
1. Exchange adapters connect via WebSocket with automatic reconnection
2. Each adapter parses updates into `OrderBook` snapshots and sends via `broadcast`
3. Merger receives from all exchanges, maintains latest book per exchange, merges and sorts
4. Merged top-10 + spread published via `tokio::watch` (latest-value semantics)
5. gRPC clients subscribe and receive the stream in real-time

## Build & Run

```bash
# Build
cargo build --release

# Run with default settings (ethbtc on port 50051)
cargo run --release --bin orderbook-aggregator

# Custom pair and port
cargo run --release --bin orderbook-aggregator -- --symbol btcusdt --port 50052

# Enable debug logging
RUST_LOG=debug cargo run --release --bin orderbook-aggregator
```

## Configuration

| Flag | Default | Description |
|------|---------|-------------|
| `-s, --symbol` | `ethbtc` | Trading pair (must exist on both exchanges) |
| `-p, --port` | `50051` | gRPC server listen port |

## Testing the gRPC Stream

The project includes a built-in client for testing:

```bash
# In terminal 1: start the server
cargo run --release --bin orderbook-aggregator

# In terminal 2: start the client
cargo run --release --bin client

# Or connect to a different address
cargo run --release --bin client -- http://localhost:50052
```

Alternatively, using `grpcurl`:

```bash
grpcurl -plaintext -import-path proto -proto orderbook.proto \
  localhost:50051 orderbook.OrderbookAggregator/BookSummary
```

## Sorting Logic

The merged order book follows the principle that **best deals come first**:

- **Bids** (buy orders): highest price first — a seller gets the best price from the highest bidder
- **Asks** (sell orders): lowest price first — a buyer gets the best price from the lowest seller
- **Tiebreaker**: at the same price, the larger amount comes first (more liquidity is a better deal)
- **Spread**: `best_ask - best_bid` (the gap between the best buy and sell prices)

## Design Decisions

### Channel Architecture

- **`tokio::broadcast`** (exchange → merger): Multiple exchanges fan into a single merger. Handles backpressure by dropping old messages, which is correct behavior — stale order book snapshots are worthless.
- **`tokio::watch`** (merger → gRPC): Latest-value semantics. When the merger updates faster than a client consumes, the client always gets the most recent state. This is the correct choice for order book data — intermediate states between two reads are irrelevant; only the latest merged book matters.

### Exchange Adapters

- **Binance `depth20@100ms`**: Partial depth stream provides snapshots of the top 20 levels every 100ms. No sequence tracking or REST snapshot needed — the exchange does the bookkeeping.
- **Bitstamp `order_book` channel**: Full snapshot on each update. Requires an explicit subscribe message after connection.
- **Reconnection**: Exponential backoff (1s → 30s) with random jitter to prevent thundering herd on network recovery.

### Performance Considerations

- **Zero-alloc exchange names**: `Level.exchange` uses `&'static str` instead of `String` — eliminates per-tick heap allocations for the most frequently created type.
- **Cooperative shutdown**: `CancellationToken` propagated to all tasks — no orphaned connections or spinning loops on Ctrl+C.
- **Bounded channels**: Broadcast buffer of 64 prevents unbounded memory growth under load.

### What I'd Add in Production

- **Metrics**: Prometheus counters for messages received/parsed/errors per exchange, histogram for merge latency
- **Health endpoint**: HTTP `/health` with per-exchange connectivity status
- **Sequence validation**: For Binance diff-depth streams (not needed for partial depth, but required for full book)
- **TCP_NODELAY**: On WebSocket connections to eliminate Nagle's algorithm delay
- **Pre-allocated merge buffers**: Reuse `Vec` allocations across merge cycles instead of collecting fresh each time

## Proto Schema

```protobuf
service OrderbookAggregator {
  rpc BookSummary(Empty) returns (stream Summary);
}

message Summary {
  double spread = 1;
  repeated Level bids = 2;  // Top 10, highest price first
  repeated Level asks = 3;  // Top 10, lowest price first
}

message Level {
  string exchange = 1;
  double price = 2;
  double amount = 3;
}
```

## Running Tests

```bash
cargo test
```

Tests cover the merger logic: cross-exchange merging, truncation to top-10, empty book handling, and sort tiebreaking by amount.
