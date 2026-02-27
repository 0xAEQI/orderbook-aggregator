//! Core domain types for order book data.
//!
//! All types on the hot path are stack-allocated via [`ArrayVec`] -- no heap
//! allocation between WebSocket receive and merge output. [`Level`] is `Copy`;
//! [`OrderBook`] and [`Summary`] are `Clone` (stack-allocated but too large
//! for implicit copies).

use std::fmt;
use std::time::Instant;

use arrayvec::ArrayVec;

/// Max levels we keep per side from a single exchange (Binance sends 20).
pub const MAX_LEVELS: usize = 20;

/// Top N levels in the merged output sent to gRPC clients.
pub const TOP_N: usize = 10;

/// Fixed-point decimal with 8 fractional digits: `value × 10⁻⁸` stored as `u64`.
///
/// Matches the 8-decimal-place precision used by Binance and Bitstamp.
/// Integer storage gives deterministic `Ord` (no NaN/Inf) and ~5× faster
/// comparison than `f64::total_cmp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct FixedPoint(u64);

/// Powers of 10 for fractional digit padding (single multiply instead of loop).
const POW10: [u64; 9] = [
    1,
    10,
    100,
    1_000,
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
];

impl FixedPoint {
    pub const SCALE: u64 = 100_000_000; // 10^8

    /// `"123.456"` → `FixedPoint(12345600000)`. No intermediate `f64`.
    ///
    /// Returns `None` on empty input, non-digit bytes, or overflow.
    #[inline]
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let bytes = s.as_bytes();
        if bytes.is_empty() {
            return None;
        }

        let mut i = 0;
        let mut int_part: u64 = 0;
        while i < bytes.len() && bytes[i] != b'.' {
            let d = bytes[i].wrapping_sub(b'0');
            if d > 9 {
                return None;
            }
            int_part = int_part.checked_mul(10)?.checked_add(u64::from(d))?;
            i += 1;
        }

        let mut frac_part: u64 = 0;
        let mut frac_digits: u32 = 0;
        if i < bytes.len() && bytes[i] == b'.' {
            i += 1;
            while i < bytes.len() && frac_digits < 8 {
                let d = bytes[i].wrapping_sub(b'0');
                if d > 9 {
                    return None;
                }
                frac_part = frac_part * 10 + u64::from(d);
                frac_digits += 1;
                i += 1;
            }
            while i < bytes.len() {
                let d = bytes[i].wrapping_sub(b'0');
                if d > 9 {
                    return None;
                }
                i += 1;
            }
        }

        frac_part *= POW10[(8 - frac_digits) as usize];

        let value = int_part.checked_mul(Self::SCALE)?.checked_add(frac_part)?;
        Some(Self(value))
    }

    /// Convert to f64 for proto serialization and display (cold path only).
    #[inline]
    #[must_use]
    pub fn to_f64(self) -> f64 {
        self.0 as f64 / Self::SCALE as f64
    }

    /// Create from f64 -- convenience for tests and cold-path construction.
    #[inline]
    #[must_use]
    pub fn from_f64(v: f64) -> Self {
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        Self((v * Self::SCALE as f64).round() as u64)
    }

    /// Raw inner value (for debugging/testing).
    #[inline]
    #[must_use]
    pub fn raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for FixedPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let int = self.0 / Self::SCALE;
        let frac = self.0 % Self::SCALE;
        write!(f, "{int}.{frac:08}")
    }
}

/// A price/amount pair without exchange attribution. Used inside [`OrderBook`]
/// where all levels come from the same exchange (16 bytes, `Copy`).
#[derive(Debug, Clone, Copy)]
pub struct RawLevel {
    pub price: FixedPoint,
    pub amount: FixedPoint,
}

/// A price level with exchange attribution. Used in merged [`Summary`] output
/// where levels from different exchanges are interleaved (32 bytes, `Copy`).
#[derive(Debug, Clone, Copy)]
pub struct Level {
    pub exchange: &'static str,
    pub price: FixedPoint,
    pub amount: FixedPoint,
}

