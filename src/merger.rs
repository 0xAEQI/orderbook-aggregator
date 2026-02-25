//! Order book merger.
//!
//! Receives [`OrderBook`] updates from all exchanges via an mpsc channel
//! (move semantics — no clone overhead), maintains the latest book per
//! exchange, and publishes merged [`Summary`] snapshots via a `tokio::watch`
//! channel.
//!
//! Uses a k-way merge of pre-sorted exchange books: `O(TOP_N × k)` comparisons
//! instead of `O(n log n)` concat+sort. For 2 exchanges × 20 levels → top 10,
//! that's ~20 comparisons vs ~212.

use std::cmp::Ordering;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use arrayvec::ArrayVec;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::metrics::Metrics;
use crate::types::{Level, OrderBook, Summary, TOP_N};

/// Max exchanges we support (stack-allocated cursor + book arrays, no heap alloc).
const MAX_EXCHANGES: usize = 8;

/// Fixed-size exchange book store — avoids `HashMap` hashing and heap allocation.
/// With only 2 exchanges, a linear scan for name→index is faster than hashing.
struct BookStore {
    books: [Option<OrderBook>; MAX_EXCHANGES],
    /// Maps exchange name → slot index. Grows on first insert, never shrinks.
    names: [&'static str; MAX_EXCHANGES],
    len: usize,
}

impl BookStore {
    fn new() -> Self {
        Self {
            books: [const { None }; MAX_EXCHANGES],
            names: [""; MAX_EXCHANGES],
            len: 0,
        }
    }

    /// Insert or update a book. Returns the slot index.
    #[inline]
    fn insert(&mut self, book: OrderBook) {
        let name = book.exchange;
        // Linear scan — with k=2, this is 1-2 comparisons (faster than hashing).
        for i in 0..self.len {
            if self.names[i] == name {
                self.books[i] = Some(book);
                return;
            }
        }
        // New exchange — add to the end.
        if self.len < MAX_EXCHANGES {
            self.names[self.len] = name;
            self.books[self.len] = Some(book);
            self.len += 1;
        }
    }

    /// Iterate over populated books.
    #[inline]
    fn iter(&self) -> impl Iterator<Item = &OrderBook> {
        self.books[..self.len].iter().filter_map(|b| b.as_ref())
    }
}

/// Runs the merger loop until cancellation or channel closure.
pub async fn run(
    mut rx: mpsc::Receiver<OrderBook>,
    summary_tx: watch::Sender<Summary>,
    metrics: Arc<Metrics>,
    cancel: CancellationToken,
) {
    let mut books = BookStore::new();

    info!("merger started");

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("merger shutting down");
                return;
            }
            result = rx.recv() => {
                let Some(book) = result else {
                    info!("mpsc channel closed");
                    return;
                };
                let received_at = book.received_at;
                books.insert(book);
                let t0 = Instant::now();
                let summary = merge(&books);
                metrics.merge_latency.record(t0.elapsed());
                metrics.merges.fetch_add(1, Relaxed);
                debug!(
                    spread = summary.spread,
                    bids = summary.bids.len(),
                    asks = summary.asks.len(),
                    "merged"
                );
                let _ = summary_tx.send(summary);
                // e2e: WS frame received → merged summary published.
                metrics.e2e_latency.record(received_at.elapsed());
            }
        }
    }
}

/// K-way merge of pre-sorted slices, taking only the top `n` elements.
///
/// Each exchange's order book arrives pre-sorted from the exchange. Instead of
/// concatenating all levels and sorting O(m log m), we maintain a cursor per
/// exchange and pick the best head at each step: O(n × k) total comparisons.
///
/// Fully stack-allocated — zero heap allocation.
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
        // Level is Copy — no clone needed.
        if result.try_push(slices[i][cursors[i]]).is_err() {
            break;
        }
        cursors[i] += 1;
    }

    result
}

/// Merge all exchange order books into a single [`Summary`].
#[inline]
fn merge(books: &BookStore) -> Summary {
    // Stack-allocated slice collectors — no heap alloc.
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
        |a, b| {
            b.price
                .total_cmp(&a.price)
                .then(b.amount.total_cmp(&a.amount))
        },
        TOP_N,
    );

    // Asks: lowest price first, then largest amount as tiebreaker.
    let asks = merge_top_n(
        &ask_slices[..k],
        |a, b| {
            a.price
                .total_cmp(&b.price)
                .then(b.amount.total_cmp(&a.amount))
        },
        TOP_N,
    );

    let spread = match (asks.first(), bids.first()) {
        (Some(ask), Some(bid)) => ask.price - bid.price,
        _ => 0.0,
    };

    Summary { spread, bids, asks }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // Exact f64 literals from test inputs — no arithmetic rounding.
mod tests {
    use super::*;

    fn level(exchange: &'static str, price: f64, amount: f64) -> Level {
        Level {
            exchange,
            price,
            amount,
        }
    }

    fn book(exchange: &'static str, bids: &[Level], asks: &[Level]) -> OrderBook {
        OrderBook {
            exchange,
            bids: bids.iter().copied().collect(),
            asks: asks.iter().copied().collect(),
            received_at: Instant::now(),
        }
    }

    #[test]
    fn test_merge_two_exchanges() {
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
        assert_eq!(summary.bids[0].price, 100.5);

        // Best ask should be bitstamp at 100.8.
        assert_eq!(summary.asks[0].exchange, "bitstamp");
        assert_eq!(summary.asks[0].price, 100.8);

        // Spread = best ask - best bid = 100.8 - 100.5 = 0.3.
        assert!((summary.spread - 0.3).abs() < 1e-10);

        // All 4 levels on each side.
        assert_eq!(summary.bids.len(), 4);
        assert_eq!(summary.asks.len(), 4);
    }

    #[test]
    fn test_merge_truncates_to_top_10() {
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
            received_at: Instant::now(),
        });

        let summary = merge(&books);
        assert_eq!(summary.bids.len(), 10);
        assert_eq!(summary.asks.len(), 10);
    }

    #[test]
    fn test_merge_empty() {
        let books = BookStore::new();

        let summary = merge(&books);
        assert!(summary.bids.is_empty());
        assert!(summary.asks.is_empty());
        assert_eq!(summary.spread, 0.0);
    }

    #[test]
    fn test_bid_tiebreak_by_amount_across_exchanges() {
        let mut books = BookStore::new();

        // Same price from two exchanges — larger amount should come first.
        books.insert(book("a", &[level("a", 100.0, 1.0)], &[]));
        books.insert(book("b", &[level("b", 100.0, 5.0)], &[]));

        let summary = merge(&books);
        assert_eq!(summary.bids[0].amount, 5.0);
        assert_eq!(summary.bids[1].amount, 1.0);
    }

    #[test]
    fn test_kway_merge_interleaves_correctly() {
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
        let prices: Vec<f64> = summary.bids.iter().map(|l| l.price).collect();
        assert_eq!(prices, vec![100.0, 99.0, 98.0, 97.0, 96.0, 95.0]);
    }
}
