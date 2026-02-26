# Design Tradeoffs

Every major design decision in this system, with alternatives considered and rationale.

## SPSC Ring Buffers over `mpsc` Channels

**Chose**: Per-exchange `rtrb` SPSC ring buffers (4 slots each).

**Over**: `tokio::sync::mpsc`, `crossbeam::channel`, `flume`.

**Why**: SPSC ring buffers have the lowest possible synchronization overhead — a single `store(Release)` for push, a single `load(Acquire)` for pop. No CAS loops, no mutex, no atomic read-modify-write. The tokio `mpsc` channel adds wake-up notifications, semaphore management, and linked-list node allocation. For a pipeline where only the latest value matters (order book snapshots), these costs are pure overhead.

The ring is intentionally small (4 slots). For order book data, the latest snapshot supersedes all previous ones. A large ring would cause the merger to drain dozens of stale snapshots after any processing delay. With 4 slots, the merger always processes near-fresh data, and the producer simply drops on full (correct behavior for this data model).

**Tradeoff**: One ring per exchange (not shared). Acceptable for 2 exchanges. Would need rethinking for dozens.

## Busy-Poll Merger over Async/Epoll

**Chose**: Dedicated OS thread with `core::hint::spin_loop()` (PAUSE on x86).

**Over**: `tokio::select!` on channel receivers, `crossbeam::select!`, condvar-based wake-up.

**Why**: Any sleep/wake mechanism adds 1-50μs of latency — the OS must schedule the thread back onto a core, reload caches, and resume execution. For an HFT merger that processes data in ~200ns, this wake-up latency dominates the pipeline. Busy-poll eliminates it entirely: the thread is always running, always cache-warm, and responds within nanoseconds of data arriving.

**Tradeoff**: Burns one CPU core continuously. Acceptable for latency-critical infrastructure. Standard practice in HFT (LMAX Disruptor, Aeron, custom exchange matching engines all use busy-spin).

## Fixed-Point Integer Prices over `f64`

**Chose**: `FixedPoint(u64)` — 10^8-scaled integer — on the entire hot path.

**Over**: `f64`, `rust_decimal::Decimal`, `bigdecimal::BigDecimal`.

**Why**:
- **Deterministic ordering**: Integer `cmp` is 1 cycle with no edge cases. `f64::total_cmp` is ~5 cycles and must handle NaN/Inf.
- **Direct string parse**: Byte scanning straight to integer — no intermediate IEEE 754 conversion, no rounding.
- **Exact arithmetic**: No floating-point representation error. `0.1 + 0.2 == 0.3` in fixed-point.
- **Both exchanges use 8 decimal places**, aligning perfectly with the 10^8 scale factor.

The `rust_decimal` crate would give arbitrary precision but requires 128-bit operations and heap-allocated string conversion. For order book comparison (which only needs `Ord`), the extra precision is unnecessary overhead.

**Tradeoff**: f64 conversion happens at boundaries (Protobuf serialization, TUI display). These are cold paths — the cost is negligible.

## Custom SIMD Byte Walker over serde/simd-json

**Chose**: Hand-written JSON scanner using `memchr::memmem` for key lookup + manual array parsing.

**Over**: `serde_json`, `simd-json` with serde derive, `sonic-rs`.

**Why**: Exchange JSON has a fixed, known schema. A generic JSON parser (even SIMD-accelerated like simd-json) must:
1. Build a tape/DOM of all fields
2. Run serde visitor dispatch for field matching
3. Allocate `String` and `Vec` for deserialized output
4. Drain unused fields (Bitstamp sends 100 levels, we keep 20)

The custom walker skips all of this. It uses `memmem::Finder` objects (precompiled SIMD patterns) to jump directly to `"bids":` and `"asks":` — bypassing envelope fields entirely. Price/quantity strings are borrowed as `&str` slices (zero-copy). The walker keeps the first 20 levels and stops — no drain loop for the remaining 80.

**Result**: ~1.85μs decode vs ~3-4μs with simd-json+serde for equivalent payloads.

**Tradeoff**: Tied to Binance/Bitstamp JSON structure. Adding a new exchange with a different schema requires writing a new walker function (not just a serde struct). However, the pattern is well-established — each walker is ~15 lines of `seek() + read_*()` calls.

