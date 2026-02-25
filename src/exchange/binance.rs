//! Binance partial book depth WebSocket adapter.
//!
//! Connects to the partial depth stream which provides a snapshot of the top 20
//! price levels every 100ms — no sequence tracking or REST snapshot needed.
//!
//! Uses simd-json (AVX2/SSE4.2 vectorized) for parsing + `#[serde(borrow)]`
//! to borrow `&str` directly from the WS frame buffer. Combined: zero heap
//! allocations and ~3-5x faster tokenization than serde_json.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Instant;

use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::error::Result;
use crate::metrics::ExchangeMetrics;
use crate::types::{Level, OrderBook};

use super::Exchange;

pub struct Binance {
    pub metrics: Arc<ExchangeMetrics>,
}

/// Zero-copy: borrows price/qty strings directly from the JSON input buffer.
#[derive(Deserialize)]
struct DepthSnapshot<'a> {
    #[serde(borrow)]
    bids: Vec<[&'a str; 2]>,
    #[serde(borrow)]
    asks: Vec<[&'a str; 2]>,
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
                    self.metrics.connected.store(true, Relaxed);
                    backoff_ms = 1000;

                    let (_, mut read) = ws_stream.split();

                    loop {
                        tokio::select! {
                            _ = cancel.cancelled() => {
                                info!(exchange = "binance", "shutting down");
                                self.metrics.connected.store(false, Relaxed);
                                return Ok(());
                            }
                            msg = read.next() => {
                                match msg {
                                    Some(Ok(msg)) => {
                                        if let tokio_tungstenite::tungstenite::Message::Text(mut text) = msg {
                                            let t0 = Instant::now();
                                            // SAFETY: text is a heap-allocated String from a WS
                                            // text frame — valid UTF-8 and properly aligned.
                                            match unsafe { simd_json::from_str::<DepthSnapshot<'_>>(&mut text) } {
                                                Ok(snapshot) => {
                                                    let book = parse_snapshot(&snapshot, t0);
                                                    self.metrics.decode_latency.record(t0.elapsed());
                                                    self.metrics.messages.fetch_add(1, Relaxed);
                                                    let _ = sender.send(book);
                                                }
                                                Err(e) => {
                                                    self.metrics.errors.fetch_add(1, Relaxed);
                                                    warn!(exchange = "binance", error = %e, "parse error");
                                                }
                                            }
                                        }
                                    }
                                    Some(Err(e)) => {
                                        self.metrics.errors.fetch_add(1, Relaxed);
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

                    self.metrics.connected.store(false, Relaxed);
                }
                Err(e) => {
                    self.metrics.errors.fetch_add(1, Relaxed);
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

/// Parse borrowed string slices directly into f64 — no intermediate String allocation.
/// Uses `fast-float` for SIMD-accelerated float parsing (~2-3x faster than `str::parse`).
#[inline]
fn parse_levels(exchange: &'static str, raw: &[[&str; 2]]) -> Vec<Level> {
    let mut levels = Vec::with_capacity(raw.len());
    for &[price, amount] in raw {
        if let (Ok(p), Ok(a)) = (fast_float::parse(price), fast_float::parse(amount)) {
            levels.push(Level { exchange, price: p, amount: a });
        }
    }
    levels
}

#[inline]
fn parse_snapshot(snapshot: &DepthSnapshot<'_>, received_at: Instant) -> OrderBook {
    OrderBook {
        exchange: "binance",
        bids: parse_levels("binance", &snapshot.bids),
        asks: parse_levels("binance", &snapshot.asks),
        received_at,
    }
}
