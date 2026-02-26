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
| `binance_decode_20` | SIMD byte walker + fixed-point for 20-level Binance depth snapshot | 1.85 μs | [1.80, 1.91] |
| `bitstamp_decode_20` | SIMD byte walker + fixed-point for 20-level Bitstamp order book | 1.85 μs | [1.79, 1.92] |
| `bitstamp_decode_100` | Same, but 100 levels (production Bitstamp payload) — keeps 20, skips 80 | 1.91 μs | [1.84, 1.98] |
| `fixed_point_parse` | Byte-scan decimal string to `FixedPoint(u64)` — price + quantity pair | 13.2 ns | [12.9, 13.5] |
| `merge_2x20` | K-way merge of 2×20 levels into top-10 output | 223 ns | [212, 235] |
| `e2e_parse_merge` | Full pipeline: Binance 20 + Bitstamp 100 → decode → merge → Summary | **3.34 μs** | [3.23, 3.46] |

## Analysis

### SIMD Skip Efficiency

`bitstamp_decode_100` (1.91μs) is nearly identical to `bitstamp_decode_20` (1.85μs). The SIMD `memmem` search skips the 80 excess levels so fast that buffer size barely affects decode time. The walker reads the first 20 levels and bails out — the subsequent `seek()` to `"asks":` jumps past the remaining data in a single vectorized scan.

### Fixed-Point Parse Cost

At 13.2ns per price+quantity pair, `FixedPoint::parse` adds ~264ns for a 20-level side (20 pairs). This is ~14% of the total decode time — the rest is SIMD search and JSON structure navigation.

### Merge Dominance

The merge step (223ns) is roughly 10× cheaper than a single exchange decode. The hot path is dominated by JSON parsing, not merging. This validates the choice to optimize the parser (SIMD walker) rather than the merger algorithm.

### Pipeline Breakdown

For the e2e benchmark (3.34μs):
- Binance 20-level decode: ~1.85μs (55%)
- Bitstamp 100-level decode: ~1.91μs → but only top 20 parsed
- Merge 2×20 → top 10: ~0.22μs (7%)
- Overhead (function calls, setup): ~0.36μs

## Production Latency

Live production numbers (measured via internal Prometheus histograms with 100ns-resolution buckets):

| Metric | Value |
|--------|-------|
| Decode P50 | ~2μs |
| Decode P99 | ~5μs |
| Merge P50 | sub-1μs |
| **E2E P50** | **6μs** |
| **E2E P99** | **sub-20μs** |

The gap between benchmark e2e (3.34μs) and production P50 (6μs) is accounted for by:
1. WebSocket frame allocation (~1μs) — the `String` from tokio-tungstenite
2. SPSC ring buffer transfer (~10ns)
3. `watch::send` notification (~50ns)
4. Clock reads for metrics recording (~40ns)
5. Measurement overhead (histogram `record()`)

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
