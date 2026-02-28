//! Bitstamp order book WebSocket adapter.
//!
//! Connects to the Bitstamp WebSocket API and subscribes to the `order_book`
//! channel for the configured pair. Bitstamp sends full order book snapshots
//! on each update (top 100 levels).
//!
//! Uses a fused byte walker for zero-copy JSON parsing -- scans directly to
//! `event` and `data.bids`/`data.asks`, parsing quoted decimals into `FixedPoint`
//! in a single forward pass. No serde, no simd-json, no `IgnoredAny` overhead.

use std::sync::OnceLock;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use arrayvec::ArrayVec;
use memchr::memmem;
use rtrb::Producer;
use tracing::{error, info, warn};

use crate::BITSTAMP_ID;
use crate::json_walker::{Scanner, extract_string};
use crate::metrics::ExchangeMetrics;
use crate::types::{DEPTH, OrderBook, RawLevel};

use super::{HandleResult, WsHandler};

// ── Per-exchange byte walker ─────────────────────────────────────────────────

struct BitstampPatterns {
    bids: memmem::Finder<'static>,
    asks: memmem::Finder<'static>,
    event: memmem::Finder<'static>,
    microtimestamp: memmem::Finder<'static>,
}

fn patterns() -> &'static BitstampPatterns {
    static P: OnceLock<BitstampPatterns> = OnceLock::new();
    P.get_or_init(|| BitstampPatterns {
        bids: memmem::Finder::new(b"\"bids\":"),
        asks: memmem::Finder::new(b"\"asks\":"),
        event: memmem::Finder::new(b"\"event\":"),
        microtimestamp: memmem::Finder::new(b"\"microtimestamp\":"),
    })
}

/// Single forward pass matching Bitstamp's real wire format:
/// `{"data":{"bids":[...],"asks":[...]},...,"event":"data"}`
///
/// Fused walker: parses quoted decimals directly into `FixedPoint`, filters
/// zero-amount levels inline. Returns `(event, microtimestamp, bids, asks)`.
///
/// Data messages always contain `"microtimestamp":` -- its presence is used to
/// detect data events without rescanning for `"event":` at the end of the JSON.
/// Only non-data events (`subscription_succeeded`, error) trigger the rescan.
fn walk(
    json: &str,
) -> Option<(
    &str,
    u64,
    ArrayVec<RawLevel, DEPTH>,
    ArrayVec<RawLevel, DEPTH>,
)> {
    let p = patterns();
    let mut s = Scanner::new(json);
    // microtimestamp appears before bids in the data object. Its presence
    // reliably identifies data events -- non-data events have an empty data
    // object with no microtimestamp.
    let has_data = s.seek(&p.microtimestamp).is_some();
    let micro = if has_data {
        // Value is a quoted string: "1700000000000000"
        s.read_string()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0)
    } else {
        s.pos = 0;
        0
    };
    let bids = s.read_optional_raw_levels(&p.bids)?;
    let asks = s.read_optional_raw_levels(&p.asks)?;

    if has_data {
        // Data message -- skip the rescan for "event":. Saves ~200-500ns
        // (avoids rescanning the full 2-4KB message from position 0).
        Some(("data", micro, bids, asks))
    } else {
        // Non-data event (subscription_succeeded, error) -- rescan for event type.
        s.pos = 0;
        s.seek(&p.event)?;
        let event = s.read_string()?;
        Some((event, micro, bids, asks))
    }
}

// ── WebSocket adapter ────────────────────────────────────────────────────────

const BITSTAMP_WS_URL: &str = "wss://ws.bitstamp.net";

/// Bitstamp `order_book` channel adapter (full snapshots, top 100 levels).
#[derive(Default)]
pub struct BitstampHandler {
    last_micro: u64,
}

impl BitstampHandler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl WsHandler for BitstampHandler {
    const NAME: &'static str = "bitstamp";

    fn ws_url(&self, _symbol: &str) -> String {
        BITSTAMP_WS_URL.to_owned()
    }

    fn subscribe_message(&self, symbol: &str) -> Option<String> {
        // Symbol is already lowercase from Config validation.
        Some(format!(
            r#"{{"event":"bts:subscribe","data":{{"channel":"order_book_{symbol}"}}}}"#
        ))
    }