/// Single-exchange order book snapshot. Moved through the SPSC ring buffer.
///
/// **Invariant**: bids sorted descending by price, asks ascending.
#[derive(Debug, Clone)]
pub struct OrderBook {
    pub exchange: &'static str,
    pub bids: ArrayVec<RawLevel, MAX_LEVELS>,
    pub asks: ArrayVec<RawLevel, MAX_LEVELS>,
    /// When decode started (immediately after WS frame dispatch) -- e2e latency anchor.
    pub decode_start: Instant,
}

/// Merged top-of-book across all exchanges. Published via `watch` channel.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    /// `best_ask - best_bid` in raw [`FixedPoint`] units (10⁻⁸ scale).
    /// Negative for crossed books. Zero if either side is empty.
    /// Convert to `f64` only at the proto boundary.
    pub spread_raw: i64,
    pub bids: ArrayVec<Level, TOP_N>,
    pub asks: ArrayVec<Level, TOP_N>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_integer_only() {
        assert_eq!(
            FixedPoint::parse("123").unwrap().raw(),
            123 * FixedPoint::SCALE
        );
    }

    #[test]
    fn parse_with_fractional() {
        let fp = FixedPoint::parse("0.06824000").unwrap();
        assert_eq!(fp.raw(), 6_824_000);
    }

    #[test]
    fn parse_short_fractional() {
        let fp = FixedPoint::parse("101.5").unwrap();
        assert_eq!(fp.raw(), 101 * FixedPoint::SCALE + 50_000_000);
    }

    #[test]
    fn parse_leading_dot() {
        let fp = FixedPoint::parse(".5").unwrap();
        assert_eq!(fp.raw(), 50_000_000);
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(FixedPoint::parse("").is_none());
    }

    #[test]
    fn parse_rejects_non_digit() {
        assert!(FixedPoint::parse("abc").is_none());
        assert!(FixedPoint::parse("1.2x").is_none());
    }

    #[test]
    fn to_f64_roundtrip() {
        let fp = FixedPoint::parse("0.06824000").unwrap();
        assert!((fp.to_f64() - 0.06824).abs() < 1e-12);
    }

    #[test]
    fn from_f64_roundtrip() {
        let fp = FixedPoint::from_f64(100.5);
        assert_eq!(fp.raw(), 100 * FixedPoint::SCALE + 50_000_000);
    }

    #[test]
    fn display_formatting() {
        let fp = FixedPoint::parse("0.06824000").unwrap();
        assert_eq!(format!("{fp}"), "0.06824000");
    }

    #[test]
    fn ordering_matches_numeric() {
        let a = FixedPoint::parse("0.06824000").unwrap();
        let b = FixedPoint::parse("0.06825000").unwrap();
        assert!(a < b);
    }

    #[test]
    fn parse_rejects_overflow() {
        // u64::MAX / SCALE ≈ 184_467_440_737, so this overflows.
        assert!(FixedPoint::parse("999999999999999").is_none());
    }

    #[test]
    fn truncates_beyond_8_digits() {
        // "0.068240001" → truncates to "0.06824000"
        let fp = FixedPoint::parse("0.068240001").unwrap();
        assert_eq!(fp.raw(), 6_824_000);
    }

    #[test]
    fn parse_zero() {
        assert_eq!(FixedPoint::parse("0").unwrap(), FixedPoint(0));
    }

    #[test]
    fn parse_trailing_dot() {
        // "0." -- integer part only, dot consumed but no fractional digits.
        assert_eq!(FixedPoint::parse("0.").unwrap(), FixedPoint(0));
    }

    #[test]
    fn parse_dot_only() {
        // "." -- no integer part, no fractional digits. Treated as 0.
        assert_eq!(FixedPoint::parse(".").unwrap(), FixedPoint(0));
    }

    #[test]
    fn parse_leading_zeros() {
        let fp = FixedPoint::parse("00.1").unwrap();
        assert_eq!(fp.raw(), 10_000_000);
    }
}
