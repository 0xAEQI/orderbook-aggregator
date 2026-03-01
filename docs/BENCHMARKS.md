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
| `binance_decode_20` | Fused byte walker + FixedPoint for 20-level Binance depth (keeps 10, skips 10) | 659 ns | [638, 682] |
| `bitstamp_decode_20` | Fused byte walker + FixedPoint for 20-level Bitstamp order book (keeps 10, skips 10) | 656 ns | [636, 677] |
| `bitstamp_decode_100` | Same, but 100 levels (production Bitstamp payload) -- keeps 10, skips 90 | 731 ns | [711, 753] |
| `fixed_point_parse` | Byte-scan decimal string to `FixedPoint(u64)` -- price + quantity pair | 13.3 ns | [12.9, 13.6] |
| `merge_2x10` | K-way merge of 2×10 levels into top-10 output | 314 ns | [304, 325] |
| `e2e_parse_merge` | Full pipeline: Binance 20 + Bitstamp 100 → decode → merge → Summary | **1.65 μs** | [1.60, 1.70] |

## Analysis

### SIMD Skip Efficiency

`bitstamp_decode_100` (731ns) is within 11% of `bitstamp_decode_20` (656ns). The SIMD `memmem` search skips the 90 excess levels so fast that buffer size barely affects decode time. The walker reads the first 10 levels and bails out -- the subsequent `seek()` to `"asks":` jumps past the remaining data in a single vectorized scan.

### Fixed-Point Parse Cost

At 13.3ns per price+quantity pair, `FixedPoint::parse` adds ~133ns for a 10-level side (10 pairs). This is ~20% of the total decode time -- the rest is SIMD search and JSON structure navigation.

### Merge Dominance

The merge step (314ns) is roughly 2× cheaper than a single exchange decode. The hot path is dominated by JSON parsing, not merging. This validates the choice to optimize the parser (SIMD walker) rather than the merger algorithm.

### Pipeline Breakdown

The e2e benchmark (1.65μs) runs both decodes + merge sequentially in a single Criterion iteration. Individual benchmark medians don't sum to 1.65μs because each separate benchmark includes its own per-iteration overhead (function call setup, `black_box`, `Instant::now`). The e2e benchmark amortizes this overhead across the full pipeline:

- Binance 20-level decode: ~659ns (keeps 10)
- Bitstamp 100-level decode: ~731ns (keeps 10, skips 90)
- Merge 2×10 → top 10: ~314ns
- BookStore insert (2×): ~20ns

## Production Latency

Live numbers from Prometheus histograms (`/metrics` endpoint), 2-minute run with live Binance + Bitstamp feeds on a shared Hetzner EX-44 (Intel i7-8700, no `isolcpus`, no Docker `cpuset`):

**Decode latency** (`orderbook_decode_duration_seconds`) — per-exchange fused byte walker + FixedPoint parse:

| Exchange | Samples | P50 | P99 | Mean |
|----------|---------|-----|-----|------|
| Binance | 726 | ~3 μs | ~10 μs | 3.6 μs |
| Bitstamp | 549 | ~3 μs | ~10 μs | 3.6 μs |

**Merge latency** — measured offline by Criterion benchmarks (`merge_2x10`): **314ns median**. Not recorded on the hot path; merge latency is stable enough that Criterion benchmarks suffice.

**End-to-end latency** (`orderbook_e2e_duration_seconds`) — WS frame received → merged summary published:

| Samples | P50 | P99 | Mean |
|---------|-----|-----|------|
| 1,275 | **~8 μs** | **~30 μs** | 8.1 μs |

Percentiles are interpolated from cumulative histogram buckets (16 logarithmic buckets from 100ns to 100ms). Zero errors, zero reconnections during the 138-second measurement window.

The gap between benchmark e2e (1.65μs) and production P50 (~8μs) is accounted for by:
1. WebSocket frame allocation — the `String` from tokio-tungstenite (unavoidable without kernel bypass)
2. Slot wait time — time between send and merger's next `recv()` (busy-poll interval)
3. Production merge overhead — `evict_stale()` + `Instant::now()` + metrics recording
4. Shared-server noise — no core isolation, no `isolcpus`, CPU shared with other processes

With dedicated hardware, `isolcpus`, and Docker `cpuset`, the gap narrows -- slot wait time approaches zero with a warm busy-poll loop and zero preemption.

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
