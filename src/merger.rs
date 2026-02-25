//! Order book merger.
//!
//! Receives [`OrderBook`] updates from all exchanges via a broadcast channel,
//! maintains the latest book per exchange, and publishes merged [`Summary`]
//! snapshots via a `tokio::watch` channel.
//!
//! Working buffers are pre-allocated and reused across merge cycles to avoid
//! per-tick heap allocations on the hot path.

use std::collections::HashMap;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::metrics::Metrics;
use crate::types::{Level, OrderBook, Summary};

const TOP_N: usize = 10;

/// Runs the merger loop until cancellation.
pub async fn run(
    mut rx: broadcast::Receiver<OrderBook>,
    summary_tx: watch::Sender<Summary>,
    metrics: Arc<Metrics>,
    cancel: CancellationToken,
) {
    let mut books: HashMap<&'static str, OrderBook> = HashMap::new();

    // Pre-allocated working buffers — grow once, reused every merge cycle.
    // 20 levels per exchange * 2 exchanges = 40 max.
    let mut bid_buf: Vec<Level> = Vec::with_capacity(40);
    let mut ask_buf: Vec<Level> = Vec::with_capacity(40);

    info!("merger started");

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("merger shutting down");
                return;
            }
            result = rx.recv() => {
                match result {
                    Ok(book) => {
                        let received_at = book.received_at;
                        books.insert(book.exchange, book);
                        let t0 = Instant::now();
                        let summary = merge(&books, &mut bid_buf, &mut ask_buf);
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
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        debug!(skipped = n, "merger lagged — catching up");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("broadcast channel closed");
                        return;
                    }
                }
            }
        }
    }
}

/// Merge all exchange order books into a single [`Summary`].
///
/// Uses caller-provided buffers to avoid allocating working vectors on each call.
/// Only the final 10-element result vectors are freshly allocated (unavoidable since
/// they cross the watch channel).
fn merge(
    books: &HashMap<&'static str, OrderBook>,
    bid_buf: &mut Vec<Level>,
    ask_buf: &mut Vec<Level>,
) -> Summary {
    bid_buf.clear();
    ask_buf.clear();

    for book in books.values() {
        bid_buf.extend(book.bids.iter().cloned());
        ask_buf.extend(book.asks.iter().cloned());
    }

    // Bids: highest price first, then largest amount as tiebreaker.
    bid_buf.sort_by(|a, b| {
        b.price
            .partial_cmp(&a.price)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                b.amount
                    .partial_cmp(&a.amount)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });

    // Asks: lowest price first, then largest amount as tiebreaker.
    ask_buf.sort_by(|a, b| {
        a.price
            .partial_cmp(&b.price)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                b.amount
                    .partial_cmp(&a.amount)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });

    bid_buf.truncate(TOP_N);
    ask_buf.truncate(TOP_N);

    let spread = match (ask_buf.first(), bid_buf.first()) {
        (Some(ask), Some(bid)) => ask.price - bid.price,
        _ => 0.0,
    };

    Summary {
        spread,
        bids: bid_buf.to_vec(),
        asks: ask_buf.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(exchange: &'static str, price: f64, amount: f64) -> Level {
        Level {
            exchange,
            price,
            amount,
        }
    }

    fn book(exchange: &'static str, bids: Vec<Level>, asks: Vec<Level>) -> OrderBook {
        OrderBook {
            exchange,
            bids,
            asks,
            received_at: Instant::now(),
        }
    }

    #[test]
    fn test_merge_two_exchanges() {
        let mut books = HashMap::new();
        let mut bid_buf = Vec::new();
        let mut ask_buf = Vec::new();

        books.insert(
            "binance",
            book(
                "binance",
                vec![level("binance", 100.0, 5.0), level("binance", 99.0, 3.0)],
                vec![level("binance", 101.0, 4.0), level("binance", 102.0, 2.0)],
            ),
        );

        books.insert(
            "bitstamp",
            book(
                "bitstamp",
                vec![
                    level("bitstamp", 100.5, 2.0),
                    level("bitstamp", 99.5, 1.0),
                ],
                vec![
                    level("bitstamp", 100.8, 3.0),
                    level("bitstamp", 101.5, 1.0),
                ],
            ),
        );

        let summary = merge(&books, &mut bid_buf, &mut ask_buf);

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
        let mut books = HashMap::new();
        let mut bid_buf = Vec::new();
        let mut ask_buf = Vec::new();

        let many_bids: Vec<Level> = (0..15)
            .map(|i| level("binance", 100.0 - i as f64, 1.0))
            .collect();
        let many_asks: Vec<Level> = (0..15)
            .map(|i| level("binance", 101.0 + i as f64, 1.0))
            .collect();

        books.insert("binance", book("binance", many_bids, many_asks));

        let summary = merge(&books, &mut bid_buf, &mut ask_buf);
        assert_eq!(summary.bids.len(), 10);
        assert_eq!(summary.asks.len(), 10);
    }

    #[test]
    fn test_merge_empty() {
        let books = HashMap::new();
        let mut bid_buf = Vec::new();
        let mut ask_buf = Vec::new();

        let summary = merge(&books, &mut bid_buf, &mut ask_buf);
        assert!(summary.bids.is_empty());
        assert!(summary.asks.is_empty());
        assert_eq!(summary.spread, 0.0);
    }

    #[test]
    fn test_bid_sort_tiebreak_by_amount() {
        let mut books = HashMap::new();
        let mut bid_buf = Vec::new();
        let mut ask_buf = Vec::new();

        books.insert(
            "test",
            book(
                "test",
                vec![level("a", 100.0, 1.0), level("b", 100.0, 5.0)],
                vec![],
            ),
        );

        let summary = merge(&books, &mut bid_buf, &mut ask_buf);
        // Larger amount should come first at same price.
        assert_eq!(summary.bids[0].amount, 5.0);
        assert_eq!(summary.bids[1].amount, 1.0);
    }

    #[test]
    fn test_buffers_reused_across_merges() {
        let mut books = HashMap::new();
        let mut bid_buf = Vec::with_capacity(40);
        let mut ask_buf = Vec::with_capacity(40);

        books.insert(
            "test",
            book(
                "test",
                vec![level("test", 100.0, 1.0)],
                vec![level("test", 101.0, 1.0)],
            ),
        );

        // First merge grows buffers.
        let _ = merge(&books, &mut bid_buf, &mut ask_buf);
        assert!(bid_buf.capacity() >= 40);
        assert!(ask_buf.capacity() >= 40);

        // Second merge reuses — capacity stays, no realloc.
        let cap_before = (bid_buf.capacity(), ask_buf.capacity());
        let _ = merge(&books, &mut bid_buf, &mut ask_buf);
        assert_eq!(bid_buf.capacity(), cap_before.0);
        assert_eq!(ask_buf.capacity(), cap_before.1);
    }
}
