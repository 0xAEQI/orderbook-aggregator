//! Shared test fixtures and helpers.
//!
//! Provides reusable constructors for domain types used across multiple test
//! modules. Avoids duplicating `raw()`, `book()`, etc. in every `#[cfg(test)]`
//! block.

use std::time::Instant;

use crate::metrics::ExchangeMetrics;
use crate::types::{FixedPoint, OrderBook, RawLevel};

/// `RawLevel` from f64 price and amount (test convenience).
pub(crate) fn raw(price: f64, amount: f64) -> RawLevel {
    RawLevel {
        price: FixedPoint::from_f64(price),
        amount: FixedPoint::from_f64(amount),
    }
}

/// `OrderBook` from exchange name and level slices.
pub(crate) fn book(exchange: &'static str, bids: &[RawLevel], asks: &[RawLevel]) -> OrderBook {
    OrderBook {
        exchange,
        bids: bids.iter().copied().collect(),
        asks: asks.iter().copied().collect(),
        decode_start: Instant::now(),
    }
}

/// `ExchangeMetrics` zeroed out for testing.
pub(crate) fn exchange_metrics(name: &'static str) -> ExchangeMetrics {
    ExchangeMetrics::new(name)
}
