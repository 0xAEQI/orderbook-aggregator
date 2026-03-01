//! Exchange WebSocket adapters.
//!
//! Each adapter runs on a **dedicated OS thread** with its own single-threaded
//! tokio runtime -- isolates WS I/O from the main runtime, eliminates
//! work-stealing scheduler jitter. Both exchanges provide full snapshots
//! (not incremental diffs), so no local order book maintenance or sequence
//! reconciliation is required.
//!
//! Adapters push [`OrderBook`] snapshots into a per-exchange **SPSC slot**
//! (`atomic_slot`). The slot uses a single atomic swap per operation -- the
//! producer always overwrites the latest value, the consumer always gets the
//! freshest snapshot. Stale intermediates are silently dropped (correct for
//! full-snapshot order book data where each update supersedes the previous).
//!
//! Connections use `write_buffer_size: 0` for immediate frame flushing.
//! `TCP_NODELAY` is enabled via `connect_async_tls_with_config`'s
//! `disable_nagle` parameter -- disables Nagle's algorithm for lower latency.

pub mod binance;
pub mod bitstamp;

use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::metrics::ExchangeMetrics;
use crate::atomic_slot::{SlotReceiver, SlotSender};
use crate::types::OrderBook;

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
    /// Server error or protocol violation -- reconnect.
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

    /// Called after a new WebSocket connection is established (before reading).
    /// Use to reset sequence tracking state (e.g. `lastUpdateId`, `microtimestamp`).
    fn on_connected(&mut self) {}

    /// Process a single `Message::Text` frame. Called on the hot path.
    fn process_text(
        &mut self,
        text: &str,
        sender: &SlotSender<OrderBook>,
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

/// Send book into SPSC slot. Always succeeds (overwrites previous value).
/// Returns `false` if consumer dropped.
#[inline]
pub fn try_send_book(
    sender: &SlotSender<OrderBook>,
    book: OrderBook,
    metrics: &ExchangeMetrics,
) -> bool {
    if sender.is_abandoned() {
        warn!(exchange = metrics.name, "consumer dropped, stopping");
        return false;
    }
    sender.send(book);
    true
}

/// Exponential backoff with equal jitter. Returns `Break` on cancellation.
///
/// Equal jitter = `[backoff/2, backoff)` — decorrelates reconnection storms
/// better than additive jitter (AWS architecture blog).
#[cold]
pub async fn backoff_sleep(
    backoff_ms: &mut u64,
    max_ms: u64,
    exchange: &'static str,
    cancel: &CancellationToken,
) -> ControlFlow<()> {
    let half = *backoff_ms / 2;
    let delay = half + fastrand::u64(0..half.max(1));
    warn!(exchange, delay_ms = delay, "reconnecting");
    tokio::select! {
        _ = cancel.cancelled() => return ControlFlow::Break(()),
        _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
    }
    *backoff_ms = (*backoff_ms * 2).min(max_ms);
    ControlFlow::Continue(())
}

/// Shared reconnection loop for all exchange adapters. Handles connection,
/// optional subscribe, inner read loop, metrics, backoff, and cancellation.
pub async fn run_ws_loop<H: WsHandler>(
    mut handler: H,
    symbol: String,
    sender: SlotSender<OrderBook>,
    metrics: Arc<ExchangeMetrics>,
    cancel: CancellationToken,
) {
    let url = handler.ws_url(&symbol);
    let ws_config = ws_config();
    let mut backoff_ms = INITIAL_BACKOFF_MS;

    loop {
        if cancel.is_cancelled() {
            return;
        }

        info!(exchange = H::NAME, %url, "connecting");

        let connect_fut = connect_async_tls_with_config(
            &url,
            Some(ws_config),
            /* disable_nagle */ true,
            None,
        );
        match tokio::time::timeout(CONNECT_TIMEOUT, connect_fut).await {
            Err(_) => {
                metrics.errors.fetch_add(1, Relaxed);
                error!(exchange = H::NAME, "connection timed out");
            }
            Ok(Ok((mut ws_stream, _))) => {
                info!(exchange = H::NAME, "connected");
                metrics.connected.store(true, Relaxed);
                backoff_ms = INITIAL_BACKOFF_MS;
                handler.on_connected();

                // Send optional subscribe message on the unsplit stream.
                // No split() = no BiLock = no atomic CAS per frame read.
                if let Some(msg) = handler.subscribe_message(&symbol)
                    && let Err(e) = ws_stream.send(Message::Text(msg)).await
                {
                    metrics.errors.fetch_add(1, Relaxed);
                    error!(exchange = H::NAME, error = %e, "subscribe failed");
                    metrics.connected.store(false, Relaxed);
                    // Fall through to backoff + reconnect.
                    if cancel.is_cancelled() {
                        return;
                    }
                    metrics.reconnections.fetch_add(1, Relaxed);
                    if backoff_sleep(&mut backoff_ms, MAX_BACKOFF_MS, H::NAME, &cancel)
                        .await
                        .is_break()
                    {
                        return;
                    }
                    continue;
                }

                // Inner read loop -- reads directly from unsplit stream.
                loop {
                    let text = tokio::select! {
                        _ = cancel.cancelled() => {
                            info!(exchange = H::NAME, "shutting down");
                            metrics.connected.store(false, Relaxed);
                            return;
                        }
                        msg = ws_stream.next() => match msg {
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

                    match handler.process_text(&text, &sender, &metrics) {
                        HandleResult::Continue => {}
                        HandleResult::Reconnect => break,
                        HandleResult::Shutdown => return,
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
            return;
        }

        metrics.reconnections.fetch_add(1, Relaxed);
        if backoff_sleep(&mut backoff_ms, MAX_BACKOFF_MS, H::NAME, &cancel)
            .await
            .is_break()
        {
            return;
        }
    }
}

/// Create a SPSC slot and spawn an exchange adapter on a dedicated OS thread.
/// Returns the receiver half (for the merger) and the thread handle.
///
/// Encapsulates slot creation + thread spawn so adding a new exchange is a
/// single call site.
pub fn wire_exchange<H: WsHandler>(
    handler: H,
    symbol: String,
    metrics: Arc<ExchangeMetrics>,
    cancel: CancellationToken,
) -> std::io::Result<(SlotReceiver<OrderBook>, std::thread::JoinHandle<()>)> {
    let (sender, receiver) = crate::atomic_slot::slot();
    let handle = spawn_exchange(handler, symbol, sender, metrics, cancel)?;
    Ok((receiver, handle))
}

/// Spawn an exchange adapter on a dedicated OS thread with its own single-threaded
/// tokio runtime. Isolates WS I/O from the main runtime.
fn spawn_exchange<H: WsHandler>(
    handler: H,
    symbol: String,
    sender: SlotSender<OrderBook>,
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
            rt.block_on(run_ws_loop(handler, symbol, sender, metrics, cancel));
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{book, exchange_metrics, raw};

    #[test]
    fn try_send_slot_overwrites() {
        let (sender, receiver) = crate::atomic_slot::slot();
        let m = exchange_metrics("test");
        let b1 = book(0, &[raw(100.0, 1.0)], &[raw(101.0, 1.0)]);
        let b2 = book(0, &[raw(200.0, 2.0)], &[raw(201.0, 2.0)]);
        assert!(try_send_book(&sender, b1, &m));
        // Second send overwrites the first -- always succeeds.
        assert!(try_send_book(&sender, b2, &m));
        // Consumer gets only the latest.
        let latest = receiver.recv().unwrap();
        assert_eq!(latest.bids[0].price, crate::types::FixedPoint::from_f64(200.0));
        assert!(receiver.recv().is_none());
    }

    #[test]
    fn try_send_abandoned_returns_false() {
        let (sender, receiver) = crate::atomic_slot::slot::<OrderBook>();
        let m = exchange_metrics("test");
        drop(receiver); // Abandon the consumer.
        assert!(!try_send_book(
            &sender,
            book(0, &[raw(100.0, 1.0)], &[raw(101.0, 1.0)]),
            &m
        ));
    }
}
