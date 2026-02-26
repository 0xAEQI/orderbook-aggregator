//! Binance partial book depth WebSocket adapter.
//!
//! Runs on a dedicated OS thread with its own `current_thread` tokio runtime.
//! Pushes parsed [`OrderBook`] snapshots into an SPSC ring buffer -- the push
//! is a single `store(Release)`, no CAS.

use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use rtrb::Producer;
use tracing::warn;

use crate::json_walker::walk_binance;
use crate::metrics::ExchangeMetrics;
use crate::types::OrderBook;

use super::{HandleResult, WsHandler};

/// Binance partial book depth adapter (`depth20@100ms` stream).
#[derive(Default)]
pub struct BinanceHandler {
    last_seq: u64,
}

impl BinanceHandler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl WsHandler for BinanceHandler {
    const NAME: &'static str = "binance";

    fn ws_url(&self, symbol: &str) -> String {
        format!(
            "wss://stream.binance.com:9443/ws/{}@depth20@100ms",
            symbol.to_lowercase()
        )
    }

    fn process_text(
        &mut self,
        text: &str,
        producer: &mut Producer<OrderBook>,
        metrics: &ExchangeMetrics,
    ) -> HandleResult {
        let t0 = Instant::now();
        let Some((seq, bids, asks)) = walk_binance(text) else {
            metrics.errors.fetch_add(1, Relaxed);
            warn!(
                exchange = "binance",
                payload_head = &text[..text.len().min(200)],
                "parse error"
            );
            return HandleResult::Continue;
        };

        // Sequence gap detection: lastUpdateId should increase monotonically.
        if seq > 0 && self.last_seq > 0 && seq <= self.last_seq {
            warn!(
                exchange = "binance",
                seq,
                last_seq = self.last_seq,
                "out-of-order update (stale or duplicate)"
            );
            metrics.errors.fetch_add(1, Relaxed);
            return HandleResult::Continue;
        }
        self.last_seq = seq;

        let Some(book) = super::build_book("binance", &bids, &asks, t0) else {
            metrics.errors.fetch_add(1, Relaxed);
            warn!(exchange = "binance", "malformed level");
            return HandleResult::Continue;
        };
        metrics.decode_latency.record(t0.elapsed());
        metrics.messages.fetch_add(1, Relaxed);
        if !super::try_send_book(producer, book, "binance") {
            return HandleResult::Shutdown;
        }
        HandleResult::Continue
    }
}

/// Decode a Binance `depth20` JSON frame. For benchmarks.
#[must_use]
pub fn parse_depth_json(json: &str) -> Option<OrderBook> {
    let (_seq, bids, asks) = walk_binance(json)?;
    super::build_book("binance", &bids, &asks, Instant::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FixedPoint;

    const BINANCE_JSON: &str = r#"{
        "lastUpdateId": 123456789,
        "bids": [
            ["0.06824000", "12.50000000"],
            ["0.06823000", "8.30000000"],
            ["0.06822000", "5.00000000"]
        ],
        "asks": [
            ["0.06825000", "10.00000000"],
            ["0.06826000", "7.20000000"],
            ["0.06827000", "3.50000000"]
        ]
    }"#;

    #[test]
    fn parse_binance_snapshot() {
        let book = parse_depth_json(BINANCE_JSON).expect("valid JSON");

        assert_eq!(book.exchange, "binance");
        assert_eq!(book.bids.len(), 3);
        assert_eq!(book.asks.len(), 3);

        assert_eq!(book.bids[0].price, FixedPoint::parse("0.06824000").unwrap());
        assert_eq!(
            book.bids[0].amount,
            FixedPoint::parse("12.50000000").unwrap()
        );
        assert_eq!(book.bids[2].price, FixedPoint::parse("0.06822000").unwrap());

        assert_eq!(book.asks[0].price, FixedPoint::parse("0.06825000").unwrap());
        assert_eq!(
            book.asks[0].amount,
            FixedPoint::parse("10.00000000").unwrap()
        );
        assert_eq!(book.asks[2].price, FixedPoint::parse("0.06827000").unwrap());

        assert!(book.bids.iter().all(|l| l.exchange == "binance"));
        assert!(book.asks.iter().all(|l| l.exchange == "binance"));
    }

    #[test]
    fn parse_binance_ignores_unknown_fields() {
        let json = r#"{
            "lastUpdateId": 999,
            "E": 1234567890,
            "bids": [["1.0", "2.0"]],
            "asks": [["3.0", "4.0"]]
        }"#;
        let book = parse_depth_json(json).expect("should ignore unknown fields");
        assert_eq!(book.bids.len(), 1);
        assert_eq!(book.asks.len(), 1);
    }
}
