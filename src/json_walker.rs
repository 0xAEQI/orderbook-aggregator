//! Shared scanner utilities for zero-copy JSON parsing.
//!
//! Provides a byte-level `Scanner` with fused `read_quoted_decimal` and
//! `read_raw_levels` that parse JSON quoted decimals directly into `FixedPoint`
//! in a single forward pass -- no intermediate `&str`, no double-scan.
//! Uses SIMD-accelerated substring search (`memchr::memmem`) for key lookup.

use arrayvec::ArrayVec;
use memchr::memmem;

use crate::types::{FixedPoint, POW10, RawLevel};

/// Byte scanner that tracks position in an input buffer.
pub(crate) struct Scanner<'a> {
    buf: &'a [u8],
    src: &'a str,
    pub(crate) pos: usize,
}

impl<'a> Scanner<'a> {
    #[inline]
    pub(crate) fn new(json: &'a str) -> Self {
        Self {
            buf: json.as_bytes(),
            src: json,
            pos: 0,
        }
    }

    #[inline]
    pub(crate) fn peek(&self) -> Option<u8> {
        self.buf.get(self.pos).copied()
    }

    #[inline]
    pub(crate) fn skip_ws(&mut self) {
        while self.pos < self.buf.len() {
            match self.buf[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => return,
            }
        }
    }