    fn on_connected(&mut self) {
        self.last_micro = 0;
    }

    fn process_text(
        &mut self,
        text: &str,
        producer: &mut Producer<OrderBook>,
        metrics: &ExchangeMetrics,
    ) -> HandleResult {
        let t0 = Instant::now();
        let Some((event, micro, bids, asks)) = walk(text) else {
            metrics.errors.fetch_add(1, Relaxed);
            warn!(
                exchange = Self::NAME,
                payload_head = &text[..text.floor_char_boundary(200)],
                "parse error"
            );
            return HandleResult::Continue;
        };
        match event {
            "data" => {
                // Stale/duplicate detection: microtimestamp should increase monotonically.
                if micro > 0 && self.last_micro > 0 && micro <= self.last_micro {
                    warn!(
                        exchange = Self::NAME,
                        micro,
                        last_micro = self.last_micro,
                        "out-of-order update (stale or duplicate)"
                    );
                    metrics.errors.fetch_add(1, Relaxed);
                    return HandleResult::Continue;
                }
                self.last_micro = micro;

                let book = OrderBook {
                    exchange_id: BITSTAMP_ID,
                    bids,
                    asks,
                    decode_start: t0,
                };
                metrics.decode_latency.record(t0.elapsed());
                metrics.messages.fetch_add(1, Relaxed);
                if !super::try_send_book(producer, book, metrics) {
                    return HandleResult::Shutdown;
                }
            }
            "bts:subscription_succeeded" => {
                info!(exchange = Self::NAME, "subscription confirmed");
            }
            "bts:error" => {
                metrics.errors.fetch_add(1, Relaxed);
                let msg = extract_string(text, b"\"message\":").unwrap_or("unknown");
                error!(exchange = Self::NAME, message = msg, "server error");
                return HandleResult::Reconnect;
            }
            _ => {}
        }
        HandleResult::Continue
    }
}

