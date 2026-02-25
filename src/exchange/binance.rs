//! Binance partial book depth WebSocket adapter.
//!
//! Connects to the partial depth stream which provides a snapshot of the top 20
//! price levels every 100ms — no sequence tracking or REST snapshot needed.
//!
//! Uses simd-json (AVX2/SSE4.2 vectorized) for parsing + `#[serde(borrow)]`
//! to borrow `&str` directly from the WS frame buffer. Combined: zero heap
//! allocations and ~3-5x faster tokenization than `serde_json`.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use arrayvec::ArrayVec;
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::error::Result;
use crate::metrics::ExchangeMetrics;
use crate::types::OrderBook;

use super::Exchange;

pub struct Binance {
    pub metrics: Arc<ExchangeMetrics>,
}

/// Zero-copy: borrows price/qty strings directly from the JSON input buffer.
/// Binance depth20 always sends exactly 20 levels — `ArrayVec` avoids heap allocation.
#[derive(Deserialize)]
struct DepthSnapshot<'a> {
    #[serde(borrow)]
    bids: ArrayVec<[&'a str; 2], 20>,
    #[serde(borrow)]
    asks: ArrayVec<[&'a str; 2], 20>,
}

impl Exchange for Binance {
    async fn connect(
        &self,
        symbol: String,
        sender: mpsc::Sender<OrderBook>,
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

            let connect_fut = connect_async_tls_with_config(&url, Some(ws_config), true, None);
            match tokio::time::timeout(std::time::Duration::from_secs(10), connect_fut).await {
                Err(_) => {
                    self.metrics.errors.fetch_add(1, Relaxed);
                    error!(exchange = "binance", "connection timed out");
                }
                Ok(Ok((ws_stream, _))) => {
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
                                            // SAFETY: `simd_json::from_str` requires `&mut str` for in-place
                                            // SIMD tokenization. `text` is a heap-allocated String from
                                            // a WS text frame — valid UTF-8, properly aligned, and not
                                            // accessed after this call (simd-json may mutate the buffer).
                                            #[allow(unsafe_code)]
                                            let parsed = unsafe { simd_json::from_str::<DepthSnapshot<'_>>(&mut text) };
                                            match parsed {
                                                Ok(snapshot) => {
                                                    let book = parse_snapshot(&snapshot, t0);
                                                    self.metrics.decode_latency.record(t0.elapsed());
                                                    self.metrics.messages.fetch_add(1, Relaxed);
                                                    match sender.try_send(book) {
                                                        Ok(()) => {}
                                                        Err(mpsc::error::TrySendError::Full(_)) => {
                                                            warn!(exchange = "binance", "channel full, dropping snapshot");
                                                        }
                                                        Err(mpsc::error::TrySendError::Closed(_)) => {
                                                            warn!(exchange = "binance", "channel closed, stopping");
                                                            return Ok(());
                                                        }
                                                    }
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
                Ok(Err(e)) => {
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

#[inline]
fn parse_snapshot(snapshot: &DepthSnapshot<'_>, received_at: Instant) -> OrderBook {
    OrderBook {
        exchange: "binance",
        bids: super::parse_levels("binance", &snapshot.bids),
        asks: super::parse_levels("binance", &snapshot.asks),
        received_at,
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    /// Realistic Binance depth20 payload (truncated to 3 levels per side for clarity).
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
    fn test_parse_binance_snapshot() {
        let mut json = BINANCE_JSON.to_string();
        // SAFETY: owned String, valid UTF-8, not accessed after mutation.
        #[allow(unsafe_code)]
        let snapshot: DepthSnapshot<'_> =
            unsafe { simd_json::from_str(&mut json) }.expect("valid JSON");
        let book = parse_snapshot(&snapshot, Instant::now());

        assert_eq!(book.exchange, "binance");
        assert_eq!(book.bids.len(), 3);
        assert_eq!(book.asks.len(), 3);

        // Bids: highest first (exchange sends them sorted).
        assert_eq!(book.bids[0].price, 0.06824);
        assert_eq!(book.bids[0].amount, 12.5);
        assert_eq!(book.bids[2].price, 0.06822);

        // Asks: lowest first.
        assert_eq!(book.asks[0].price, 0.06825);
        assert_eq!(book.asks[0].amount, 10.0);
        assert_eq!(book.asks[2].price, 0.06827);

        // All levels attributed to binance.
        assert!(book.bids.iter().all(|l| l.exchange == "binance"));
        assert!(book.asks.iter().all(|l| l.exchange == "binance"));
    }

    #[test]
    fn test_parse_binance_ignores_unknown_fields() {
        // Binance may add fields; serde should ignore them.
        let mut json = r#"{
            "lastUpdateId": 999,
            "E": 1234567890,
            "bids": [["1.0", "2.0"]],
            "asks": [["3.0", "4.0"]]
        }"#
        .to_string();
        #[allow(unsafe_code)]
        let snapshot: DepthSnapshot<'_> =
            unsafe { simd_json::from_str(&mut json) }.expect("should ignore unknown fields");
        assert_eq!(snapshot.bids.len(), 1);
        assert_eq!(snapshot.asks.len(), 1);
    }
}