    /// Raw byte check without whitespace skipping. Exchange APIs always send
    /// compact JSON (no whitespace in level arrays), so `skip_ws()` overhead
    /// is eliminated on the hot path.
    #[inline]
    pub(crate) fn expect_byte(&mut self, byte: u8) -> bool {
        if self.pos < self.buf.len() && self.buf[self.pos] == byte {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// SIMD-seek to a key pattern and advance past it. Uses vectorized
    /// two-byte or multi-byte algorithms from `memchr::memmem` -- typically
    /// 4-8× faster than byte-by-byte scanning on `x86_64`.
    #[inline]
    pub(crate) fn seek(&mut self, finder: &memmem::Finder<'_>) -> Option<()> {
        let needle_len = finder.needle().len();
        let offset = finder.find(&self.buf[self.pos..])?;
        self.pos += offset + needle_len;
        Some(())
    }

    /// Extract the content of a JSON string value as a borrowed `&str`.
    #[inline]
    pub(crate) fn read_string(&mut self) -> Option<&'a str> {
        self.skip_ws();
        if self.pos >= self.buf.len() || self.buf[self.pos] != b'"' {
            return None;
        }
        self.pos += 1;
        let start = self.pos;
        while self.pos < self.buf.len() {
            match self.buf[self.pos] {
                b'"' => {
                    let result = self.src.get(start..self.pos)?;
                    self.pos += 1;
                    return Some(result);
                }
                b'\\' => {
                    // Skip escaped character. +2 is safe: if the backslash is
                    // the last byte, the while condition exits on the next iteration.
                    self.pos += 2;
                }
                _ => self.pos += 1,
            }
        }
        None
    }

    /// Read an unsigned integer value (JSON number). Returns 0 if not found
    /// or on overflow.
    #[inline]
    pub(crate) fn read_u64(&mut self) -> u64 {
        self.skip_ws();
        let mut val: u64 = 0;
        while self.pos < self.buf.len() {
            let d = self.buf[self.pos].wrapping_sub(b'0');
            if d > 9 {
                break;
            }
            val = match val
                .checked_mul(10)
                .and_then(|v| v.checked_add(u64::from(d)))
            {
                Some(v) => v,
                None => return 0, // overflow → treat as missing
            };
            self.pos += 1;
        }
        val
    }

    /// Parse a JSON quoted decimal directly into [`FixedPoint`] in a single
    /// forward pass. Opens `"`, scans digits/dot building `int_part`+`frac_part`,
    /// closes `"`. No intermediate `&str`, no escape handling (decimal strings
    /// never contain backslashes).
    ///
    /// Assumes compact JSON (no leading whitespace). Called exclusively from
    /// `read_raw_levels` on exchange data where arrays are always compact.
    #[inline]
    pub(crate) fn read_quoted_decimal(&mut self) -> Option<FixedPoint> {
        if self.pos >= self.buf.len() || self.buf[self.pos] != b'"' {
            return None;
        }
        self.pos += 1; // opening quote

        let mut int_part: u64 = 0;
        let mut int_digits: u32 = 0;
        while self.pos < self.buf.len() && self.buf[self.pos] != b'.' && self.buf[self.pos] != b'"'
        {
            let d = self.buf[self.pos].wrapping_sub(b'0');
            if d > 9 {
                return None;
            }
            int_part = int_part.checked_mul(10)?.checked_add(u64::from(d))?;
            int_digits += 1;
            self.pos += 1;
        }

        let mut frac_part: u64 = 0;
        let mut frac_digits: u32 = 0;
        if self.pos < self.buf.len() && self.buf[self.pos] == b'.' {
            self.pos += 1;
            while self.pos < self.buf.len() && self.buf[self.pos] != b'"' && frac_digits < 8 {
                let d = self.buf[self.pos].wrapping_sub(b'0');
                if d > 9 {
                    return None;
                }
                frac_part = frac_part * 10 + u64::from(d);
                frac_digits += 1;
                self.pos += 1;
            }
            // Skip any fractional digits beyond 8.
            while self.pos < self.buf.len() && self.buf[self.pos] != b'"' {
                let d = self.buf[self.pos].wrapping_sub(b'0');
                if d > 9 {
                    return None;
                }
                self.pos += 1;
            }
        }

        // Reject bare "." or empty string.
        if int_digits == 0 && frac_digits == 0 {
            return None;
        }

        // Closing quote.
        if self.pos >= self.buf.len() || self.buf[self.pos] != b'"' {
            return None;
        }
        self.pos += 1;

        frac_part *= POW10[(8 - frac_digits) as usize];
        let value = int_part
            .checked_mul(FixedPoint::SCALE)?
            .checked_add(frac_part)?;
        Some(FixedPoint::from_raw(value))
    }

    /// Seek to a key pattern and read raw levels. Returns empty if key is absent.
    #[inline]
    pub(crate) fn read_optional_raw_levels<const N: usize>(
        &mut self,
        finder: &memmem::Finder<'_>,
    ) -> Option<ArrayVec<RawLevel, N>> {
        if self.seek(finder).is_some() {
            read_raw_levels(self)
        } else {
            Some(ArrayVec::new())
        }
    }
}

/// Read an array of `["price","qty"]` pairs, parsing each directly into
/// [`RawLevel`] via `read_quoted_decimal`. Filters zero-amount levels inline.
/// Keeps the first `N` non-zero levels; once at capacity, returns immediately.
///
/// **Compact JSON only**: all byte checks are direct — no `skip_ws()` overhead.
/// Exchange APIs (Binance, Bitstamp) always send compact arrays with no
/// whitespace between brackets, commas, or quoted decimals. This eliminates
/// ~160 redundant whitespace checks per 20-level message.
pub(crate) fn read_raw_levels<const N: usize>(
    s: &mut Scanner<'_>,
) -> Option<ArrayVec<RawLevel, N>> {
    if !s.expect_byte(b'[') {
        return None;
    }
    let mut levels = ArrayVec::new();
    if s.peek() == Some(b']') {
        s.pos += 1;
        return Some(levels);
    }
    loop {
        if !s.expect_byte(b'[') {
            return None;
        }
        let price = s.read_quoted_decimal()?;
        if !s.expect_byte(b',') {
            return None;
        }
        let amount = s.read_quoted_decimal()?;
        if !s.expect_byte(b']') {
            return None;
        }
        // Filter zero-amount levels inline (Binance sends "0.00000000" for cleared levels).
        if amount.raw() != 0 && levels.try_push(RawLevel { price, amount }).is_err() {
            return Some(levels);
        }
        match s.peek() {
            Some(b',') => {
                s.pos += 1;
                if levels.len() == N {
                    return Some(levels);
                }
            }
            Some(b']') => {
                s.pos += 1;
                return Some(levels);
            }
            _ => return None,
        }
    }
}

/// Extract a string value by key pattern from a JSON payload (cold path).
///
/// Used for error messages, channel names, etc. -- not on the hot path.
/// `pattern` should include the key and colon, e.g. `b"\"message\":"`.
#[must_use]
pub(crate) fn extract_string<'a>(json: &'a str, pattern: &[u8]) -> Option<&'a str> {
    let finder = memmem::Finder::new(pattern);
    let mut s = Scanner::new(json);
    s.seek(&finder)?;
    s.read_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FixedPoint;

    #[test]
    fn read_string_basic() {
        let mut s = Scanner::new(r#""hello""#);
        assert_eq!(s.read_string(), Some("hello"));
    }

    #[test]
    fn read_string_with_escape() {
        let mut s = Scanner::new(r#""say \"hi\"""#);
        // The scanner doesn't unescape — it returns the raw content between outer quotes.
        assert_eq!(s.read_string(), Some(r#"say \"hi\""#));
    }

    #[test]
    fn read_u64_parses_number() {
        let mut s = Scanner::new("42,");
        assert_eq!(s.read_u64(), 42);
        assert_eq!(s.pos, 2); // Stopped at the comma.
    }

    #[test]
    fn read_u64_returns_zero_on_overflow() {
        let mut s = Scanner::new("99999999999999999999"); // > u64::MAX
        assert_eq!(s.read_u64(), 0);
    }

    #[test]
    fn seek_advances_past_needle() {
        let mut s = Scanner::new(r#"{"key":"value"}"#);
        let finder = memmem::Finder::new(b"\"key\":");
        s.seek(&finder).unwrap();
        // Position should be right after the needle, ready to read the value.
        assert_eq!(s.read_string(), Some("value"));
    }

    #[test]
    fn extract_string_finds_value() {
        let json = r#"{"event":"data","message":"hello world"}"#;
        assert_eq!(extract_string(json, b"\"message\":"), Some("hello world"));
        assert_eq!(extract_string(json, b"\"event\":"), Some("data"));
        assert_eq!(extract_string(json, b"\"missing\":"), None);
    }

    // ── Fused read_quoted_decimal tests ──────────────────────────────────

    #[test]
    fn read_quoted_decimal_basic() {
        let mut s = Scanner::new(r#""123.45600000""#);
        let fp = s.read_quoted_decimal().unwrap();
        assert_eq!(fp, FixedPoint::parse("123.45600000").unwrap());
    }

    #[test]
    fn read_quoted_decimal_fractional_only() {
        let mut s = Scanner::new(r#""0.06824000""#);
        let fp = s.read_quoted_decimal().unwrap();
        assert_eq!(fp, FixedPoint::parse("0.06824000").unwrap());
    }

    #[test]
    fn read_quoted_decimal_integer_only() {
        let mut s = Scanner::new(r#""42""#);
        let fp = s.read_quoted_decimal().unwrap();
        assert_eq!(fp, FixedPoint::parse("42").unwrap());
    }

    #[test]
    fn read_quoted_decimal_zero() {
        let mut s = Scanner::new(r#""0.00000000""#);
        let fp = s.read_quoted_decimal().unwrap();
        assert_eq!(fp.raw(), 0);
    }

    // ── Fused read_raw_levels tests ──────────────────────────────────────

    #[test]
    fn read_raw_levels_filters_zero_amount() {
        let json = r#"[["100.0","5.0"],["99.0","0.00000000"],["98.0","3.0"]]"#;
        let mut s = Scanner::new(json);
        let levels = read_raw_levels::<20>(&mut s).unwrap();
        assert_eq!(levels.len(), 2);
        assert_eq!(levels[0].price, FixedPoint::parse("100.0").unwrap());
        assert_eq!(levels[1].price, FixedPoint::parse("98.0").unwrap());
    }

    #[test]
    fn read_raw_levels_rejects_malformed() {
        for (input, label) in [
            (r#"[["not_a_number","1.0"]]"#, "bad price"),
            (r#"[["100.0","xyz"]]"#, "bad amount"),
        ] {
            let mut s = Scanner::new(input);
            assert!(
                read_raw_levels::<20>(&mut s).is_none(),
                "expected None for {label}: {input:?}"
            );
        }
    }

    #[test]
    fn read_raw_levels_empty() {
        let mut s = Scanner::new("[]");
        let levels = read_raw_levels::<20>(&mut s).unwrap();
        assert!(levels.is_empty());
    }

    #[test]
    fn read_raw_levels_caps_at_n() {
        let pairs: Vec<String> = (0..30)
            .map(|i| format!(r#"["{}.0","1.0"]"#, 100 + i))
            .collect();
        let json = format!("[{}]", pairs.join(","));
        let mut s = Scanner::new(&json);
        let levels = read_raw_levels::<5>(&mut s).unwrap();
        assert_eq!(levels.len(), 5);
    }
}
