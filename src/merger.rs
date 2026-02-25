//! Order book merger.
//!
//! Receives [`OrderBook`] updates from all exchanges via a broadcast channel,
//! maintains the latest book per exchange, and publishes merged [`Summary`]
//! snapshots via a `tokio::watch` channel.

use std::collections::HashMap;

use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::types::{Level, OrderBook, Summary};

const TOP_N: usize = 10;

/// Runs the merger loop until cancellation.
pub async fn run(
    mut rx: broadcast::Receiver<OrderBook>,
    summary_tx: watch::Sender<Summary>,
    cancel: CancellationToken,
) {
    let mut books: HashMap<String, OrderBook> = HashMap::new();

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
                        books.insert(book.exchange.clone(), book);
                        let summary = merge(&books);
                        debug!(
                            spread = summary.spread,
                            bids = summary.bids.len(),
                            asks = summary.asks.len(),
                            "merged"
                        );
                        let _ = summary_tx.send(summary);
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
fn merge(books: &HashMap<String, OrderBook>) -> Summary {
    let mut all_bids: Vec<Level> = books
        .values()
        .flat_map(|b| b.bids.iter().cloned())
        .collect();

    let mut all_asks: Vec<Level> = books
        .values()
        .flat_map(|b| b.asks.iter().cloned())
        .collect();

    // Bids: highest price first, then largest amount as tiebreaker.
    all_bids.sort_by(|a, b| {
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
    all_asks.sort_by(|a, b| {
        a.price
            .partial_cmp(&b.price)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                b.amount
                    .partial_cmp(&a.amount)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });

    all_bids.truncate(TOP_N);
    all_asks.truncate(TOP_N);

    let spread = match (all_asks.first(), all_bids.first()) {
        (Some(ask), Some(bid)) => ask.price - bid.price,
        _ => 0.0,
    };

    Summary {
        spread,
        bids: all_bids,
        asks: all_asks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(exchange: &str, price: f64, amount: f64) -> Level {
        Level {
            exchange: exchange.to_string(),
            price,
            amount,
        }
    }

    fn book(exchange: &str, bids: Vec<Level>, asks: Vec<Level>) -> OrderBook {
        OrderBook {
            exchange: exchange.to_string(),
            bids,
            asks,
        }
    }

    #[test]
    fn test_merge_two_exchanges() {
        let mut books = HashMap::new();

        books.insert(
            "binance".to_string(),
            book(
                "binance",
                vec![level("binance", 100.0, 5.0), level("binance", 99.0, 3.0)],
                vec![level("binance", 101.0, 4.0), level("binance", 102.0, 2.0)],
            ),
        );

        books.insert(
            "bitstamp".to_string(),
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
        let mut books = HashMap::new();

        let many_bids: Vec<Level> = (0..15)
            .map(|i| level("binance", 100.0 - i as f64, 1.0))
            .collect();
        let many_asks: Vec<Level> = (0..15)
            .map(|i| level("binance", 101.0 + i as f64, 1.0))
            .collect();

        books.insert(
            "binance".to_string(),
            book("binance", many_bids, many_asks),
        );

        let summary = merge(&books);
        assert_eq!(summary.bids.len(), 10);
        assert_eq!(summary.asks.len(), 10);
    }

    #[test]
    fn test_merge_empty() {
        let books = HashMap::new();
        let summary = merge(&books);
        assert!(summary.bids.is_empty());
        assert!(summary.asks.is_empty());
        assert_eq!(summary.spread, 0.0);
    }

    #[test]
    fn test_bid_sort_tiebreak_by_amount() {
        let mut books = HashMap::new();

        books.insert(
            "test".to_string(),
            book(
                "test",
                vec![
                    level("a", 100.0, 1.0),
                    level("b", 100.0, 5.0),
                ],
                vec![],
            ),
        );

        let summary = merge(&books);
        // Larger amount should come first at same price.
        assert_eq!(summary.bids[0].amount, 5.0);
        assert_eq!(summary.bids[1].amount, 1.0);
    }
}
