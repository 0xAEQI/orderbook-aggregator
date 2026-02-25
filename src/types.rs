//! Core domain types for order book data.
//!
//! All types on the hot path are stack-allocated via [`ArrayVec`] and [`Copy`]
//! to avoid heap allocation between WebSocket receive and merge output.

use std::time::Instant;

use arrayvec::ArrayVec;

/// Max levels we keep per side from a single exchange (Binance sends 20).
pub const MAX_LEVELS: usize = 20;

/// Top N levels in the merged output sent to gRPC clients.
pub const TOP_N: usize = 10;

/// A single price level from an exchange.
///
/// `Copy` by design — all fields are trivially copyable (`&'static str`, `f64`,
/// `f64`), allowing zero-overhead moves through channels and merge buffers.
#[derive(Debug, Clone, Copy)]
pub struct Level {
    /// Source exchange name (e.g., `"binance"`, `"bitstamp"`).
    pub exchange: &'static str,
    pub price: f64,
    pub amount: f64,
}

/// Snapshot of one exchange's order book at a point in time.
///
/// Produced by exchange adapters and sent to the merger via `mpsc` with move
/// semantics — no cloning, no heap allocation.
#[derive(Debug, Clone)]
pub struct OrderBook {
    /// Source exchange name.
    pub exchange: &'static str,
    /// Bids sorted highest price first (exchange-provided ordering).
    pub bids: ArrayVec<Level, MAX_LEVELS>,
    /// Asks sorted lowest price first (exchange-provided ordering).
    pub asks: ArrayVec<Level, MAX_LEVELS>,
    /// When the WebSocket frame was received — used for e2e latency measurement.
    pub received_at: Instant,
}

/// Merged top-of-book summary published to gRPC clients via `watch` channel.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    /// `best_ask - best_bid`. Zero if either side is empty.
    pub spread: f64,
    /// Top 10 bids across all exchanges, highest price first.
    pub bids: ArrayVec<Level, TOP_N>,
    /// Top 10 asks across all exchanges, lowest price first.
    pub asks: ArrayVec<Level, TOP_N>,
}
