//! Core domain types for order book data.

use std::time::Instant;

use arrayvec::ArrayVec;

/// Max levels we keep per side from a single exchange.
pub const MAX_LEVELS: usize = 20;

/// Top N levels in the merged output.
pub const TOP_N: usize = 10;

/// A single price level from an exchange.
#[derive(Debug, Clone, Copy)]
pub struct Level {
    pub exchange: &'static str,
    pub price: f64,
    pub amount: f64,
}

/// Snapshot of one exchange's order book.
#[derive(Debug, Clone)]
pub struct OrderBook {
    pub exchange: &'static str,
    /// Bids sorted highest price first.
    pub bids: ArrayVec<Level, MAX_LEVELS>,
    /// Asks sorted lowest price first.
    pub asks: ArrayVec<Level, MAX_LEVELS>,
    /// Timestamp when the WebSocket frame was received (for e2e latency).
    pub received_at: Instant,
}

/// Merged summary ready for gRPC broadcast.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub spread: f64,
    /// Top 10 bids, highest price first.
    pub bids: ArrayVec<Level, TOP_N>,
    /// Top 10 asks, lowest price first.
    pub asks: ArrayVec<Level, TOP_N>,
}
