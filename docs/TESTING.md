# Testing

## Running

```bash
cargo test     # 53 tests
cargo clippy   # 0 warnings, unsafe_code = "forbid"
cargo bench    # Criterion benchmarks (see BENCHMARKS.md)
```

## Coverage

53 tests covering:

**Integration** -- end-to-end gRPC stream: mock exchange data through SPSC merger to gRPC client.

**Merger** -- cross-exchange merging, single-exchange degraded mode, crossed book (negative spread), truncation to top-10, empty book handling, bid/ask tiebreaking by amount, k-way interleave correctness, stale book eviction.

**BookStore** -- insert-overwrites-existing, overflow beyond MAX_EXCHANGES silently dropped, evict_stale keeps fresh books.

**SPSC ring** -- ring-full drops snapshot (returns true), consumer-abandoned returns false.

**Health endpoint** -- all-connected returns OK, partial returns DEGRADED, none returns DOWN (503).

**Histogram** -- record + render produces correct cumulative bucket counts, sum, and count.

**FixedPoint** -- parse formats (integer, fractional, leading dot, trailing dot, dot-only, leading zeros, zero, truncation), rejection of invalid input and overflow, f64 roundtrip, ordering, display.

**JSON walker** -- Binance/Bitstamp happy path, unknown field tolerance, empty levels, non-data events, level capping at 20, malformed JSON rejection.

**Exchange parsers** -- realistic payloads, unknown field tolerance, level capping.

**Config** -- symbol validation (empty, special chars, case normalization).

## CI

GitHub Actions runs on every push and PR:

1. `cargo clippy --all-targets -- -D warnings`
2. `cargo test`
3. `cargo build --release`

Config: `.github/workflows/ci.yml`
