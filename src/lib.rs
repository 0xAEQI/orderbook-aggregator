//! Order Book Aggregator library.
//!
//! Connects to Binance and Bitstamp WebSocket feeds, merges their order books,
//! and streams the top-10 bid/ask levels with spread via gRPC.

pub mod config;
pub mod exchange;
pub(crate) mod json_walker;
pub mod merger;
pub mod metrics;
pub mod server;
#[cfg(test)]
pub(crate) mod testutil;
pub mod types;

/// Supported exchanges. Single source of truth -- sizes stack arrays in the
/// merger and registers metrics. To add an exchange: add its name here and
/// wire its handler in `main.rs`.
pub const EXCHANGES: &[&str] = &["binance", "bitstamp"];
