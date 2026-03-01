# Architecture

## Thread Model

```
OS Thread "ws-binance"                  OS Thread "merger"
┌─────────────────────┐   atomic      ┌──────────────────────┐
│ current_thread      │   slot        │ busy-poll (PAUSE)    │    Main Thread
│ tokio runtime       │──send()───▶│ recv() both slots    │    ┌──────────┐
│ WS → parse → send   │                │ merge → publish      │──▶│  tonic   │──▶ Client
└─────────────────────┘                └──────────────────────┘    │  gRPC    │──▶ Client
                                               ▲    │watch       └──────────┘
OS Thread "ws-bitstamp"                        │
┌─────────────────────┐   atomic              │    ┌──────────────────────────────┐
│ current_thread      │   slot                │    │  HTTP :9090                  │
│ tokio runtime       │──send()────────────────┘    │  GET /health → OK/DEGRADED   │
│ WS → parse → send   │                             │  GET /metrics → Prometheus   │
└─────────────────────┘                             └──────────────────────────────┘
```

Four OS threads, each with a single responsibility:

| Thread | Runtime | Role | Pinning |
|--------|---------|------|---------|
| `ws-binance` | `current_thread` tokio | WS receive → JSON parse → slot send | OS-scheduled |
| `ws-bitstamp` | `current_thread` tokio | WS receive → JSON parse → slot send | OS-scheduled |
| `merger` | None (plain thread) | Slot recv → k-way merge → watch publish | Last CPU core |
| `main` | Multi-threaded tokio | gRPC serving, metrics HTTP, signal handling | OS-scheduled |

### Why Dedicated Threads

The multi-threaded tokio runtime uses work-stealing -- tasks can migrate between worker threads at any `await` point. For latency-sensitive WS receive loops, this causes 5-20μs jitter from cache invalidation and scheduler overhead. By giving each exchange its own `current_thread` runtime on a dedicated OS thread, the WS read loop runs with no task migration, no work-stealing, and warm L1/L2 caches.

The merger has no async code at all. It's a tight synchronous loop: `recv() → merge → publish → spin_loop()`. Running it on a plain OS thread avoids the overhead of async state machines and future polling entirely.

### Core Pinning

The merger thread auto-pins to the last available CPU core at startup using `libc::sched_setaffinity`. Combined with Docker `cpuset`, this gives the merger a dedicated core with no contention from other threads:

```yaml
# docker-compose.yml
cpuset: "0-3"   # merger pins to core 3, exchange + tokio use 0-2
```

For maximum isolation, add `isolcpus=3` to kernel boot parameters -- this prevents the OS scheduler from placing *any* work on core 3.

## Data Flow

### Stage 1: WebSocket Receive

Each exchange adapter connects with `TCP_NODELAY` and `write_buffer_size: 0` for immediate frame delivery. The WS text frame arrives as a `String` from tokio-tungstenite -- the only heap allocation on the hot path (unavoidable without kernel bypass).

### Stage 2: JSON Parse (Fused Walker)

Each exchange adapter has its own fused byte walker matching its specific wire format in a single forward pass. The walkers use SIMD-accelerated substring search (`memchr::memmem`) via shared `Scanner` utilities to seek directly to `"bids":` and `"asks":` -- skipping all envelope fields without parsing them. `read_quoted_decimal()` opens the quote, scans digits/dot building `int_part`+`frac_part` in a single pass, closes the quote, and returns `FixedPoint` directly -- no intermediate `&str`, no double-scan. For 20-level Binance messages (80 quoted decimals), this eliminates 80 redundant string scans. Zero-amount levels are filtered inline during parsing.

The level parser (`read_raw_levels`) uses a **compact JSON fast path**: all byte checks are direct (`expect_byte`) with no whitespace scanning. Exchange APIs always send compact arrays, so the ~160 `skip_ws()` calls per 20-level message are eliminated entirely.

**Latency**: ~660ns per 20-level snapshot (Criterion median, keeps top 10).

### Stage 3: Atomic Slot Transfer

The parsed `OrderBook` (stack-allocated `ArrayVec<RawLevel, DEPTH>`) is written into the per-exchange atomic slot -- a hand-rolled triple-buffered SPSC channel with latest-value overwrite semantics. The send is a single buffer write + one atomic swap (`Release`). No CAS, no mutex, no contention, no heap allocation. Previous unread values are silently overwritten in-place (correct for order book data -- each snapshot supersedes the previous).

