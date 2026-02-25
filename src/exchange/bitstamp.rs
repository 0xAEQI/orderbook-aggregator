//! Bitstamp order book WebSocket adapter.
//!
//! Connects to the Bitstamp WebSocket API and subscribes to the `order_book`
//! channel for the configured pair. Bitstamp sends full order book snapshots
//! on each update (top 100 levels).
//!
//! Two-phase zero-copy parse: the outer envelope uses `&RawValue` to skip
//! materializing the `data` field as `serde_json::Value` (dozens of allocs).
//! The inner book data borrows `[&str; 2]` from the original WS frame buffer.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Instant;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio::sync::broadcast;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::error::Result;
use crate::metrics::ExchangeMetrics;
use crate::types::{Level, OrderBook};

use super::Exchange;

const BITSTAMP_WS_URL: &str = "wss://ws.bitstamp.net";

pub struct Bitstamp {
    pub metrics: Arc<ExchangeMetrics>,
}

/// Outer envelope — `data` stays as raw JSON bytes, not materialized into Value.
#[derive(Deserialize)]
struct Envelope<'a> {
    #[serde(borrow)]
    event: &'a str,
    #[serde(borrow)]
    data: &'a RawValue,
}

/// Inner book data — borrows price/amount strings from the JSON buffer.
#[derive(Deserialize)]
struct BookData<'a> {
    #[serde(borrow)]
    bids: Vec<[&'a str; 2]>,
    #[serde(borrow)]
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

            match connect_async_tls_with_config(BITSTAMP_WS_URL, Some(ws_config), true, None).await
            {
                Ok((ws_stream, _)) => {
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
                                    Some(Ok(Message::Text(text))) => {
                                        match serde_json::from_str::<Envelope<'_>>(&text) {
                                            Ok(envelope) => {
                                                match envelope.event {
                                                    "data" => {
                                                        let t0 = Instant::now();
                                                        match serde_json::from_str::<BookData<'_>>(envelope.data.get()) {
                                                            Ok(book_data) => {
                                                                let book = parse_book(&book_data, t0);
                                                                self.metrics.decode_latency.record(t0.elapsed());
                                                                self.metrics.messages.fetch_add(1, Relaxed);
                                                                let _ = sender.send(book);
                                                            }
                                                            Err(e) => {
                                                                self.metrics.errors.fetch_add(1, Relaxed);
                                                                warn!(exchange = "bitstamp", error = %e, "parse data error");
                                                            }
                                                        }
                                                    }
                                                    "bts:subscription_succeeded" => {
                                                        info!(exchange = "bitstamp", "subscription confirmed");
                                                    }
                                                    "bts:error" => {
                                                        self.metrics.errors.fetch_add(1, Relaxed);
                                                        error!(exchange = "bitstamp", raw = %envelope.data, "server error");
                                                        break;
                                                    }
                                                    _ => {}
                                                }
                                            }
                                            Err(e) => {
                                                self.metrics.errors.fetch_add(1, Relaxed);
                                                warn!(exchange = "bitstamp", error = %e, "parse envelope error");
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
                Err(e) => {
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

/// Parse borrowed string slices directly into f64 — no intermediate String allocation.
fn parse_levels(exchange: &'static str, raw: &[[&str; 2]]) -> Vec<Level> {
    let mut levels = Vec::with_capacity(raw.len());
    for &[price, amount] in raw {
        if let (Ok(p), Ok(a)) = (price.parse(), amount.parse()) {
            levels.push(Level { exchange, price: p, amount: a });
        }
    }
    levels
}

fn parse_book(data: &BookData<'_>, received_at: Instant) -> OrderBook {
    OrderBook {
        exchange: "bitstamp",
        bids: parse_levels("bitstamp", &data.bids),
        asks: parse_levels("bitstamp", &data.asks),
        received_at,
    }
}
