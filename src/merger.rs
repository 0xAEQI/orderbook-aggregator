//! Order book merger.
//!
//! Runs on a **dedicated OS thread** with a busy-poll loop over per-exchange
//! SPSC ring buffers (`rtrb`). No tokio runtime, no async overhead -- just
//! `pop()` + `core::hint::spin_loop()` (PAUSE on x86). Burns one CPU core
//! for minimum wake-up latency.
//!
//! Maintains the latest book per exchange and publishes merged [`Summary`]
//! snapshots via a `tokio::watch` channel (`send()` is sync -- no runtime
//! needed).
//!
//! Uses a k-way merge of pre-sorted exchange books: `O(TOP_N × k)` comparisons
//! instead of `O(n log n)` concat+sort. For 2 exchanges × 20 levels → top 10,
//! that's ~20 comparisons vs ~212.

use std::cmp::Ordering;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use arrayvec::ArrayVec;
use rtrb::Consumer;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::metrics::Metrics;
use crate::types::{Level, OrderBook, Summary, TOP_N};

/// Number of exchanges (Binance + Bitstamp). Sizes stack-allocated arrays.
const MAX_EXCHANGES: usize = 2;

/// Books older than this are evicted before merging -- prevents stale data from
/// one exchange contaminating the merged output after a disconnect. A crossed
/// book (negative spread) from 5-second-old data is worse than a single-exchange
/// book from fresh data.
///
/// 5s tolerates brief network jitter and reconnection delays while remaining
/// well under the threshold where stale prices create false arbitrage signals.
/// At Binance's 100ms update cadence, 5s ≈ 50 missed snapshots -- clearly a
/// disconnect, not transient jitter.
const STALE_THRESHOLD: Duration = Duration::from_secs(5);

/// Latest book per exchange. Fixed-size array, linear scan -- no `HashMap`.
pub struct BookStore {
    books: [Option<OrderBook>; MAX_EXCHANGES],
    names: [&'static str; MAX_EXCHANGES],
    len: usize,
}

impl Default for BookStore {
    fn default() -> Self {
        Self::new()
    }
}

impl BookStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            books: [const { None }; MAX_EXCHANGES],
            names: [""; MAX_EXCHANGES],
            len: 0,
        }
    }

    #[inline]
    pub fn insert(&mut self, book: OrderBook) {
        let name = book.exchange;
        for i in 0..self.len {
            if self.names[i] == name {
                self.books[i] = Some(book);
                return;
            }
        }
        if self.len < MAX_EXCHANGES {
            self.names[self.len] = name;
            self.books[self.len] = Some(book);
            self.len += 1;
        }
    }

    /// Iterate over populated books (stale books already evicted as `None`).
    #[inline]
    fn iter(&self) -> impl Iterator<Item = &OrderBook> {
        self.books[..self.len].iter().filter_map(|b| b.as_ref())
    }

    /// Evict books older than `threshold`. Prevents stale data from a
    /// disconnected exchange from contaminating the merged output.
    fn evict_stale(&mut self, now: Instant, threshold: Duration) {
        for i in 0..self.len {
            if let Some(book) = &self.books[i] {
                let age = now - book.decode_start;
                if age > threshold {
                    warn!(
                        exchange = book.exchange,
                        age_ms = age.as_millis() as u64,
                        "evicting stale book"
                    );
                    self.books[i] = None;
                }
            }
        }
    }
}

