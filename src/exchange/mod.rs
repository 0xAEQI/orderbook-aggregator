//! Exchange WebSocket adapters.
//!
//! Each adapter connects to an exchange's depth stream, parses updates into
//! [`OrderBook`] snapshots, and publishes them via an mpsc channel.
//!
//! Connections use `TCP_NODELAY` to eliminate Nagle's algorithm delay and
//! `write_buffer_size: 0` for immediate WebSocket frame flushing.
//!
//! Shared reconnection and channel-send logic lives here to avoid duplication
//! across adapters.

pub mod binance;
pub mod bitstamp;

use std::time::Duration;

use arrayvec::ArrayVec;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::types::{FixedPoint, Level, MAX_LEVELS, OrderBook};

/// Connection timeout for WebSocket handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Trait implemented by each exchange adapter.
pub trait Exchange: Send + Sync + 'static {
    /// Connect to the exchange WebSocket and stream order book updates.
    ///
    /// Implementations must handle reconnection internally and respect the
    /// cancellation token for graceful shutdown.
    fn connect(
        &self,
        symbol: String,
        sender: mpsc::Sender<OrderBook>,
        cancel: CancellationToken,
    ) -> impl std::future::Future<Output = Result<()>> + Send;
}

/// WebSocket config optimized for low-latency reads.
#[must_use]
pub fn ws_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
        write_buffer_size: 0, // Flush every frame immediately.
        ..Default::default()
    }
}

/// Parse borrowed string slices directly into fixed-point integers — no intermediate
/// f64. Byte-scans each decimal string into `FixedPoint(u64)` with 10^8 scaling.
/// Caps output at `MAX_LEVELS` — excess levels from the exchange are dropped.
#[inline]
pub fn parse_levels(exchange: &'static str, raw: &[[&str; 2]]) -> ArrayVec<Level, MAX_LEVELS> {
    let mut levels = ArrayVec::new();
    for &[price, amount] in raw {
        match (FixedPoint::parse(price), FixedPoint::parse(amount)) {
            (Some(p), Some(a)) => {
                if levels.try_push(Level {
                    exchange,
                    price: p,
                    amount: a,
                }).is_err() {
                    break; // At capacity — stop parsing.
                }
            }
            _ => {
                tracing::warn!(
                    exchange,
                    price,
                    amount,
                    "malformed price level — skipped"
                );
            }
        }
    }
    levels
}

/// Try to send an order book snapshot to the merger, handling backpressure.
///
/// Returns `true` to continue, `false` if the channel is closed (caller should stop).
#[inline]
pub fn try_send_book(
    sender: &mpsc::Sender<OrderBook>,
    book: OrderBook,
    exchange: &'static str,
) -> bool {
    match sender.try_send(book) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Full(_)) => {
            tracing::warn!(exchange, "channel full, dropping snapshot");
            true
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            tracing::warn!(exchange, "channel closed, stopping");
            false
        }
    }
}

/// Exponential backoff with random jitter, cancellation-aware.
///
/// Returns `true` if the caller should continue reconnecting, `false` on cancellation.
pub async fn backoff_sleep(
    backoff_ms: &mut u64,
    max_ms: u64,
    exchange: &'static str,
    cancel: &CancellationToken,
) -> bool {
    let jitter = rand::random::<u64>() % (*backoff_ms / 2).max(1);
    let delay = *backoff_ms + jitter;
    tracing::warn!(exchange, delay_ms = delay, "reconnecting");
    tokio::select! {
        _ = cancel.cancelled() => return false,
        _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
    }
    *backoff_ms = (*backoff_ms * 2).min(max_ms);
    true
}
