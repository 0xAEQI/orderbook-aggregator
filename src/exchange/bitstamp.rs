//! Bitstamp order book WebSocket adapter.
//!
//! Connects to the Bitstamp WebSocket API and subscribes to the `order_book`
//! channel for the configured pair. Bitstamp sends full order book snapshots
//! on each update (top 100 levels).
//!
//! Uses a custom byte walker for zero-copy JSON parsing — scans directly to
//! `event` and `data.bids`/`data.asks`, borrowing `&str` slices from the WS
//! frame buffer. Keeps first 20 of 100 levels and skips the rest in a tight
//! loop — no serde, no simd-json, no `IgnoredAny` overhead.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use futures_util::{SinkExt, StreamExt};
use rtrb::Producer;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::error::Result;
use crate::json_walker::walk_bitstamp;
use crate::metrics::ExchangeMetrics;
use crate::types::OrderBook;

use super::Exchange;

const BITSTAMP_WS_URL: &str = "wss://ws.bitstamp.net";

/// Bitstamp `order_book` channel adapter (full snapshots, top 100 levels).
pub struct Bitstamp {
    pub metrics: Arc<ExchangeMetrics>,
}

impl Exchange for Bitstamp {
    async fn connect(
        &self,
        symbol: String,
        mut producer: Producer<OrderBook>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let channel = format!("order_book_{}", symbol.to_lowercase());
        let subscribe_msg = format!(
            r#"{{"event":"bts:subscribe","data":{{"channel":"{channel}"}}}}"#
        );
        let ws_config = super::ws_config();
        let mut backoff_ms = super::INITIAL_BACKOFF_MS;

        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }

            info!(exchange = "bitstamp", url = BITSTAMP_WS_URL, "connecting");

            let connect_fut = connect_async_tls_with_config(
                BITSTAMP_WS_URL,
                Some(ws_config),
                true, // TCP_NODELAY — disable Nagle's algorithm for lower latency
                None,
            );
            match tokio::time::timeout(super::CONNECT_TIMEOUT, connect_fut).await {
                Err(_) => {
                    self.metrics.errors.fetch_add(1, Relaxed);
                    error!(exchange = "bitstamp", "connection timed out");
                }
                Ok(Ok((ws_stream, _))) => {
                    info!(exchange = "bitstamp", "connected");
                    self.metrics.connected.store(true, Relaxed);
                    backoff_ms = super::INITIAL_BACKOFF_MS;

                    let (mut write, mut read) = ws_stream.split();

                    if let Err(e) = write.send(Message::Text(subscribe_msg.clone())).await {
                        self.metrics.errors.fetch_add(1, Relaxed);
                        error!(exchange = "bitstamp", error = %e, "subscribe failed");
                        self.metrics.connected.store(false, Relaxed);
                        continue;
                    }

                    info!(exchange = "bitstamp", %channel, "subscribe requested");

                    loop {
                        let text = tokio::select! {
                            _ = cancel.cancelled() => {
                                info!(exchange = "bitstamp", "shutting down");
                                self.metrics.connected.store(false, Relaxed);
                                return Ok(());
                            }
                            msg = read.next() => match msg {
                                Some(Ok(Message::Text(t))) => t,
                                Some(Ok(_)) => continue,
                                Some(Err(e)) => {
                                    self.metrics.errors.fetch_add(1, Relaxed);
                                    warn!(exchange = "bitstamp", error = %e, "ws error");
                                    break;
                                }
                                None => {
                                    warn!(exchange = "bitstamp", "stream ended");
                                    break;
                                }
                            }
                        };

                        let t0 = Instant::now();
                        let Some((event, bids, asks)) = walk_bitstamp(&text) else {
                            self.metrics.errors.fetch_add(1, Relaxed);
                            warn!(
                                exchange = "bitstamp",
                                payload_head = &text[..text.len().min(200)],
                                "parse error"
                            );
                            continue;
                        };
                        match event {
                            "data" => {
                                let Some(book) = super::build_book("bitstamp", &bids, &asks, t0) else {
                                    self.metrics.errors.fetch_add(1, Relaxed);
                                    warn!(exchange = "bitstamp", "malformed level");
                                    continue;
                                };
                                self.metrics.decode_latency.record(t0.elapsed());
                                self.metrics.messages.fetch_add(1, Relaxed);
                                if !super::try_send_book(&mut producer, book, "bitstamp") {
                                    return Ok(());
                                }
                            }
                            "bts:subscription_succeeded" => {
                                info!(exchange = "bitstamp", "subscription confirmed");
                            }
                            "bts:error" => {
                                self.metrics.errors.fetch_add(1, Relaxed);
                                let msg = crate::json_walker::extract_string(&text, b"\"message\":")
                                    .unwrap_or("unknown");
                                error!(exchange = "bitstamp", message = msg, "server error");
                                break;
                            }
                            _ => {}
                        }
                    }

                    self.metrics.connected.store(false, Relaxed);
                }
                Ok(Err(e)) => {
                    self.metrics.errors.fetch_add(1, Relaxed);
                    error!(exchange = "bitstamp", error = %e, "connection failed");
                }
            }

            if cancel.is_cancelled() {
                return Ok(());
            }

            if !super::backoff_sleep(&mut backoff_ms, super::MAX_BACKOFF_MS, "bitstamp", &cancel).await {
                return Ok(());
            }
        }
    }
}

/// Decode a Bitstamp `order_book` JSON message. Returns `None` for non-data events.
#[must_use]
pub fn parse_order_book_json(json: &str) -> Option<OrderBook> {
    let (event, bids, asks) = walk_bitstamp(json)?;
    if event != "data" {
        return None;
    }
    super::build_book("bitstamp", &bids, &asks, Instant::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FixedPoint, MAX_LEVELS};

    /// Realistic Bitstamp `order_book` message (truncated to 3 levels per side).
    const BITSTAMP_JSON: &str = r#"{
        "event": "data",
        "channel": "order_book_ethbtc",
        "data": {
            "timestamp": "1700000000",
            "microtimestamp": "1700000000000000",
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
        }
    }"#;

    #[test]
    fn parse_bitstamp_data_message() {
        let book = parse_order_book_json(BITSTAMP_JSON).expect("valid data message");

        assert_eq!(book.exchange, "bitstamp");
        assert_eq!(book.bids.len(), 3);
        assert_eq!(book.asks.len(), 3);

        assert_eq!(book.bids[0].price, FixedPoint::parse("0.06824000").unwrap());
        assert_eq!(book.bids[0].amount, FixedPoint::parse("12.50000000").unwrap());
        assert_eq!(book.asks[0].price, FixedPoint::parse("0.06825000").unwrap());
        assert!(book.bids.iter().all(|l| l.exchange == "bitstamp"));
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
        // Build a JSON array with 50 levels — more than MAX_LEVELS (20).
        let levels: Vec<String> = (0..50)
            .map(|i| format!("[\"{}.0\", \"1.0\"]", 100 + i))
            .collect();
        let json_array = format!("[{}]", levels.join(","));

        let json = format!(
            r#"{{"event":"data","data":{{"bids":{json_array},"asks":[]}}}}"#,
        );
        let book = parse_order_book_json(&json).expect("valid JSON");

        // Should cap at MAX_LEVELS, not error.
        assert_eq!(book.bids.len(), MAX_LEVELS);
        assert!(book.asks.is_empty());

        // Verify the first and last captured levels are correct.
        assert_eq!(book.bids[0].price, FixedPoint::parse("100.0").unwrap());
        assert_eq!(book.bids[MAX_LEVELS - 1].price, FixedPoint::parse("119.0").unwrap());
    }
}