/// Runs the merger on a dedicated OS thread. Spin-polls SPSC ring buffers,
/// merges, and publishes via `watch` (sync send -- no tokio runtime needed).
///
/// Exits when all producers are dropped or cancellation is signalled.
pub fn run_spsc(
    mut consumers: Vec<Consumer<OrderBook>>,
    summary_tx: &watch::Sender<Summary>,
    metrics: &Metrics,
    cancel: &CancellationToken,
) {
    // Pin to the last available core -- isolates the merger from exchange threads
    // and tokio workers which naturally spread across the remaining cores.
    // With Docker `cpuset`, this pins to the last core in the allowed set.
    match core_affinity::get_core_ids() {
        Some(cores) if !cores.is_empty() => {
            let core = *cores.last().unwrap();
            if core_affinity::set_for_current(core) {
                info!(core_id = core.id, "merger pinned to core");
            } else {
                warn!("failed to pin merger to core");
            }
        }
        _ => warn!("could not determine available cores; merger running unpinned"),
    }

    let mut books = BookStore::new();

    info!(
        "merger started (SPSC busy-poll, {} consumers)",
        consumers.len()
    );

    loop {
        if cancel.is_cancelled() {
            info!("merger cancelled");
            break;
        }

        // All producers dropped → no more data.
        if consumers.iter().all(Consumer::is_abandoned) {
            info!("all producers dropped, merger exiting");
            break;
        }

        // Drain all available snapshots before merging. If both exchanges pushed
        // in the same spin cycle, we merge once with the freshest data from each
        // instead of merging twice (first with stale data from the other).
        let mut latest_decode_start: Option<Instant> = None;
        for consumer in &mut consumers {
            while let Ok(book) = consumer.pop() {
                latest_decode_start = Some(book.decode_start);
                books.insert(book);
            }
        }

        if let Some(decode_start) = latest_decode_start {
            let t0 = Instant::now();
            books.evict_stale(t0, STALE_THRESHOLD);
            let active: ArrayVec<&str, MAX_EXCHANGES> = books.iter().map(|b| b.exchange).collect();
            let summary = merge(&books);
            metrics.merge_latency.record(t0.elapsed());
            metrics.merges.fetch_add(1, Relaxed);
            debug!(
                spread = summary.spread,
                bids = summary.bids.len(),
                asks = summary.asks.len(),
                exchanges = ?active,
                "merged"
            );
            let _ = summary_tx.send(summary);
            metrics.e2e_latency.record(decode_start.elapsed());
        } else {
            core::hint::spin_loop();
        }
    }

    info!("merger exiting");
}

/// K-way merge of pre-sorted slices → top `n`. O(n × k) comparisons, zero alloc.
#[inline]
fn merge_top_n(
    slices: &[&[Level]],
    cmp: impl Fn(&Level, &Level) -> Ordering,
    n: usize,
) -> ArrayVec<Level, TOP_N> {
    let k = slices.len();
    let mut cursors = [0usize; MAX_EXCHANGES];
    let mut result = ArrayVec::new();

    for _ in 0..n {
        let mut best: Option<usize> = None;
        for i in 0..k {
            if cursors[i] < slices[i].len() {
                best = Some(match best {
                    None => i,
                    Some(b) if cmp(&slices[i][cursors[i]], &slices[b][cursors[b]]).is_lt() => i,
                    Some(b) => b,
                });
            }
        }
        let Some(i) = best else { break };
        if result.try_push(slices[i][cursors[i]]).is_err() {
            break;
        }
        cursors[i] += 1;
    }

    result
}

