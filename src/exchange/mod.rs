//! Exchange WebSocket adapters.
//!
//! Each adapter runs on a **dedicated OS thread** with its own single-threaded
//! tokio runtime -- isolates WS I/O from the main runtime, eliminates
//! work-stealing scheduler jitter. Both exchanges provide full snapshots
//! (not incremental diffs), so no local order book maintenance or sequence
//! reconciliation is required.
//!
//! Adapters push [`OrderBook`] snapshots into a per-exchange **SPSC ring buffer**
//! (`rtrb`). The ring uses only `store(Release)` / `load(Acquire)` -- no CAS,
//! no contention with the merger thread.
//!
//! Connections use `TCP_NODELAY` and `write_buffer_size: 0` for immediate frame
//! flushing.

pub mod binance;
pub mod bitstamp;

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use arrayvec::ArrayVec;
use futures_util::{SinkExt, StreamExt};
use rtrb::Producer;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::error::Result;
use crate::metrics::ExchangeMetrics;
use crate::types::{FixedPoint, Level, MAX_LEVELS, OrderBook};

/// Connection timeout for WebSocket handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum reconnection backoff (shared across all adapters).
const MAX_BACKOFF_MS: u64 = 30_000;

/// Initial reconnection backoff before exponential increase.
const INITIAL_BACKOFF_MS: u64 = 1_000;

/// Result of processing a single WebSocket text message.
pub enum HandleResult {
    /// Message processed, keep reading.
    Continue,
    /// Protocol error, reconnect.
    Reconnect,
    /// Consumer dropped or unrecoverable, stop entirely.
    Shutdown,
}

/// Exchange-specific behavior, called by the shared reconnection loop.
pub trait WsHandler: Send + 'static {
    /// Short lowercase name for logging and thread naming (e.g. `"binance"`).
    const NAME: &'static str;

    /// Build the WebSocket URL for the given symbol.
    fn ws_url(&self, symbol: &str) -> String;

    /// Optional subscribe message to send immediately after connecting.
    fn subscribe_message(&self, symbol: &str) -> Option<String> {
        let _ = symbol;
        None
    }

    /// Process a single `Message::Text` frame. Called on the hot path.
    fn process_text(
        &mut self,
        text: &str,
        producer: &mut Producer<OrderBook>,
        metrics: &ExchangeMetrics,
    ) -> HandleResult;
}

/// WebSocket config: `write_buffer_size: 0` for immediate frame flushing.
#[must_use]
pub fn ws_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
        write_buffer_size: 0, // Flush every frame immediately.
        ..Default::default()
    }
}

/// `[price, qty]` string pairs → `ArrayVec<Level>`. Returns `None` on any malformed pair.
#[inline]
fn parse_levels(exchange: &'static str, raw: &[[&str; 2]]) -> Option<ArrayVec<Level, MAX_LEVELS>> {
    let mut levels = ArrayVec::new();
    for &[price, amount] in raw {
        let p = FixedPoint::parse(price)?;
        let a = FixedPoint::parse(amount)?;
        if levels
            .try_push(Level {
                exchange,
                price: p,
                amount: a,
            })
            .is_err()
        {
            break;
        }
    }
    Some(levels)
}

/// Parse walker output into an `OrderBook`. Returns `None` if any level is malformed.
#[inline]
#[must_use]
pub fn build_book(
    exchange: &'static str,
    bids: &[[&str; 2]],
    asks: &[[&str; 2]],
    decode_start: Instant,
) -> Option<OrderBook> {
    Some(OrderBook {
        exchange,
        bids: parse_levels(exchange, bids)?,
        asks: parse_levels(exchange, asks)?,
        decode_start,
    })
}

/// Push book into SPSC ring. Drops on full (stale data is correct to drop for
/// order books -- next snapshot supersedes). Returns `false` if consumer dropped.
#[inline]
pub fn try_send_book(
    producer: &mut Producer<OrderBook>,
    book: OrderBook,
    exchange: &'static str,
) -> bool {
    if producer.is_abandoned() {
        tracing::warn!(exchange, "consumer dropped, stopping");
        return false;
    }
    match producer.push(book) {
        Ok(()) => true,
        Err(rtrb::PushError::Full(_)) => {
            tracing::warn!(exchange, "ring full, dropping snapshot");
            true
        }
    }
}

/// Exponential backoff with jitter. Returns `false` on cancellation.
pub async fn backoff_sleep(
    backoff_ms: &mut u64,
    max_ms: u64,
    exchange: &'static str,
    cancel: &CancellationToken,
) -> bool {
    let jitter = fastrand::u64(0..(*backoff_ms / 2).max(1));
    let delay = *backoff_ms + jitter;
    tracing::warn!(exchange, delay_ms = delay, "reconnecting");
    tokio::select! {
        _ = cancel.cancelled() => return false,
        _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
    }
    *backoff_ms = (*backoff_ms * 2).min(max_ms);
    true
}

