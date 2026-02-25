//! Exchange WebSocket adapters.
//!
//! Each adapter connects to an exchange's depth stream, parses updates into
//! [`OrderBook`] snapshots, and publishes them via an mpsc channel.
//!
//! Connections use `TCP_NODELAY` to eliminate Nagle's algorithm delay and
//! `write_buffer_size: 0` for immediate WebSocket frame flushing.

pub mod binance;
pub mod bitstamp;

use arrayvec::ArrayVec;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::types::{Level, MAX_LEVELS, OrderBook};

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

/// Parse borrowed string slices directly into f64 — no intermediate String allocation.
/// Uses `fast-float` for SIMD-accelerated float parsing (~2-3x faster than `str::parse`).
/// Caps output at `MAX_LEVELS` — excess levels from the exchange are dropped.
#[inline]
pub fn parse_levels(exchange: &'static str, raw: &[[&str; 2]]) -> ArrayVec<Level, MAX_LEVELS> {
    let mut levels = ArrayVec::new();
    for &[price, amount] in raw {
        match (fast_float::parse(price), fast_float::parse(amount)) {
            (Ok(p), Ok(a)) => {
                if levels.try_push(Level {
                    exchange,
                    price: p,
                    amount: a,
                }).is_err() {
                    break; // At capacity — stop parsing.
                }
            }
            (Err(e), _) | (_, Err(e)) => {
                tracing::warn!(
                    exchange,
                    price,
                    amount,
                    error = %e,
                    "malformed price level — skipped"
                );
            }
        }
    }
    levels
}