/// Decode a Bitstamp `order_book` JSON message. Returns `None` for non-data events.
#[must_use]
pub fn parse_order_book_json(json: &str) -> Option<OrderBook> {
    let (event, _micro, bids, asks) = walk(json)?;
    if event != "data" {
        return None;
    }
    Some(OrderBook {
        exchange_id: BITSTAMP_ID,
        bids,
        asks,
        decode_start: Instant::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::BITSTAMP_JSON_3L;
    use crate::types::{DEPTH, FixedPoint};

    #[test]
    fn parse_bitstamp_data_message() {
        let book = parse_order_book_json(BITSTAMP_JSON_3L).expect("valid data message");

        assert_eq!(book.exchange_id, BITSTAMP_ID);
        assert_eq!(book.bids.len(), 3);
        assert_eq!(book.asks.len(), 3);

        assert_eq!(book.bids[0].price, FixedPoint::parse("0.06824000").unwrap());
        assert_eq!(
            book.bids[0].amount,
            FixedPoint::parse("12.50000000").unwrap()
        );
        assert_eq!(book.asks[0].price, FixedPoint::parse("0.06825000").unwrap());
    }

    #[test]
    fn parse_bitstamp_non_data_event() {
        let json = r#"{
            "event": "bts:subscription_succeeded",
            "channel": "order_book_ethbtc",
            "data": {}
        }"#;
        // Non-data events return None from parse_order_book_json.
        assert!(parse_order_book_json(json).is_none());
    }

    #[test]
    fn level_capping_at_max() {
        // Build a JSON array with 50 levels -- more than DEPTH.
        let levels: Vec<String> = (0..50)
            .map(|i| format!("[\"{}.0\",\"1.0\"]", 100 + i))
            .collect();
        let json_array = format!("[{}]", levels.join(","));

        let json = format!(r#"{{"event":"data","data":{{"bids":{json_array},"asks":[]}}}}"#,);
        let book = parse_order_book_json(&json).expect("valid JSON");

        // Should cap at DEPTH, not error.
        assert_eq!(book.bids.len(), DEPTH);
        assert!(book.asks.is_empty());

        // Verify the first and last captured levels are correct.
        assert_eq!(book.bids[0].price, FixedPoint::parse("100.0").unwrap());
        assert_eq!(
            book.bids[DEPTH - 1].price,
            FixedPoint::parse(&format!("{}.0", 100 + DEPTH - 1)).unwrap()
        );
    }

    // ── Walker tests ─────────────────────────────────────────────────────

    #[test]
    fn walk_data_event() {
        // Compact JSON — matches production wire format (no whitespace in arrays).
        let json = r#"{"event":"data","channel":"order_book_ethbtc","data":{"timestamp":"1700000000","microtimestamp":"1700000000000000","bids":[["0.06824","12.5"],["0.06823","8.3"],["0.06822","5.0"]],"asks":[["0.06825","10.0"],["0.06826","7.2"],["0.06827","3.5"]]}}"#;
        let (event, micro, bids, asks) = walk(json).expect("valid JSON");
        assert_eq!(event, "data");
        assert_eq!(micro, 1_700_000_000_000_000);
        assert_eq!(bids.len(), 3);
        assert_eq!(asks.len(), 3);
        assert_eq!(bids[0].price, FixedPoint::parse("0.06824").unwrap());
        assert_eq!(bids[0].amount, FixedPoint::parse("12.5").unwrap());
        assert_eq!(asks[0].price, FixedPoint::parse("0.06825").unwrap());
    }

    #[test]
    fn walk_non_data_event() {
        let json = r#"{
            "event": "bts:subscription_succeeded",
            "channel": "order_book_ethbtc",
            "data": {}
        }"#;
        let (event, micro, bids, asks) = walk(json).expect("valid JSON");
        assert_eq!(event, "bts:subscription_succeeded");
        assert_eq!(micro, 0); // No microtimestamp in non-data events.
        assert!(bids.is_empty());
        assert!(asks.is_empty());
    }

    #[test]
    fn walk_level_capping() {
        let levels: Vec<String> = (0..50)
            .map(|i| format!("[\"{}.0\",\"1.0\"]", 100 + i))
            .collect();
        let json_array = levels.join(",");
        let json = format!(r#"{{"event":"data","data":{{"bids":[{json_array}],"asks":[]}}}}"#);
        let (event, _micro, bids, asks) = walk(&json).expect("valid JSON");
        assert_eq!(event, "data");
        assert_eq!(bids.len(), DEPTH);
        assert!(asks.is_empty());
        assert_eq!(bids[0].price, FixedPoint::parse("100.0").unwrap());
        assert_eq!(
            bids[DEPTH - 1].price,
            FixedPoint::parse(&format!("{}.0", 100 + DEPTH - 1)).unwrap()
        );
    }

    #[test]
    fn walk_extra_fields() {
        let json = r#"{"event":"data","channel":"order_book_ethbtc","data":{"timestamp":"1700000000","microtimestamp":"1700000000000000","bids":[["3.0","4.0"]],"asks":[["5.0","6.0"]]}}"#;
        let (event, _micro, bids, asks) = walk(json).expect("should skip extra data fields");
        assert_eq!(event, "data");
        assert_eq!(bids[0].price, FixedPoint::parse("3.0").unwrap());
        assert_eq!(asks[0].price, FixedPoint::parse("5.0").unwrap());
    }

    #[test]
    fn walk_real_field_order() {
        // Bitstamp's real API sends "data" BEFORE "event" in the JSON object.
        let json = r#"{"data":{"timestamp":"1772110554","microtimestamp":"1772110554110141","bids":[["0.03035870","1.35795438"],["0.03035566","0.08250000"]],"asks":[["0.03036200","10.00000000"],["0.03036500","5.00000000"]]},"channel":"order_book_ethbtc","event":"data"}"#;
        let (event, micro, bids, asks) = walk(json).expect("should handle data-before-event order");
        assert_eq!(event, "data");
        assert_eq!(micro, 1_772_110_554_110_141);
        assert_eq!(bids.len(), 2);
        assert_eq!(asks.len(), 2);
        assert_eq!(bids[0].price, FixedPoint::parse("0.03035870").unwrap());
        assert_eq!(asks[0].price, FixedPoint::parse("0.03036200").unwrap());
    }

    #[test]
    fn walk_malformed_returns_none() {
        for (input, label) in [
            ("", "empty"),
            ("{", "unclosed brace"),
            (r#"{"event": 123}"#, "event as number"),
            ("null", "null literal"),
        ] {
            assert!(
                walk(input).is_none(),
                "expected None for {label}: {input:?}"
            );
        }
    }
}