## Dedicated OS Threads over Tokio Tasks

**Chose**: One OS thread per exchange + one for the merger.

**Over**: Tokio tasks on the default multi-threaded runtime.

**Why**: Tokio's work-stealing scheduler migrates tasks between worker threads at every `await` point. For a tight WS read loop, this means:
- L1/L2 cache invalidation on every migration (~5-20μs penalty)
- Unpredictable scheduling jitter from other tasks on the runtime
- Contention on the runtime's shared run queue

By giving each exchange a `current_thread` runtime on a dedicated OS thread, the WS loop runs with warm caches and zero contention. The merger thread goes further — it has no tokio runtime at all.

**Tradeoff**: 3 OS threads dedicated to this process. Each exchange thread runs its own event loop (no shared I/O reactor). Acceptable for a system with 2 exchanges; would need pooling for 10+.

## Protobuf Conversion at the Edge

**Chose**: Merger publishes internal `Summary` (stack-allocated `ArrayVec`). Protobuf conversion happens in gRPC request handlers.

**Over**: Merger serializes to Protobuf and publishes the bytes.

**Why**: Protobuf types require `Vec<Level>` (heap-allocated) and `String` (exchange name). Converting in the merger would add heap allocation and serialization to the latency-critical path. By deferring to the gRPC handler, the merger stays allocation-free, and each client connection does its own conversion on a tokio worker thread.

**Tradeoff**: If many clients connect, each does redundant conversion. For a monitoring/aggregation service with a handful of clients, this is negligible. For 1000+ clients, pre-serializing once would be better.

## `tokio::watch` over Broadcast/MPSC

**Chose**: `tokio::watch` channel from merger to gRPC server.

**Over**: `tokio::sync::broadcast`, `tokio::sync::mpsc`.

**Why**: `watch` has latest-value semantics — receivers always get the most recent value, not a queue of historical values. This is exactly right for order book data where only the current state matters. `send()` is also synchronous, so the merger thread (which has no tokio runtime) can publish without spawning futures.

`broadcast` would deliver every intermediate value, causing slow clients to buffer unbounded history. `mpsc` would require the merger to know the number of receivers.

**Tradeoff**: `watch` drops intermediate values. A client that takes 100ms to process will skip ~99 updates. For a streaming order book, this is correct behavior — the client wants the latest state, not historical replay.

## Stale Book Eviction at 5 Seconds

**Chose**: Evict exchange books older than 5 seconds before merging.

**Over**: Keep stale data indefinitely, or evict immediately on disconnect.

**Why**: Without eviction, a disconnected exchange's book lingers in the merged output. If Binance is stale by 5 seconds, its prices may be significantly wrong, creating a crossed book (negative spread) that misleads consumers. Eviction ensures the merged output degrades gracefully to single-exchange data rather than producing incorrect cross-exchange prices.

5 seconds balances two concerns:
- **Too short** (e.g., 500ms): Normal network jitter or brief reconnection would cause unnecessary single-exchange fallback, losing cross-exchange spread accuracy.
- **Too long** (e.g., 60s): Stale data poisons the merged book for a full minute after disconnect.

At Binance's 100ms update cadence, 5s = ~50 missed snapshots — clearly a disconnect, not transient jitter.

## Snapshot Streams over Diff/Delta

**Chose**: Subscribe to full order book snapshots from both exchanges.

**Over**: Diff/delta streams with local book maintenance.

**Why**: Binance `depth20@100ms` and Bitstamp `order_book` both provide full top-of-book snapshots. Using snapshots eliminates the need for:
- Local order book state management
- Sequence number reconciliation
- Snapshot + diff synchronization on reconnect
- Gap detection and recovery

The cost is slightly higher bandwidth (~2KB per snapshot vs ~100 bytes per diff), but the implementation is dramatically simpler and more robust — no state to corrupt, no gaps to recover from.

**Tradeoff**: Higher bandwidth. For 2 exchanges at 10 updates/sec, this is ~40KB/s — negligible. For full L3 market data at 100K updates/sec, diff streams would be necessary.
