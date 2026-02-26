# Architecture

## Thread Model

```
OS Thread "ws-binance"                  OS Thread "merger"
┌─────────────────────┐                ┌──────────────────────┐
│ current_thread      │   SPSC(4)     │ busy-poll (PAUSE)    │    Main Thread
│ tokio runtime       │──rtrb::push──▶│ pop() both rings     │    ┌──────────┐
│ WS → parse → push   │                │ merge → publish      │──▶│  tonic   │──▶ Client
└─────────────────────┘                └──────────────────────┘    │  gRPC    │──▶ Client
                                               ▲    │watch       └──────────┘
OS Thread "ws-bitstamp"                        │
┌─────────────────────┐   SPSC(4)             │    ┌──────────────────────────────┐
│ current_thread      │──rtrb::push────────────┘    │  HTTP :9090                  │
│ tokio runtime       │                             │  GET /health → OK/DEGRADED   │
│ WS → parse → push   │                             │  GET /metrics → Prometheus   │
└─────────────────────┘                             └──────────────────────────────┘
```

Four OS threads, each with a single responsibility:

| Thread | Runtime | Role | Pinning |
|--------|---------|------|---------|
| `ws-binance` | `current_thread` tokio | WS receive → JSON parse → SPSC push | OS-scheduled |
| `ws-bitstamp` | `current_thread` tokio | WS receive → JSON parse → SPSC push | OS-scheduled |
| `merger` | None (plain thread) | SPSC pop → k-way merge → watch publish | Last CPU core |
| `main` | Multi-threaded tokio | gRPC serving, metrics HTTP, signal handling | OS-scheduled |

### Why Dedicated Threads

The multi-threaded tokio runtime uses work-stealing -- tasks can migrate between worker threads at any `await` point. For latency-sensitive WS receive loops, this causes 5-20μs jitter from cache invalidation and scheduler overhead. By giving each exchange its own `current_thread` runtime on a dedicated OS thread, the WS read loop runs with no task migration, no work-stealing, and warm L1/L2 caches.

The merger has no async code at all. It's a tight synchronous loop: `pop() → merge → publish → spin_loop()`. Running it on a plain OS thread avoids the overhead of async state machines and future polling entirely.

### Core Pinning

The merger thread auto-pins to the last available CPU core at startup using `core_affinity`. Combined with Docker `cpuset`, this gives the merger a dedicated core with no contention from other threads:

```yaml
# docker-compose.yml
cpuset: "0-3"   # merger pins to core 3, exchange + tokio use 0-2
```

For maximum isolation, add `isolcpus=3` to kernel boot parameters -- this prevents the OS scheduler from placing *any* work on core 3.

## Data Flow

### Stage 1: WebSocket Receive

Each exchange adapter connects with `TCP_NODELAY` and `write_buffer_size: 0` for immediate frame delivery. The WS text frame arrives as a `String` from tokio-tungstenite -- the only heap allocation on the hot path (unavoidable without kernel bypass).

### Stage 2: JSON Parse

Each exchange adapter has its own byte walker (~15 lines) matching its specific wire format in a single forward pass. The walkers use SIMD-accelerated substring search (`memchr::memmem`) via shared `Scanner` utilities to seek directly to `"bids":` and `"asks":` -- skipping all envelope fields without parsing them. Price/quantity strings are borrowed as `&str` slices from the input buffer (zero-copy). The `FixedPoint::parse` function converts decimal strings to `u64` integers with 10^8 scaling -- no intermediate `f64`.

**Latency**: ~1.94μs per 20-level snapshot (Criterion median).

### Stage 3: SPSC Transfer

The parsed `OrderBook` (stack-allocated `ArrayVec<Level, 20>`) is pushed into the per-exchange SPSC ring buffer. The push is a single `store(Release)` -- no CAS, no mutex, no contention. Ring capacity is 4 slots; if full, the stale snapshot is dropped (correct for order book data -- the next frame supersedes it).

### Stage 4: Merge

The merger performs one merge-and-publish per input: every `pop()` from any consumer triggers a fresh merge with the latest data from all exchanges. This ensures no updates are silently collapsed and every book's end-to-end latency is individually recorded. The k-way merge uses stack-allocated cursors to interleave pre-sorted bid/ask arrays in O(TOP_N × k) comparisons (~20 for 2 exchanges).

**Latency**: ~245ns per merge (Criterion median).

### Stage 5: Publish

The merged `Summary` is published via `tokio::watch` (latest-value semantics). `send()` is synchronous -- the merger thread doesn't need a tokio runtime. gRPC client handlers receive the latest value and convert to Protobuf on their own tokio worker threads.

### End-to-End Latency Budget

| Stage | Median | Notes |
|-------|--------|-------|
| WS frame → parse complete | 1.94 μs | Per-exchange SIMD walker + FixedPoint parse |
| SPSC push | ~10 ns | Single atomic store |
| SPSC pop + merge | 245 ns | k-way merge, 2×20 → top 10 |
| watch::send | ~50 ns | Atomic swap |
| **Total hot path** | **~2.1 μs** | Parse + transfer + merge |
| Protobuf encode (cold) | ~1 μs | Per-client, off merger thread |

Production P50 (live exchange data): **6μs**. P99: **sub-20μs**.

## Memory Layout

All hot-path types are stack-allocated and `Copy`-safe:

```
Level (32 bytes):
  exchange: &'static str  (16 bytes: ptr + len)
  price:    FixedPoint     (8 bytes: u64)
  amount:   FixedPoint     (8 bytes: u64)

OrderBook (~1KB):
  exchange:     &'static str
  bids:         ArrayVec<Level, 20>  (inline array, no heap)
  asks:         ArrayVec<Level, 20>  (inline array, no heap)
  decode_start: Instant

Summary (~700 bytes):
  spread: f64
  bids:   ArrayVec<Level, 10>  (inline array, no heap)
  asks:   ArrayVec<Level, 10>  (inline array, no heap)
```

The only heap allocations are:
1. The WebSocket frame `String` (from tokio-tungstenite, unavoidable without kernel bypass)
2. Protobuf encoding on the gRPC egress path (cold, per-client)

## Metrics Architecture

Lock-free Prometheus metrics using `AtomicU64` counters and a custom histogram with O(1) `record()` (single `fetch_add` into the correct bucket). Cumulative sums for Prometheus exposition are computed lazily on the `/metrics` scrape path -- never on the hot path.

16 logarithmic buckets from 100ns to 100ms cover the full latency range with sufficient granularity for P50/P99/P99.9 analysis.

## Data Integrity

- **Binance `lastUpdateId`**: Tracked per connection for sequence gap detection. Out-of-order or duplicate snapshots are detected and dropped.
- **Stale book eviction**: If an exchange disconnects, its book is evicted after 5 seconds to prevent stale prices from contaminating the merged output. 5s tolerates network jitter while remaining well under the threshold where stale data creates false arbitrage signals.
- **Snapshot streams**: Both exchanges provide full order book snapshots (not incremental diffs), so no local book maintenance or sequence reconciliation is required.
