//! Criterion benchmarks for the order book aggregator hot path.
//!
//! Measures the zero-allocation pipeline: JSON decode → fixed-point parse → merge.
//!
//! Run: `cargo bench`

use std::time::Instant;

use criterion::{Criterion, black_box, criterion_group, criterion_main};

use orderbook_aggregator::exchange::binance::parse_depth_json;
use orderbook_aggregator::exchange::bitstamp::parse_order_book_json;
use orderbook_aggregator::merger::{BookStore, merge};
use orderbook_aggregator::types::{FixedPoint, Level, OrderBook};

// ── JSON payloads ────────────────────────────────────────────────────────────

/// Generate a realistic Binance depth20 JSON payload with `n` levels per side.
fn binance_json(n: usize) -> String {
    let bids: Vec<String> = (0..n)
        .map(|i| format!("[\"0.{:08}\", \"{}.00000000\"]", 6824 - i, 12 - (i % 10)))
        .collect();
    let asks: Vec<String> = (0..n)
        .map(|i| format!("[\"0.{:08}\", \"{}.00000000\"]", 6825 + i, 10 - (i % 8)))
        .collect();
    format!(
        r#"{{"lastUpdateId":123456789,"bids":[{}],"asks":[{}]}}"#,
        bids.join(","),
        asks.join(",")
    )
}

/// Generate a realistic Bitstamp `order_book` JSON message with `n` levels per side.
fn bitstamp_json(n: usize) -> String {
    let bids: Vec<String> = (0..n)
        .map(|i| format!("[\"0.{:08}\", \"{}.00000000\"]", 6824 - i, 12 - (i % 10)))
        .collect();
    let asks: Vec<String> = (0..n)
        .map(|i| format!("[\"0.{:08}\", \"{}.00000000\"]", 6825 + i, 10 - (i % 8)))
        .collect();
    format!(
        r#"{{"event":"data","channel":"order_book_ethbtc","data":{{"timestamp":"1700000000","microtimestamp":"1700000000000000","bids":[{}],"asks":[{}]}}}}"#,
        bids.join(","),
        asks.join(",")
    )
}

/// Build a pre-parsed `OrderBook` with `n` levels per side for merge benchmarks.
fn make_book(exchange: &'static str, base_bid: f64, base_ask: f64, n: usize) -> OrderBook {
    OrderBook {
        exchange,
        bids: (0..n)
            .map(|i| Level {
                exchange,
                price: FixedPoint::from_f64(base_bid - i as f64 * 0.001),
                amount: FixedPoint::from_f64(10.0 - (i % 8) as f64),
            })
            .collect(),
        asks: (0..n)
            .map(|i| Level {
                exchange,
                price: FixedPoint::from_f64(base_ask + i as f64 * 0.001),
                amount: FixedPoint::from_f64(8.0 - (i % 6) as f64),
            })
            .collect(),
        decode_start: Instant::now(),
    }
}

// ── Benchmarks ───────────────────────────────────────────────────────────────

fn bench_binance_decode(c: &mut Criterion) {
    let json = binance_json(20);
    c.bench_function("binance_decode_20", |b| {
        b.iter(|| black_box(parse_depth_json(black_box(&json))));
    });
}

fn bench_bitstamp_decode(c: &mut Criterion) {
    let json = bitstamp_json(20);
    c.bench_function("bitstamp_decode_20", |b| {
        b.iter(|| black_box(parse_order_book_json(black_box(&json))));
    });
}

fn bench_bitstamp_decode_100(c: &mut Criterion) {
    let json = bitstamp_json(100);
    c.bench_function("bitstamp_decode_100", |b| {
        b.iter(|| black_box(parse_order_book_json(black_box(&json))));
    });
}

fn bench_fixed_parse(c: &mut Criterion) {
    c.bench_function("fixed_point_parse", |b| {
        b.iter(|| {
            black_box(FixedPoint::parse(black_box("0.06824000")).unwrap());
            black_box(FixedPoint::parse(black_box("12.50000000")).unwrap());
        });
    });
}

fn bench_merge(c: &mut Criterion) {
    let book_a = make_book("binance", 0.06824, 0.06825, 20);
    let book_b = make_book("bitstamp", 0.06823, 0.06826, 20);

    c.bench_function("merge_2x20", |b| {
        b.iter_batched(
            || {
                let mut store = BookStore::new();
                store.insert(book_a.clone());
                store.insert(book_b.clone());
                store
            },
            |store| black_box(merge(&store)),
            criterion::BatchSize::SmallInput,
        );
    });
}

fn bench_e2e(c: &mut Criterion) {
    let binance = binance_json(20);
    let bitstamp = bitstamp_json(100); // Bitstamp sends 100 levels in production.

    c.bench_function("e2e_parse_merge", |b| {
        b.iter(|| {
            let book_a = parse_depth_json(&binance).unwrap();
            let book_b = parse_order_book_json(&bitstamp).unwrap();
            let mut store = BookStore::new();
            store.insert(book_a);
            store.insert(book_b);
            black_box(merge(&store))
        });
    });
}

criterion_group! {
    name = benches;
    // 10s measurement, 200 samples, 3s warmup — reduces variance on noisy machines.
    config = Criterion::default()
        .measurement_time(std::time::Duration::from_secs(10))
        .sample_size(200)
        .warm_up_time(std::time::Duration::from_secs(3))
        .noise_threshold(0.03);
    targets =
        bench_binance_decode,
        bench_bitstamp_decode,
        bench_bitstamp_decode_100,
        bench_fixed_parse,
        bench_merge,
        bench_e2e,
}
criterion_main!(benches);
