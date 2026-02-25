//! Core domain types for order book data.

use std::time::Instant;

/// A single price level from an exchange.
#[derive(Debug, Clone)]
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
    pub bids: Vec<Level>,
    /// Asks sorted lowest price first.
    pub asks: Vec<Level>,
    /// Timestamp when the WebSocket frame was received (for e2e latency).
    pub received_at: Instant,
}

/// Merged summary ready for gRPC broadcast.
#[derive(Debug, Clone)]
pub struct Summary {
    pub spread: f64,
    /// Top 10 bids, highest price first.
    pub bids: Vec<Level>,
    /// Top 10 asks, lowest price first.
    pub asks: Vec<Level>,
}

impl Default for Summary {
    fn default() -> Self {
        Self {
            spread: 0.0,
            bids: Vec::new(),
            asks: Vec::new(),
        }
    }
}
