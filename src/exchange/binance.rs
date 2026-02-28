//! Binance partial book depth WebSocket adapter.
//!
//! Runs on a dedicated OS thread with its own `current_thread` tokio runtime.
//! Pushes parsed [`OrderBook`] snapshots into an SPSC ring buffer -- the push
//! is a single `store(Release)`, no CAS.

use std::sync::OnceLock;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use memchr::memmem;
use rtrb::Producer;
use tracing::warn;

use crate::json_walker::{Levels, Scanner, read_levels};
use crate::metrics::ExchangeMetrics;
use crate::types::OrderBook;

use super::{HandleResult, WsHandler};

// ── Per-exchange byte walker ─────────────────────────────────────────────────

struct BinancePatterns {
    last_update_id: memmem::Finder<'static>,
    bids: memmem::Finder<'static>,
    asks: memmem::Finder<'static>,
}

fn patterns() -> &'static BinancePatterns {
    static P: OnceLock<BinancePatterns> = OnceLock::new();
    P.get_or_init(|| BinancePatterns {
        last_update_id: memmem::Finder::new(b"\"lastUpdateId\":"),
        bids: memmem::Finder::new(b"\"bids\":"),
        asks: memmem::Finder::new(b"\"asks\":"),
    })
}

/// Single forward pass: lastUpdateId → bids → asks.
/// Matches Binance depth20 wire format exactly.
fn walk(json: &str) -> Option<(u64, Levels<'_>, Levels<'_>)> {
    let p = patterns();
    let mut s = Scanner::new(json);
    let seq = if s.seek(&p.last_update_id).is_some() {
        s.read_u64()
    } else {
        s.pos = 0;
        0
    };
    s.seek(&p.bids)?;
    let bids = read_levels(&mut s)?;
    s.seek(&p.asks)?;
    let asks = read_levels(&mut s)?;
    Some((seq, bids, asks))
}

// ── WebSocket adapter ────────────────────────────────────────────────────────

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
        // Symbol is already lowercase from Config validation.
        format!("wss://stream.binance.com:9443/ws/{symbol}@depth20@100ms")
    }

    fn on_connected(&mut self) {
        self.last_seq = 0;
    }

    fn process_text(
        &mut self,
        text: &str,
        producer: &mut Producer<OrderBook>,
        metrics: &ExchangeMetrics,
    ) -> HandleResult {
        let t0 = Instant::now();
        let Some((seq, bids, asks)) = walk(text) else {
            metrics.errors.fetch_add(1, Relaxed);
            warn!(
                exchange = Self::NAME,
                payload_head = &text[..text.floor_char_boundary(200)],
                "parse error"
            );
            return HandleResult::Continue;
        };

        // Stale/duplicate detection: lastUpdateId should increase monotonically.
        if seq > 0 && self.last_seq > 0 && seq <= self.last_seq {
            warn!(
                exchange = Self::NAME,
                seq,
                last_seq = self.last_seq,
                "out-of-order update (stale or duplicate)"
            );
            metrics.errors.fetch_add(1, Relaxed);
            return HandleResult::Continue;
        }
        self.last_seq = seq;

        let Some(book) = super::build_book(Self::NAME, &bids, &asks, t0) else {
            metrics.errors.fetch_add(1, Relaxed);
            warn!(exchange = Self::NAME, "malformed level");
            return HandleResult::Continue;
        };
        metrics.decode_latency.record(t0.elapsed());
        metrics.messages.fetch_add(1, Relaxed);
        if !super::try_send_book(producer, book, metrics) {
            return HandleResult::Shutdown;
        }
        HandleResult::Continue
    }
}

/// Decode a Binance `depth20` JSON frame. For benchmarks.
#[must_use]
pub fn parse_depth_json(json: &str) -> Option<OrderBook> {
    let (_seq, bids, asks) = walk(json)?;
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

    // ── Walker tests ─────────────────────────────────────────────────────

    #[test]
    fn walk_happy_path() {
        let json = r#"{"lastUpdateId":123,"bids":[["0.06824","12.5"],["0.06823","8.3"],["0.06822","5.0"]],"asks":[["0.06825","10.0"],["0.06826","7.2"],["0.06827","3.5"]]}"#;
        let (seq, bids, asks) = walk(json).expect("valid JSON");
        assert_eq!(seq, 123);
        assert_eq!(bids.len(), 3);
        assert_eq!(asks.len(), 3);
        assert_eq!(bids[0], ["0.06824", "12.5"]);
        assert_eq!(bids[2], ["0.06822", "5.0"]);
        assert_eq!(asks[0], ["0.06825", "10.0"]);
        assert_eq!(asks[2], ["0.06827", "3.5"]);
    }

    #[test]
    fn walk_unknown_fields() {
        let json = r#"{"lastUpdateId":999,"E":1234567890,"bids":[["1.0","2.0"]],"extra":"value","asks":[["3.0","4.0"]],"trailing":true}"#;
        let (seq, bids, asks) = walk(json).expect("should skip unknown fields");
        assert_eq!(seq, 999);
        assert_eq!(bids.len(), 1);
        assert_eq!(asks.len(), 1);
        assert_eq!(bids[0], ["1.0", "2.0"]);
        assert_eq!(asks[0], ["3.0", "4.0"]);
    }

    #[test]
    fn walk_empty_levels() {
        let json = r#"{"bids":[],"asks":[]}"#;
        let (seq, bids, asks) = walk(json).expect("empty arrays are valid");
        assert_eq!(seq, 0);
        assert!(bids.is_empty());
        assert!(asks.is_empty());
    }

    #[test]
    fn walk_malformed_returns_none() {
        for (input, label) in [
            ("", "empty"),
            ("{", "unclosed brace"),
            (r#"{"bids": not_an_array}"#, "non-array bids"),
            ("null", "null literal"),
            (r#"{"bids":[]}"#, "missing asks"),
        ] {
            assert!(
                walk(input).is_none(),
                "expected None for {label}: {input:?}"
            );
        }
    }
}
