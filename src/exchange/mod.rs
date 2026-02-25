//! Exchange WebSocket adapters.
//!
//! Each adapter connects to an exchange's depth stream, parses updates into
//! [`OrderBook`] snapshots, and publishes them via a broadcast channel.

pub mod binance;
pub mod bitstamp;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::types::OrderBook;

/// Trait implemented by each exchange adapter.
pub trait Exchange: Send + Sync + 'static {
    /// Connect to the exchange WebSocket and stream order book updates.
    ///
    /// Implementations must handle reconnection internally and respect the
    /// cancellation token for graceful shutdown.
    fn connect(
        &self,
        symbol: String,
        sender: broadcast::Sender<OrderBook>,
        cancel: CancellationToken,
    ) -> impl std::future::Future<Output = Result<()>> + Send;
}
