//! Binance partial book depth WebSocket adapter.
//!
//! Connects to the partial depth stream which provides a snapshot of the top 20
//! price levels every 100ms — no sequence tracking or REST snapshot needed.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::error::Result;
use crate::metrics::Metrics;
use crate::types::{Level, OrderBook};

use super::Exchange;

pub struct Binance {
    pub metrics: Arc<Metrics>,
}

#[derive(Debug, Deserialize)]
struct DepthSnapshot {
    bids: Vec<[String; 2]>,
    asks: Vec<[String; 2]>,
}

impl Exchange for Binance {
    async fn connect(
        &self,
        symbol: String,
        sender: broadcast::Sender<OrderBook>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let url = format!(
            "wss://stream.binance.com:9443/ws/{}@depth20@100ms",
            symbol.to_lowercase()
        );
        let ws_config = super::ws_config();

        let mut backoff_ms = 1000u64;
        let max_backoff_ms = 30_000u64;

        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }

            info!(exchange = "binance", %url, "connecting");

            match connect_async_tls_with_config(&url, Some(ws_config), true, None).await {
                Ok((ws_stream, _)) => {
                    info!(exchange = "binance", "connected");
                    self.metrics.binance_connected.store(true, Relaxed);
                    backoff_ms = 1000;

                    let (_, mut read) = ws_stream.split();

                    loop {
                        tokio::select! {
                            _ = cancel.cancelled() => {
                                info!(exchange = "binance", "shutting down");
                                self.metrics.binance_connected.store(false, Relaxed);
                                return Ok(());
                            }
                            msg = read.next() => {
                                match msg {
                                    Some(Ok(msg)) => {
                                        if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
                                            match serde_json::from_str::<DepthSnapshot>(&text) {
                                                Ok(snapshot) => {
                                                    self.metrics.binance_msgs.fetch_add(1, Relaxed);
                                                    let book = parse_snapshot(snapshot);
                                                    let _ = sender.send(book);
                                                }
                                                Err(e) => {
                                                    self.metrics.binance_errors.fetch_add(1, Relaxed);
                                                    warn!(exchange = "binance", error = %e, "parse error");
                                                }
                                            }
                                        }
                                    }
                                    Some(Err(e)) => {
                                        self.metrics.binance_errors.fetch_add(1, Relaxed);
                                        warn!(exchange = "binance", error = %e, "ws error");
                                        break;
                                    }
                                    None => {
                                        warn!(exchange = "binance", "stream ended");
                                        break;
                                    }
                                }
                            }
                        }
                    }

                    self.metrics.binance_connected.store(false, Relaxed);
                }
                Err(e) => {
                    self.metrics.binance_errors.fetch_add(1, Relaxed);
                    error!(exchange = "binance", error = %e, "connection failed");
                }
            }

            if cancel.is_cancelled() {
                return Ok(());
            }

            // Exponential backoff with jitter.
            let jitter = rand::random::<u64>() % (backoff_ms / 2).max(1);
            let delay = backoff_ms + jitter;
            warn!(exchange = "binance", delay_ms = delay, "reconnecting");
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = tokio::time::sleep(std::time::Duration::from_millis(delay)) => {}
            }
            backoff_ms = (backoff_ms * 2).min(max_backoff_ms);
        }
    }
}

fn parse_snapshot(snapshot: DepthSnapshot) -> OrderBook {
    let bids = snapshot
        .bids
        .iter()
        .filter_map(|[price, qty]| {
            Some(Level {
                exchange: "binance",
                price: price.parse().ok()?,
                amount: qty.parse().ok()?,
            })
        })
        .collect();

    let asks = snapshot
        .asks
        .iter()
        .filter_map(|[price, qty]| {
            Some(Level {
                exchange: "binance",
                price: price.parse().ok()?,
                amount: qty.parse().ok()?,
            })
        })
        .collect();

    OrderBook {
        exchange: "binance",
        bids,
        asks,
    }
}
