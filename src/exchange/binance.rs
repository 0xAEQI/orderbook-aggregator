//! Binance partial book depth WebSocket adapter.
//!
//! Runs on a dedicated OS thread with its own `current_thread` tokio runtime.
//! Pushes parsed [`OrderBook`] snapshots into an SPSC ring buffer — the push
//! is a single `store(Release)`, no CAS.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use futures_util::StreamExt;
use rtrb::Producer;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::error::Result;
use crate::json_walker::walk_binance;
use crate::metrics::ExchangeMetrics;
use crate::types::OrderBook;

use super::Exchange;

/// Binance partial book depth adapter (`depth20@100ms` stream).
pub struct Binance {
    pub metrics: Arc<ExchangeMetrics>,
}

impl Exchange for Binance {
    async fn connect(
        &self,
        symbol: String,
        mut producer: Producer<OrderBook>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let url = format!(
            "wss://stream.binance.com:9443/ws/{}@depth20@100ms",
            symbol.to_lowercase()
        );
        let ws_config = super::ws_config();

        let mut backoff_ms = super::INITIAL_BACKOFF_MS;

        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }

            info!(exchange = "binance", %url, "connecting");

            let connect_fut = connect_async_tls_with_config(
                &url,
                Some(ws_config),
                true, // TCP_NODELAY — disable Nagle's algorithm for lower latency
                None,
            );
            match tokio::time::timeout(super::CONNECT_TIMEOUT, connect_fut).await {
                Err(_) => {
                    self.metrics.errors.fetch_add(1, Relaxed);
                    error!(exchange = "binance", "connection timed out");
                }
                Ok(Ok((ws_stream, _))) => {
                    info!(exchange = "binance", "connected");
                    self.metrics.connected.store(true, Relaxed);
                    backoff_ms = super::INITIAL_BACKOFF_MS;
                    let mut last_seq: u64 = 0;

                    let (_write, mut read) = ws_stream.split();

                    loop {
                        let text = tokio::select! {
                            _ = cancel.cancelled() => {
                                info!(exchange = "binance", "shutting down");
                                self.metrics.connected.store(false, Relaxed);
                                return Ok(());
                            }
                            msg = read.next() => match msg {
                                Some(Ok(Message::Text(t))) => t,
                                Some(Ok(_)) => continue,
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
                        };

                        let t0 = Instant::now();
                        let Some((seq, bids, asks)) = walk_binance(&text) else {
                            self.metrics.errors.fetch_add(1, Relaxed);
                            warn!(
                                exchange = "binance",
                                payload_head = &text[..text.len().min(200)],
                                "parse error"
                            );
                            continue;
                        };

                        // Sequence gap detection: lastUpdateId should increase
                        // monotonically. A gap means we missed snapshots.
                        if seq > 0 && last_seq > 0 && seq <= last_seq {
                            warn!(
                                exchange = "binance",
                                seq, last_seq,
                                "out-of-order update (stale or duplicate)"
                            );
                            self.metrics.errors.fetch_add(1, Relaxed);
                            continue;
                        }
                        last_seq = seq;

                        let Some(book) = super::build_book("binance", &bids, &asks, t0) else {
                            self.metrics.errors.fetch_add(1, Relaxed);
                            warn!(exchange = "binance", "malformed level");
                            continue;
                        };
                        self.metrics.decode_latency.record(t0.elapsed());
                        self.metrics.messages.fetch_add(1, Relaxed);
                        if !super::try_send_book(&mut producer, book, "binance") {
                            return Ok(());
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

            self.metrics.reconnections.fetch_add(1, Relaxed);
            if !super::backoff_sleep(&mut backoff_ms, super::MAX_BACKOFF_MS, "binance", &cancel).await {
                return Ok(());
            }
        }
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
        assert_eq!(book.bids[0].amount, FixedPoint::parse("12.50000000").unwrap());
        assert_eq!(book.bids[2].price, FixedPoint::parse("0.06822000").unwrap());

        assert_eq!(book.asks[0].price, FixedPoint::parse("0.06825000").unwrap());
        assert_eq!(book.asks[0].amount, FixedPoint::parse("10.00000000").unwrap());
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
