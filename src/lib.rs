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
pub mod types;