### Stage 4: Merge

The merger performs one merge-and-publish per input: every `recv()` from any slot triggers a fresh merge with the latest data from all exchanges. With atomic slot semantics, `recv()` always returns the freshest snapshot or `None` -- no drain loop needed. This ensures every book's end-to-end latency is individually recorded. `BookStore` uses direct array indexing (`exchange_id` IS the index) for O(1) insert -- no linear scan. The k-way merge uses stack-allocated cursors to interleave pre-sorted bid/ask arrays in O(DEPTH × k) comparisons (~20 for 2 exchanges). Merge latency is measured offline by Criterion benchmarks; only E2E latency is recorded on the hot path (saves 3 atomic ops per merge).

**Latency**: ~314ns per merge (Criterion median).

### Stage 5: Publish

The merged `Summary` is published via `tokio::watch` (latest-value semantics). `send()` is synchronous -- the merger thread doesn't need a tokio runtime. gRPC client handlers receive the latest value and convert to Protobuf on their own tokio worker threads.

### End-to-End Latency Budget

| Stage | Median | Notes |
|-------|--------|-------|
| WS frame → parse complete | 660 ns | Per-exchange fused walker + FixedPoint parse |
| Slot send | ~15 ns | Buffer write + atomic swap |
| Slot recv + merge | 314 ns | k-way merge, 2×10 → top 10 |
| watch::send | ~50 ns | Atomic swap |
| **Total hot path** | **~1.0 μs** | Parse + transfer + merge |
| Protobuf encode (cold) | ~1 μs | Per-client, off merger thread |

## Memory Layout

All hot-path types are stack-allocated (no heap). `Level` is `Copy`; `OrderBook` and `Summary` are `Clone`. Exchange attribution uses `ExchangeId = u8` instead of `&'static str` (16→1 byte), reducing `Level` from 32 to 24 bytes (25% reduction). Name lookup via `exchange_name()` happens only in `to_proto()` (cold path, per-client gRPC task).

```
Level (24 bytes):
  price:       FixedPoint   (8 bytes: u64)
  amount:      FixedPoint   (8 bytes: u64)
  exchange_id: ExchangeId   (1 byte: u8 + 7 padding)

RawLevel (16 bytes):
  price:  FixedPoint  (8 bytes: u64)
  amount: FixedPoint  (8 bytes: u64)

OrderBook (~340 bytes, DEPTH=10):
  exchange_id:  ExchangeId
  bids:         ArrayVec<RawLevel, DEPTH>  (inline array, no heap)
  asks:         ArrayVec<RawLevel, DEPTH>  (inline array, no heap)
  decode_start: Instant

Summary (~500 bytes, DEPTH=10):
  spread_raw: i64
  bids:       ArrayVec<Level, DEPTH>  (inline array, no heap)
  asks:       ArrayVec<Level, DEPTH>  (inline array, no heap)
```

The only heap allocations are:
1. The WebSocket frame `String` (from tokio-tungstenite, unavoidable without kernel bypass)
2. Protobuf encoding on the gRPC egress path (cold, per-client)

## Metrics Architecture

Lock-free Prometheus metrics using `AtomicU64` counters and a custom histogram with O(1) `record()` (single `fetch_add` into the correct bucket). Cumulative sums for Prometheus exposition are computed lazily on the `/metrics` scrape path -- never on the hot path.

16 logarithmic buckets from 100ns to 100ms (1, 5, 10, 25, 50 progression per decade) cover the full latency range with sufficient granularity for P50/P99/P99.9 analysis.

## Data Integrity

- **Binance `lastUpdateId`**: Tracked for stale/duplicate detection. Out-of-order or duplicate snapshots (where `lastUpdateId` does not increase) are detected and dropped.
- **Stale book eviction**: If an exchange disconnects, its book is evicted after 5 seconds to prevent stale prices from contaminating the merged output. 5s tolerates network jitter while remaining well under the threshold where stale data creates false arbitrage signals.
- **Snapshot streams**: Both exchanges provide full order book snapshots (not incremental diffs), so no local book maintenance or sequence reconciliation is required.
