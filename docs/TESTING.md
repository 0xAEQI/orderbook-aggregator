# Testing

## Running

```bash
cargo test     # 82 tests
cargo clippy   # 0 warnings, unsafe_code = "deny" (expect-gated for core pinning + atomic_slot)
cargo bench    # Criterion benchmarks (see BENCHMARKS.md)
```

## Coverage

82 tests covering:

**Integration** -- end-to-end gRPC stream: mock exchange data through SPSC merger to gRPC client, updated book prices reflected in stream, single-exchange degraded mode delivery.

**Merger** -- cross-exchange merging, single-exchange degraded mode, crossed book (negative spread), truncation to top-10, empty book handling, bid/ask tiebreaking by amount, k-way interleave correctness, stale book eviction, asymmetric empty sides, single-side-only merge.

**BookStore** -- insert-overwrites-existing, overflow beyond MAX_EXCHANGES silently dropped, evict_stale keeps fresh books, reinsert after eviction.

**Atomic slot** -- send/recv basic, latest-value-wins overwrite semantics, abandoned detection (both sides), drop cleanup of unread values, overwrite drops stale, cross-thread correctness.

**Health endpoint** -- all-connected returns OK, partial returns DEGRADED, none returns DOWN (503).

**Histogram** -- record + render produces correct cumulative bucket counts, sum, and count. Exact bucket boundary placement, overflow into +Inf, label rendering.

**Metrics registry** -- `to_prometheus()` renders all TYPE headers, per-exchange labels, and recorded counter values.

**gRPC server** -- `to_proto` converts internal `Summary` with `FixedPoint` values to proto `f64` fields correctly.

**FixedPoint** -- parse formats (integer, fractional, leading dot, trailing dot, dot-only, leading zeros, zero, truncation), rejection of invalid input and overflow, f64 roundtrip, ordering, display.

**JSON walker (Scanner)** -- `read_string` basic + escaped, `read_u64` parse + overflow, `seek` advances past needle, `extract_string` cold-path key extraction.

**JSON walker (exchange)** -- Binance/Bitstamp happy path, unknown field tolerance, empty levels, non-data events, level capping at DEPTH, malformed JSON rejection.

**Exchange parsers** -- realistic payloads, unknown field tolerance, level capping.

**Config** -- symbol validation (empty, special chars, case normalization).

## CI

GitHub Actions runs on every push and PR:

1. `cargo fmt --check`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo test`
4. `cargo audit`
5. `cargo build --release`

Config: `.github/workflows/ci.yml`
