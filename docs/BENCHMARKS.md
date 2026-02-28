# Benchmarks

Criterion micro-benchmarks for every stage of the hot path, from JSON decode through merge output.

## Running

```bash
cargo bench
```

Configured for stability: 200 samples, 10s measurement, 3s warmup, 3% noise threshold.

## Results

| Benchmark | What it measures | Median | 95% CI |
|-----------|-----------------|--------|--------|
| `binance_decode_20` | Per-exchange SIMD byte walker + fixed-point for 20-level Binance depth snapshot | 1.94 μs | [1.88, 2.01] |
| `bitstamp_decode_20` | Per-exchange SIMD byte walker + fixed-point for 20-level Bitstamp order book | 2.05 μs | [1.97, 2.13] |
| `bitstamp_decode_100` | Same, but 100 levels (production Bitstamp payload) -- keeps 20, skips 80 | 2.16 μs | [2.10, 2.22] |
| `fixed_point_parse` | Byte-scan decimal string to `FixedPoint(u64)` -- price + quantity pair | 13.3 ns | [13.0, 13.7] |
| `merge_2x20` | K-way merge of 2×20 levels into top-10 output | 245 ns | [220, 274] |
| `e2e_parse_merge` | Full pipeline: Binance 20 + Bitstamp 100 → decode → merge → Summary | **3.72 μs** | [3.58, 3.85] |

## Analysis

### SIMD Skip Efficiency

`bitstamp_decode_100` (2.16μs) is nearly identical to `bitstamp_decode_20` (2.05μs). The SIMD `memmem` search skips the 80 excess levels so fast that buffer size barely affects decode time. The walker reads the first 20 levels and bails out -- the subsequent `seek()` to `"asks":` jumps past the remaining data in a single vectorized scan.

### Fixed-Point Parse Cost

At 13.3ns per price+quantity pair, `FixedPoint::parse` adds ~266ns for a 20-level side (20 pairs). This is ~13% of the total decode time -- the rest is SIMD search and JSON structure navigation.

### Merge Dominance

The merge step (245ns) is roughly 8× cheaper than a single exchange decode. The hot path is dominated by JSON parsing, not merging. This validates the choice to optimize the parser (SIMD walker) rather than the merger algorithm.

### Pipeline Breakdown

The e2e benchmark (3.72μs) runs both decodes + merge sequentially in a single Criterion iteration. Individual benchmark medians don't sum to 3.72μs because each separate benchmark includes its own per-iteration overhead (function call setup, `black_box`, `Instant::now`). The e2e benchmark amortizes this overhead across the full pipeline:

- Binance 20-level decode: ~1.94μs
- Bitstamp 100-level decode: ~2.16μs → keeps 20, skips 80
- Merge 2×20 → top 10: ~0.25μs
- BookStore insert (2×): ~0.02μs

## Production Latency

Live numbers from Prometheus histograms (`/metrics` endpoint), 8-minute run with live Binance + Bitstamp feeds on a shared Hetzner EX-44 (Intel i7-8700, no `isolcpus`, no Docker `cpuset`):

**Decode latency** (`orderbook_decode_duration_seconds`) — per-exchange SIMD walker + FixedPoint parse:

| Exchange | Samples | P50 | P99 | Mean |
|----------|---------|-----|-----|------|
| Binance | 2,159 | ~5 μs | ~15 μs | 5.6 μs |
| Bitstamp | 1,716 | ~4 μs | ~15 μs | 5.2 μs |

**Merge latency** (`orderbook_merge_duration_seconds`) — evict_stale + k-way merge + watch publish:

| Samples | P50 | P99 | Mean |
|---------|-----|-----|------|
| 3,875 | ~1 μs | ~5 μs | 1.2 μs |

**End-to-end latency** (`orderbook_e2e_duration_seconds`) — WS frame received → merged summary published:

| Samples | P50 | P99 | Mean |
|---------|-----|-----|------|
| 3,875 | **~9 μs** | **~25 μs** | 10.2 μs |

Percentiles are interpolated from cumulative histogram buckets (16 logarithmic buckets from 100ns to 100ms). Zero errors, zero reconnections during the measurement window.

The gap between benchmark e2e (3.72μs) and production P50 (~9μs) is accounted for by:
1. WebSocket frame allocation (~1μs) — the `String` from tokio-tungstenite
2. SPSC ring wait time (~1-3μs) — time between push and merger's next `pop()`
3. Production merge overhead — `evict_stale()` + `Instant::now()` + metrics recording
4. Shared-server noise — no core isolation, no `isolcpus`, CPU shared with other processes

With dedicated hardware, `isolcpus`, and Docker `cpuset`, the gap narrows -- WS allocation and SPSC wait time are largely eliminated by warm caches and zero preemption.

## Hardware Sensitivity

Benchmark numbers depend on:

- **CPU architecture**: SIMD search uses SSE2/AVX2 instructions. AMD Zen 3+ and Intel 10th gen+ show best results.
- **Cache state**: The merger's busy-poll loop keeps its data cache-warm. Cold-start benchmarks may show 2-3× higher latency for the first iteration.
- **`target-cpu=native`**: The `.cargo/config.toml` sets native CPU targeting. Benchmark numbers are specific to the build machine. Cross-compiled builds may be slower due to generic SIMD codegen.
- **Core isolation**: Production numbers assume the merger thread is pinned to a dedicated core. Without pinning, P99.9 can spike to 50-100μs from preemption.

## Reproducing

```bash
# Full benchmark suite
cargo bench

# Single benchmark
cargo bench -- binance_decode_20

# Save baseline for comparison
cargo bench -- --save-baseline before

# Compare after changes
cargo bench -- --baseline before
```

Criterion generates HTML reports in `target/criterion/` with detailed statistical analysis, violin plots, and regression detection.
