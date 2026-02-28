//! Order Book Aggregator library.
//!
//! Connects to Binance and Bitstamp WebSocket feeds, merges their order books,
//! and streams the top-10 bid/ask levels with spread via gRPC.

pub mod config;
pub mod error;
pub mod exchange;
pub(crate) mod json_walker;
pub mod merger;
pub mod metrics;
pub mod server;
#[cfg(test)]
pub(crate) mod testutil;
pub mod types;

use types::ExchangeId;

/// Supported exchanges. Single source of truth -- sizes stack arrays in the
/// merger and registers metrics. To add an exchange: add its name here and
/// wire its handler in `main.rs`.
pub const EXCHANGES: &[&str] = &["binance", "bitstamp"];

/// Compact exchange IDs for hot-path types. Name lookup via [`exchange_name`].
pub const BINANCE_ID: ExchangeId = 0;
pub const BITSTAMP_ID: ExchangeId = 1;

/// Resolve an [`ExchangeId`] to its string name (cold path, for proto/logging).
#[inline]
#[must_use]
pub fn exchange_name(id: ExchangeId) -> &'static str {
    EXCHANGES[id as usize]
}
