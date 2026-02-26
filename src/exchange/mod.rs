//! Exchange WebSocket adapters.
//!
//! Each adapter connects to an exchange's depth stream, parses updates into
//! [`OrderBook`] snapshots, and publishes them via an mpsc channel. Both
//! exchanges provide full snapshots (not incremental diffs), so no local
//! order book maintenance or sequence reconciliation is required.
//!
//! Connections use `TCP_NODELAY` to eliminate Nagle's algorithm delay and
//! `write_buffer_size: 0` for immediate WebSocket frame flushing.
//!
//! Shared reconnection and channel-send logic lives here to avoid duplication
//! across adapters.

pub mod binance;
pub mod bitstamp;

use std::time::{Duration, Instant};

use arrayvec::ArrayVec;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::types::{FixedPoint, Level, MAX_LEVELS, OrderBook};

/// Connection timeout for WebSocket handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum reconnection backoff (shared across all adapters).
const MAX_BACKOFF_MS: u64 = 30_000;

/// Initial reconnection backoff before exponential increase.
const INITIAL_BACKOFF_MS: u64 = 1_000;

/// Exchange adapter. Handles reconnection internally; respects cancellation.
pub trait Exchange: Send + Sync + 'static {
    fn connect(
        &self,
        symbol: String,
        sender: mpsc::Sender<OrderBook>,
        cancel: CancellationToken,
    ) -> impl std::future::Future<Output = Result<()>> + Send;
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
        if levels.try_push(Level { exchange, price: p, amount: a }).is_err() {
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

/// Send book to merger. Drops on full (stale data). Returns `false` if channel closed.
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

/// Exponential backoff with jitter. Returns `false` on cancellation.
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
