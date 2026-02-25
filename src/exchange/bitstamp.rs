//! Bitstamp order book WebSocket adapter.
//!
//! Connects to the Bitstamp WebSocket API and subscribes to the `order_book`
//! channel for the configured pair. Bitstamp sends full order book snapshots
//! on each update (top 100 levels).
//!
//! Single-pass simd-json parse: the entire message (envelope + book data) is
//! deserialized in one SIMD-accelerated pass. Non-data events get empty
//! `bids`/`asks` via `#[serde(default)]` — no separate envelope parse needed
//! since simd-json's vectorized stage 1 scans the whole buffer regardless.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use arrayvec::ArrayVec;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde::de::{Deserializer, SeqAccess, Visitor};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::error::Result;
use crate::metrics::ExchangeMetrics;
use crate::types::{MAX_LEVELS, OrderBook};

use super::Exchange;

const BITSTAMP_WS_URL: &str = "wss://ws.bitstamp.net";

/// Bitstamp `order_book` channel adapter (full snapshots, top 100 levels).
pub struct Bitstamp {
    pub metrics: Arc<ExchangeMetrics>,
}

/// Full message — single-pass parse. For non-data events, `data.bids` and
/// `data.asks` default to empty `ArrayVec`s (unknown fields in `data` are ignored).
#[derive(Deserialize)]
struct BitstampMessage<'a> {
    #[serde(borrow)]
    event: &'a str,
    #[serde(default)]
    data: BookFields<'a>,
}

/// Bitstamp sends up to 100 levels per side. We deserialize only `MAX_LEVELS`
/// (20) per side and discard the rest — avoids processing 80 unused levels
/// and keeps the future small enough to avoid `Box::pin`.
#[derive(Deserialize, Default)]
struct BookFields<'a> {
    #[serde(borrow, default, deserialize_with = "deserialize_capped_levels")]
    bids: ArrayVec<[&'a str; 2], MAX_LEVELS>,
    #[serde(borrow, default, deserialize_with = "deserialize_capped_levels")]
    asks: ArrayVec<[&'a str; 2], MAX_LEVELS>,
    /// Error message from Bitstamp (present on `bts:error` events).
    #[serde(borrow, default)]
    message: Option<&'a str>,
}

/// Deserialize a JSON array of `[price, qty]` pairs, keeping only the first
/// `MAX_LEVELS` and discarding the rest. Bitstamp sends 100 levels but the
/// merger only needs 20 — this avoids creating 80 unused serde objects.
fn deserialize_capped_levels<'de, D>(
    deserializer: D,
) -> std::result::Result<ArrayVec<[&'de str; 2], MAX_LEVELS>, D::Error>
where
    D: Deserializer<'de>,
{
    struct CappedVisitor;

    impl<'de> Visitor<'de> for CappedVisitor {
        type Value = ArrayVec<[&'de str; 2], MAX_LEVELS>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "an array of [price, qty] pairs")
        }

        fn visit_seq<A: SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut levels = ArrayVec::new();
            while levels.len() < MAX_LEVELS {
                match seq.next_element()? {
                    Some(pair) => levels.push(pair),
                    None => return Ok(levels),
                }
            }
            // Drain remaining elements without storing them.
            while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(levels)
        }
    }

    deserializer.deserialize_seq(CappedVisitor)
}

impl Exchange for Bitstamp {
    async fn connect(
        &self,
        symbol: String,
        sender: mpsc::Sender<OrderBook>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let channel = format!("order_book_{}", symbol.to_lowercase());
        let ws_config = super::ws_config();
        let mut backoff_ms = 1000u64;
        let max_backoff_ms = 30_000u64;

        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }

            info!(exchange = "bitstamp", url = BITSTAMP_WS_URL, "connecting");

