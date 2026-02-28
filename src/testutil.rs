//! Shared test fixtures and helpers.
//!
//! Provides reusable constructors for domain types used across multiple test
//! modules. Avoids duplicating `raw()`, `book()`, etc. in every `#[cfg(test)]`
//! block.

use std::time::Instant;

use crate::metrics::ExchangeMetrics;
use crate::types::{ExchangeId, FixedPoint, OrderBook, RawLevel};

/// `RawLevel` from f64 price and amount (test convenience).
pub(crate) fn raw(price: f64, amount: f64) -> RawLevel {
    RawLevel {
        price: FixedPoint::from_f64(price),
        amount: FixedPoint::from_f64(amount),
    }
}

/// `OrderBook` from exchange ID and level slices.
pub(crate) fn book(exchange_id: ExchangeId, bids: &[RawLevel], asks: &[RawLevel]) -> OrderBook {
    OrderBook {
        exchange_id,
        bids: bids.iter().copied().collect(),
        asks: asks.iter().copied().collect(),
        decode_start: Instant::now(),
    }
}

/// `ExchangeMetrics` zeroed out for testing.
pub(crate) fn exchange_metrics(name: &'static str) -> ExchangeMetrics {
    ExchangeMetrics::new(name)
}

// ---------------------------------------------------------------------------
// Shared JSON fixtures (used by exchange unit tests)
// ---------------------------------------------------------------------------

/// 3-level Binance `depth20` snapshot (compact, matches production wire format).
pub(crate) const BINANCE_JSON_3L: &str = r#"{"lastUpdateId":123456789,"bids":[["0.06824000","12.50000000"],["0.06823000","8.30000000"],["0.06822000","5.00000000"]],"asks":[["0.06825000","10.00000000"],["0.06826000","7.20000000"],["0.06827000","3.50000000"]]}"#;

/// 3-level Bitstamp `order_book` data message (real field order: data before event).
pub(crate) const BITSTAMP_JSON_3L: &str = r#"{"data":{"timestamp":"1700000000","microtimestamp":"1700000000000000","bids":[["0.06824000","12.50000000"],["0.06823000","8.30000000"],["0.06822000","5.00000000"]],"asks":[["0.06825000","10.00000000"],["0.06826000","7.20000000"],["0.06827000","3.50000000"]]},"channel":"order_book_ethbtc","event":"data"}"#;