/// Merge all exchange order books into a single [`Summary`].
#[inline]
#[must_use]
pub fn merge(books: &BookStore) -> Summary {
    let mut bid_slices = [&[][..]; MAX_EXCHANGES];
    let mut ask_slices = [&[][..]; MAX_EXCHANGES];
    let mut k = 0;
    for book in books.iter() {
        if k < MAX_EXCHANGES {
            bid_slices[k] = &book.bids;
            ask_slices[k] = &book.asks;
            k += 1;
        }
    }

    // Bids: highest price first, then largest amount as tiebreaker.
    let bids = merge_top_n(
        &bid_slices[..k],
        |a, b| b.price.cmp(&a.price).then(b.amount.cmp(&a.amount)),
        TOP_N,
    );

    // Asks: lowest price first, then largest amount as tiebreaker.
    let asks = merge_top_n(
        &ask_slices[..k],
        |a, b| a.price.cmp(&b.price).then(b.amount.cmp(&a.amount)),
        TOP_N,
    );

    // Spread computed as f64 -- goes directly to proto (cold path).
    let spread = match (asks.first(), bids.first()) {
        (Some(ask), Some(bid)) => ask.price.to_f64() - bid.price.to_f64(),
        _ => 0.0,
    };

    Summary { spread, bids, asks }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // Exact f64 spread from test inputs -- no arithmetic rounding.
mod tests {
    use super::*;
    use crate::types::FixedPoint;

    fn level(exchange: &'static str, price: f64, amount: f64) -> Level {
        Level {
            exchange,
            price: FixedPoint::from_f64(price),
            amount: FixedPoint::from_f64(amount),
        }
    }

    fn book(exchange: &'static str, bids: &[Level], asks: &[Level]) -> OrderBook {
        OrderBook {
            exchange,
            bids: bids.iter().copied().collect(),
            asks: asks.iter().copied().collect(),
            decode_start: Instant::now(),
        }
    }

    #[test]
    fn merge_two_exchanges() {
        let mut books = BookStore::new();

        books.insert(book(
            "binance",
            &[level("binance", 100.0, 5.0), level("binance", 99.0, 3.0)],
            &[level("binance", 101.0, 4.0), level("binance", 102.0, 2.0)],
        ));

        books.insert(book(
            "bitstamp",
            &[level("bitstamp", 100.5, 2.0), level("bitstamp", 99.5, 1.0)],
            &[level("bitstamp", 100.8, 3.0), level("bitstamp", 101.5, 1.0)],
        ));

        let summary = merge(&books);

        // Best bid should be bitstamp at 100.5.
        assert_eq!(summary.bids[0].exchange, "bitstamp");
        assert_eq!(summary.bids[0].price, FixedPoint::from_f64(100.5));

        // Best ask should be bitstamp at 100.8.
        assert_eq!(summary.asks[0].exchange, "bitstamp");
        assert_eq!(summary.asks[0].price, FixedPoint::from_f64(100.8));

        // Spread = best ask - best bid = 100.8 - 100.5 = 0.3.
        assert!((summary.spread - 0.3).abs() < 1e-10);

        // All 4 levels on each side.
        assert_eq!(summary.bids.len(), 4);
        assert_eq!(summary.asks.len(), 4);
    }

    #[test]
    fn merge_truncates_to_top_10() {
        let mut books = BookStore::new();

        let many_bids: ArrayVec<Level, { crate::types::MAX_LEVELS }> = (0..15)
            .map(|i| level("binance", 100.0 - f64::from(i), 1.0))
            .collect();
        let many_asks: ArrayVec<Level, { crate::types::MAX_LEVELS }> = (0..15)
            .map(|i| level("binance", 101.0 + f64::from(i), 1.0))
            .collect();

        books.insert(OrderBook {
            exchange: "binance",
            bids: many_bids,
            asks: many_asks,
            decode_start: Instant::now(),
        });

        let summary = merge(&books);
        assert_eq!(summary.bids.len(), 10);
        assert_eq!(summary.asks.len(), 10);
    }

    #[test]
    fn merge_empty() {
        let books = BookStore::new();

        let summary = merge(&books);
        assert!(summary.bids.is_empty());
        assert!(summary.asks.is_empty());
        assert_eq!(summary.spread, 0.0);
    }

    #[test]
    fn bid_tiebreak_by_amount() {
        let mut books = BookStore::new();

        // Same price from two exchanges -- larger amount should come first.
        books.insert(book("a", &[level("a", 100.0, 1.0)], &[]));
        books.insert(book("b", &[level("b", 100.0, 5.0)], &[]));

        let summary = merge(&books);
        assert_eq!(summary.bids[0].amount, FixedPoint::from_f64(5.0));
        assert_eq!(summary.bids[1].amount, FixedPoint::from_f64(1.0));
    }

    #[test]
    fn single_exchange_only() {
        let mut books = BookStore::new();

        // Only one exchange connected -- common during startup and reconnection.
        books.insert(book(
            "binance",
            &[level("binance", 100.0, 5.0), level("binance", 99.0, 3.0)],
            &[level("binance", 101.0, 4.0), level("binance", 102.0, 2.0)],
        ));

        let summary = merge(&books);
        assert_eq!(summary.bids.len(), 2);
        assert_eq!(summary.asks.len(), 2);
        assert_eq!(summary.bids[0].price, FixedPoint::from_f64(100.0));
        assert_eq!(summary.asks[0].price, FixedPoint::from_f64(101.0));
        assert!((summary.spread - 1.0).abs() < 1e-10);
    }

    #[test]
    fn crossed_book_negative_spread() {
        let mut books = BookStore::new();

        // Exchange A's best bid (102) exceeds Exchange B's best ask (101) -- crossed book.
        // Real scenario in multi-exchange aggregation with latency skew.
        books.insert(book(
            "a",
            &[level("a", 102.0, 1.0)],
            &[level("a", 103.0, 1.0)],
        ));
        books.insert(book(
            "b",
            &[level("b", 99.0, 1.0)],
            &[level("b", 101.0, 1.0)],
        ));

        let summary = merge(&books);
        assert_eq!(summary.bids[0].price, FixedPoint::from_f64(102.0));
        assert_eq!(summary.asks[0].price, FixedPoint::from_f64(101.0));
        // Spread is negative -- signals an arbitrage opportunity.
        assert!(summary.spread < 0.0);
        assert!((summary.spread - (-1.0)).abs() < 1e-10);
    }

    #[test]
    fn ask_tiebreak_by_amount() {
        let mut books = BookStore::new();

        // Same ask price from two exchanges -- larger amount should come first.
        books.insert(book("a", &[], &[level("a", 100.0, 1.0)]));
        books.insert(book("b", &[], &[level("b", 100.0, 5.0)]));

        let summary = merge(&books);
        assert_eq!(summary.asks[0].amount, FixedPoint::from_f64(5.0));
        assert_eq!(summary.asks[1].amount, FixedPoint::from_f64(1.0));
    }

    #[test]
    fn kway_merge_interleaves() {
        let mut books = BookStore::new();

        // Binance: 100, 98, 96   Bitstamp: 99, 97, 95
        // Expected merged bids: 100, 99, 98, 97, 96, 95
        books.insert(book(
            "binance",
            &[
                level("binance", 100.0, 1.0),
                level("binance", 98.0, 1.0),
                level("binance", 96.0, 1.0),
            ],
            &[],
        ));
        books.insert(book(
            "bitstamp",
            &[
                level("bitstamp", 99.0, 1.0),
                level("bitstamp", 97.0, 1.0),
                level("bitstamp", 95.0, 1.0),
            ],
            &[],
        ));

        let summary = merge(&books);
        let prices: Vec<f64> = summary.bids.iter().map(|l| l.price.to_f64()).collect();
        assert_eq!(prices, vec![100.0, 99.0, 98.0, 97.0, 96.0, 95.0]);
    }

    #[test]
    fn stale_book_eviction() {
        let mut books = BookStore::new();

        // Insert a book with a very old decode_start.
        let old_book = OrderBook {
            exchange: "stale_ex",
            bids: [level("stale_ex", 100.0, 1.0)].into_iter().collect(),
            asks: [level("stale_ex", 101.0, 1.0)].into_iter().collect(),
            decode_start: Instant::now().checked_sub(Duration::from_secs(10)).unwrap(),
        };
        books.insert(old_book);

        // Fresh book.
        books.insert(book(
            "fresh_ex",
            &[level("fresh_ex", 99.0, 2.0)],
            &[level("fresh_ex", 102.0, 3.0)],
        ));

        books.evict_stale(Instant::now(), STALE_THRESHOLD);
        let summary = merge(&books);

        // Only fresh_ex should survive.
        assert_eq!(summary.bids.len(), 1);
        assert_eq!(summary.bids[0].exchange, "fresh_ex");
    }

    #[test]
    fn bookstore_insert_updates_existing() {
        let mut books = BookStore::new();

        books.insert(book(
            "binance",
            &[level("binance", 100.0, 1.0)],
            &[level("binance", 101.0, 1.0)],
        ));
        // Second insert with same exchange name should overwrite.
        books.insert(book(
            "binance",
            &[level("binance", 200.0, 5.0)],
            &[level("binance", 201.0, 5.0)],
        ));

        let summary = merge(&books);
        // Should see the updated price, not the original.
        assert_eq!(summary.bids.len(), 1);
        assert_eq!(summary.bids[0].price, FixedPoint::from_f64(200.0));
        assert_eq!(summary.asks[0].price, FixedPoint::from_f64(201.0));
    }

    #[test]
    fn bookstore_insert_overflow() {
        let mut books = BookStore::new();

        // MAX_EXCHANGES is 2 -- fill both slots.
        books.insert(book("a", &[level("a", 100.0, 1.0)], &[]));
        books.insert(book("b", &[level("b", 99.0, 1.0)], &[]));
        // Third exchange should be silently dropped.
        books.insert(book("c", &[level("c", 98.0, 1.0)], &[]));

        let summary = merge(&books);
        // Only 2 levels (from a and b), not 3.
        assert_eq!(summary.bids.len(), 2);
        let exchanges: Vec<&str> = summary.bids.iter().map(|l| l.exchange).collect();
        assert!(exchanges.contains(&"a"));
        assert!(exchanges.contains(&"b"));
        assert!(!exchanges.contains(&"c"));
    }

    #[test]
    fn bookstore_evict_stale_keeps_fresh() {
        let mut books = BookStore::new();

        // Insert two fresh books (decode_start = now).
        books.insert(book(
            "binance",
            &[level("binance", 100.0, 1.0)],
            &[level("binance", 101.0, 1.0)],
        ));
        books.insert(book(
            "bitstamp",
            &[level("bitstamp", 99.0, 2.0)],
            &[level("bitstamp", 102.0, 2.0)],
        ));

        // Evict with current time -- neither should be evicted.
        books.evict_stale(Instant::now(), STALE_THRESHOLD);
        let summary = merge(&books);
        assert_eq!(summary.bids.len(), 2);
        assert_eq!(summary.asks.len(), 2);
    }
}