            let connect_fut =
                connect_async_tls_with_config(BITSTAMP_WS_URL, Some(ws_config), true, None);
            match tokio::time::timeout(std::time::Duration::from_secs(10), connect_fut).await {
                Err(_) => {
                    self.metrics.errors.fetch_add(1, Relaxed);
                    error!(exchange = "bitstamp", "connection timed out");
                }
                Ok(Ok((ws_stream, _))) => {
                    info!(exchange = "bitstamp", "connected");
                    self.metrics.connected.store(true, Relaxed);
                    backoff_ms = 1000;

                    let (mut write, mut read) = ws_stream.split();

                    // Subscribe to order book channel.
                    let subscribe = json!({
                        "event": "bts:subscribe",
                        "data": {
                            "channel": &channel
                        }
                    });

                    if let Err(e) = write.send(Message::Text(subscribe.to_string())).await {
                        self.metrics.errors.fetch_add(1, Relaxed);
                        error!(exchange = "bitstamp", error = %e, "subscribe failed");
                        self.metrics.connected.store(false, Relaxed);
                        continue;
                    }

                    info!(exchange = "bitstamp", %channel, "subscribed");

                    loop {
                        tokio::select! {
                            _ = cancel.cancelled() => {
                                info!(exchange = "bitstamp", "shutting down");
                                self.metrics.connected.store(false, Relaxed);
                                return Ok(());
                            }
                            msg = read.next() => {
                                match msg {
                                    Some(Ok(Message::Text(mut text))) => {
                                        let t0 = Instant::now();
                                        // SAFETY: `simd_json::from_str` requires `&mut str` for in-place
                                        // SIMD tokenization. `text` is a heap-allocated String from
                                        // a WS text frame — valid UTF-8, properly aligned, and not
                                        // accessed after this call (simd-json may mutate the buffer).
                                        #[allow(unsafe_code)]
                                        let parsed = unsafe { simd_json::from_str::<BitstampMessage<'_>>(&mut text) };
                                        match parsed {
                                            Ok(bts_msg) => {
                                                match bts_msg.event {
                                                    "data" => {
                                                        let book = parse_book(&bts_msg.data, t0);
                                                        self.metrics.decode_latency.record(t0.elapsed());
                                                        self.metrics.messages.fetch_add(1, Relaxed);
                                                        match sender.try_send(book) {
                                                            Ok(()) => {}
                                                            Err(mpsc::error::TrySendError::Full(_)) => {
                                                                warn!(exchange = "bitstamp", "channel full, dropping snapshot");
                                                            }
                                                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                                                warn!(exchange = "bitstamp", "channel closed, stopping");
                                                                return Ok(());
                                                            }
                                                        }
                                                    }
                                                    "bts:subscription_succeeded" => {
                                                        info!(exchange = "bitstamp", "subscription confirmed");
                                                    }
                                                    "bts:error" => {
                                                        self.metrics.errors.fetch_add(1, Relaxed);
                                                        error!(
                                                            exchange = "bitstamp",
                                                            message = bts_msg.data.message.unwrap_or("unknown"),
                                                            "server error"
                                                        );
                                                        break;
                                                    }
                                                    _ => {}
                                                }
                                            }
                                            Err(e) => {
                                                self.metrics.errors.fetch_add(1, Relaxed);
                                                warn!(exchange = "bitstamp", error = %e, "parse error");
                                            }
                                        }
                                    }
                                    Some(Ok(_)) => {} // Ping/Pong/Binary — ignore.
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
                            }
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

            // Exponential backoff with jitter.
            let jitter = rand::random::<u64>() % (backoff_ms / 2).max(1);
            let delay = backoff_ms + jitter;
            warn!(exchange = "bitstamp", delay_ms = delay, "reconnecting");
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = tokio::time::sleep(std::time::Duration::from_millis(delay)) => {}
            }
            backoff_ms = (backoff_ms * 2).min(max_backoff_ms);
        }
    }
}

#[inline]
fn parse_book(data: &BookFields<'_>, received_at: Instant) -> OrderBook {
    OrderBook {
        exchange: "bitstamp",
        bids: super::parse_levels("bitstamp", &data.bids),
        asks: super::parse_levels("bitstamp", &data.asks),
        received_at,
    }
}

/// Parse a raw Bitstamp `order_book` JSON message into an [`OrderBook`].
///
/// Wraps the internal simd-json + `fast-float` parse pipeline so benchmarks
/// can measure decode performance without accessing private types.
/// Returns `None` for non-data events (e.g. subscription confirmations).
#[allow(unsafe_code)]
pub fn parse_order_book_json(json: &mut str) -> Option<OrderBook> {
    let msg = unsafe { simd_json::from_str::<BitstampMessage<'_>>(json) }.ok()?;
    if msg.event != "data" {
        return None;
    }
    Some(parse_book(&msg.data, Instant::now()))
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::types::MAX_LEVELS;

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
    fn test_parse_bitstamp_data_message() {
        let mut json = BITSTAMP_JSON.to_string();
        #[allow(unsafe_code)]
        let msg: BitstampMessage<'_> =
            unsafe { simd_json::from_str(&mut json) }.expect("valid JSON");

        assert_eq!(msg.event, "data");

        let book = parse_book(&msg.data, Instant::now());
        assert_eq!(book.exchange, "bitstamp");
        assert_eq!(book.bids.len(), 3);
        assert_eq!(book.asks.len(), 3);

        assert_eq!(book.bids[0].price, 0.06824);
        assert_eq!(book.bids[0].amount, 12.5);
        assert_eq!(book.asks[0].price, 0.06825);
        assert!(book.bids.iter().all(|l| l.exchange == "bitstamp"));
    }

    #[test]
    fn test_parse_bitstamp_non_data_event() {
        let mut json = r#"{
            "event": "bts:subscription_succeeded",
            "channel": "order_book_ethbtc",
            "data": {}
        }"#
        .to_string();
        #[allow(unsafe_code)]
        let msg: BitstampMessage<'_> =
            unsafe { simd_json::from_str(&mut json) }.expect("valid JSON");

        assert_eq!(msg.event, "bts:subscription_succeeded");
        // Non-data events get empty bids/asks via #[serde(default)].
        assert!(msg.data.bids.is_empty());
        assert!(msg.data.asks.is_empty());
    }

    #[test]
    fn test_deserialize_capped_levels_caps_at_max() {
        // Build a JSON array with 50 levels — more than MAX_LEVELS (20).
        let levels: Vec<String> = (0..50)
            .map(|i| format!("[\"{}.0\", \"1.0\"]", 100 + i))
            .collect();
        let json_array = format!("[{}]", levels.join(","));

        let mut json = format!(
            r#"{{"event":"data","data":{{"bids":{json_array},"asks":[]}}}}"#,
        );
        #[allow(unsafe_code)]
        let msg: BitstampMessage<'_> =
            unsafe { simd_json::from_str(&mut json) }.expect("valid JSON");

        // Should cap at MAX_LEVELS, not error.
        assert_eq!(msg.data.bids.len(), MAX_LEVELS);
        assert!(msg.data.asks.is_empty());

        // Verify the first and last captured levels are correct.
        assert_eq!(msg.data.bids[0], ["100.0", "1.0"]);
        assert_eq!(msg.data.bids[MAX_LEVELS - 1], ["119.0", "1.0"]);
    }
}