/// Shared reconnection loop for all exchange adapters. Handles connection,
/// optional subscribe, inner read loop, metrics, backoff, and cancellation.
pub async fn run_ws_loop<H: WsHandler>(
    mut handler: H,
    symbol: String,
    mut producer: Producer<OrderBook>,
    metrics: Arc<ExchangeMetrics>,
    cancel: CancellationToken,
) -> Result<()> {
    let url = handler.ws_url(&symbol);
    let ws_config = ws_config();
    let mut backoff_ms = INITIAL_BACKOFF_MS;

    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }

        info!(exchange = H::NAME, %url, "connecting");

        let connect_fut =
            connect_async_tls_with_config(&url, Some(ws_config), true, None);
        match tokio::time::timeout(CONNECT_TIMEOUT, connect_fut).await {
            Err(_) => {
                metrics.errors.fetch_add(1, Relaxed);
                error!(exchange = H::NAME, "connection timed out");
            }
            Ok(Ok((ws_stream, _))) => {
                info!(exchange = H::NAME, "connected");
                metrics.connected.store(true, Relaxed);
                backoff_ms = INITIAL_BACKOFF_MS;

                let (mut write, mut read) = ws_stream.split();

                // Send optional subscribe message.
                if let Some(msg) = handler.subscribe_message(&symbol)
                    && let Err(e) = write.send(Message::Text(msg)).await
                {
                    metrics.errors.fetch_add(1, Relaxed);
                    error!(exchange = H::NAME, error = %e, "subscribe failed");
                    metrics.connected.store(false, Relaxed);
                    // Fall through to backoff + reconnect.
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                    metrics.reconnections.fetch_add(1, Relaxed);
                    if !backoff_sleep(&mut backoff_ms, MAX_BACKOFF_MS, H::NAME, &cancel).await
                    {
                        return Ok(());
                    }
                    continue;
                }

                // Inner read loop.
                loop {
                    let text = tokio::select! {
                        _ = cancel.cancelled() => {
                            info!(exchange = H::NAME, "shutting down");
                            metrics.connected.store(false, Relaxed);
                            return Ok(());
                        }
                        msg = read.next() => match msg {
                            Some(Ok(Message::Text(t))) => t,
                            Some(Ok(_)) => continue,
                            Some(Err(e)) => {
                                metrics.errors.fetch_add(1, Relaxed);
                                warn!(exchange = H::NAME, error = %e, "ws error");
                                break;
                            }
                            None => {
                                warn!(exchange = H::NAME, "stream ended");
                                break;
                            }
                        }
                    };

                    match handler.process_text(&text, &mut producer, &metrics) {
                        HandleResult::Continue => {}
                        HandleResult::Reconnect => break,
                        HandleResult::Shutdown => return Ok(()),
                    }
                }

                metrics.connected.store(false, Relaxed);
            }
            Ok(Err(e)) => {
                metrics.errors.fetch_add(1, Relaxed);
                error!(exchange = H::NAME, error = %e, "connection failed");
            }
        }

        if cancel.is_cancelled() {
            return Ok(());
        }

        metrics.reconnections.fetch_add(1, Relaxed);
        if !backoff_sleep(&mut backoff_ms, MAX_BACKOFF_MS, H::NAME, &cancel).await {
            return Ok(());
        }
    }
}

/// Spawn an exchange adapter on a dedicated OS thread with its own single-threaded
/// tokio runtime. Isolates WS I/O from the main runtime.
pub fn spawn_exchange<H: WsHandler>(
    handler: H,
    symbol: String,
    producer: Producer<OrderBook>,
    metrics: Arc<ExchangeMetrics>,
    cancel: CancellationToken,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("ws-{}", H::NAME))
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap_or_else(|e| panic!("{} runtime: {e}", H::NAME));
            rt.block_on(async {
                if let Err(e) = run_ws_loop(handler, symbol, producer, metrics, cancel).await {
                    tracing::error!(exchange = H::NAME, error = %e, "fatal error");
                }
            });
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FixedPoint;

    fn test_book(exchange: &'static str) -> OrderBook {
        let mut bids = ArrayVec::new();
        bids.push(Level {
            exchange,
            price: FixedPoint::from_f64(100.0),
            amount: FixedPoint::from_f64(1.0),
        });
        let mut asks = ArrayVec::new();
        asks.push(Level {
            exchange,
            price: FixedPoint::from_f64(101.0),
            amount: FixedPoint::from_f64(1.0),
        });
        OrderBook {
            exchange,
            bids,
            asks,
            decode_start: Instant::now(),
        }
    }

    #[test]
    fn try_send_ring_full_drops() {
        // 1-slot ring: push one to fill it, then push another -- should drop, not error.
        let (mut producer, _consumer) = rtrb::RingBuffer::new(1);
        assert!(try_send_book(&mut producer, test_book("a"), "a"));
        // Ring is now full -- next push drops the book but returns true.
        assert!(try_send_book(&mut producer, test_book("a"), "a"));
    }

    #[test]
    fn try_send_abandoned_returns_false() {
        let (mut producer, consumer) = rtrb::RingBuffer::new(4);
        drop(consumer); // Abandon the consumer.
        assert!(!try_send_book(&mut producer, test_book("a"), "a"));
    }
}
