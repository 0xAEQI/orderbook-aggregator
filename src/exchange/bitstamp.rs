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

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::error::Result;
use crate::metrics::ExchangeMetrics;
use crate::types::OrderBook;

use super::Exchange;

const BITSTAMP_WS_URL: &str = "wss://ws.bitstamp.net";

pub struct Bitstamp {
    pub metrics: Arc<ExchangeMetrics>,
}

/// Full message — single-pass parse. For non-data events, `data.bids` and
/// `data.asks` default to empty vecs (unknown fields in `data` are ignored).
#[derive(Deserialize)]
struct BitstampMessage<'a> {
    #[serde(borrow)]
    event: &'a str,
    #[serde(default)]
    data: BookFields<'a>,
}

#[derive(Deserialize, Default)]
struct BookFields<'a> {
    #[serde(borrow, default)]
    bids: Vec<[&'a str; 2]>,
    #[serde(borrow, default)]
    asks: Vec<[&'a str; 2]>,
}

impl Exchange for Bitstamp {
    async fn connect(
        &self,
        symbol: String,
        sender: broadcast::Sender<OrderBook>,
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
                                                        let _ = sender.send(book);
                                                    }
                                                    "bts:subscription_succeeded" => {
                                                        info!(exchange = "bitstamp", "subscription confirmed");
                                                    }
                                                    "bts:error" => {
                                                        self.metrics.errors.fetch_add(1, Relaxed);
                                                        error!(exchange = "bitstamp", "server error");
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
