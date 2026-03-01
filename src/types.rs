//! Core domain types for order book data.
//!
//! All types on the hot path are stack-allocated via [`ArrayVec`] -- no heap
//! allocation between WebSocket receive and merge output. [`Level`] is `Copy`;
//! [`OrderBook`] and [`Summary`] are `Clone` (stack-allocated but too large
//! for implicit copies).

use std::fmt;
use std::time::Instant;

use arrayvec::ArrayVec;

/// Order book depth: levels per side in both per-exchange books and merged
/// output. Both exchanges send pre-sorted levels, so parsing more than the
/// output depth per exchange is provably wasted work -- the (DEPTH+1)-th
/// level from any exchange can never appear in the merged top-DEPTH.
pub const DEPTH: usize = 10;

/// Fixed-point decimal with 8 fractional digits: `value × 10⁻⁸` stored as `u64`.
///
/// Matches the 8-decimal-place precision used by Binance and Bitstamp.
/// Integer storage gives deterministic `Ord` (no NaN/Inf) and ~5× faster
/// comparison than `f64::total_cmp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct FixedPoint(u64);

/// Powers of 10 for fractional digit padding (single multiply instead of loop).
pub(crate) const POW10: [u64; 9] = [
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
        let int_digits = i; // digits consumed before '.'

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

        // Reject bare "." -- no integer or fractional digits consumed.
        if int_digits == 0 && frac_digits == 0 {
            return None;
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
    ///
    /// # Panics
    /// Panics if `v` is negative.
    #[inline]
    #[must_use]
    pub fn from_f64(v: f64) -> Self {
        assert!(
            v >= 0.0,
            "FixedPoint::from_f64 called with negative value: {v}"
        );
        #[expect(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        Self((v * Self::SCALE as f64).round() as u64)
    }

    /// Construct from a pre-computed raw value (for fused parsing).
    #[inline]
    #[must_use]
    pub fn from_raw(val: u64) -> Self {
        Self(val)
    }

    /// Raw inner value (for debugging/testing).
    #[inline]
    #[must_use]
    pub fn raw(self) -> u64 {
        self.0
    }

    /// Convert a raw spread value (`best_ask - best_bid` in 10⁻⁸ units) to `f64`.
    /// Keeps the `FixedPoint` encoding internal -- callers never touch `SCALE`.
    #[inline]
    #[must_use]
    pub fn spread_to_f64(raw: i64) -> f64 {
        raw as f64 / Self::SCALE as f64
    }
}

impl fmt::Display for FixedPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let int = self.0 / Self::SCALE;
        let frac = self.0 % Self::SCALE;
        write!(f, "{int}.{frac:08}")
    }
}

/// Compact exchange identifier. Only 2 exchanges → `u8` suffices. Name lookup
/// happens in `to_proto()` (cold path, per-client gRPC task).
pub type ExchangeId = u8;

/// A price/amount pair without exchange attribution. Used inside [`OrderBook`]
/// where all levels come from the same exchange (16 bytes, `Copy`).
#[derive(Debug, Clone, Copy)]
pub struct RawLevel {
    pub price: FixedPoint,
    pub amount: FixedPoint,
}

const _: () = assert!(size_of::<RawLevel>() == 16);

/// A price level with exchange attribution. Used in merged [`Summary`] output
/// where levels from different exchanges are interleaved (24 bytes, `Copy`).
#[derive(Debug, Clone, Copy)]
pub struct Level {
    pub price: FixedPoint,
    pub amount: FixedPoint,
    pub exchange_id: ExchangeId,
}

const _: () = assert!(size_of::<Level>() == 24);

/// Single-exchange order book snapshot. Moved through the SPSC atomic slot.
///
/// **Invariant**: bids sorted descending by price, asks ascending.
#[derive(Debug, Clone)]
pub struct OrderBook {
    pub exchange_id: ExchangeId,
    pub bids: ArrayVec<RawLevel, DEPTH>,
    pub asks: ArrayVec<RawLevel, DEPTH>,
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
    pub bids: ArrayVec<Level, DEPTH>,
    pub asks: ArrayVec<Level, DEPTH>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a `#[test]` asserting `FixedPoint::parse(input).raw() == expected`.
    macro_rules! test_parse {
        ($name:ident, $input:expr, $expected:expr) => {
            #[test]
            fn $name() {
                assert_eq!(FixedPoint::parse($input).unwrap().raw(), $expected);
            }
        };
    }

    test_parse!(parse_integer_only, "123", 123 * FixedPoint::SCALE);
    test_parse!(parse_with_fractional, "0.06824000", 6_824_000);
    test_parse!(
        parse_short_fractional,
        "101.5",
        101 * FixedPoint::SCALE + 50_000_000
    );
    test_parse!(parse_leading_dot, ".5", 50_000_000);
    test_parse!(parse_zero, "0", 0);
    test_parse!(parse_trailing_dot, "0.", 0);
    test_parse!(parse_leading_zeros, "00.1", 10_000_000);
    test_parse!(parse_truncates_beyond_8, "0.068240001", 6_824_000);
    test_parse!(parse_max_fractional, "0.99999999", 99_999_999);
    test_parse!(
        parse_large_integer,
        "184467440737.0",
        184_467_440_737 * FixedPoint::SCALE
    );

    #[test]
    fn parse_rejects_invalid_inputs() {
        let cases: &[(&str, &str)] = &[
            ("", "empty"),
            (".", "bare dot"),
            ("abc", "non-digit"),
            ("1.2x", "trailing non-digit"),
            ("-1.0", "negative"),
            ("999999999999999", "overflow"),
            ("1.2.3", "double dot"),
            (" 1", "leading space"),
        ];
        for &(input, label) in cases {
            assert!(
                FixedPoint::parse(input).is_none(),
                "expected None for {label}: {input:?}"
            );
        }
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
    #[should_panic(expected = "negative value")]
    fn from_f64_panics_on_negative_in_debug() {
        let _ = FixedPoint::from_f64(-1.0);
    }

    #[test]
    fn static_size_assertions() {
        assert_eq!(size_of::<FixedPoint>(), 8);
        assert_eq!(size_of::<RawLevel>(), 16);
        assert_eq!(size_of::<Level>(), 24);
    }
}
