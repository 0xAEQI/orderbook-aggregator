//! Custom byte walker for zero-copy JSON parsing.
//!
//! Uses SIMD-accelerated substring search (`memchr::memmem`) to seek directly to
//! `"bids":` and `"asks":` in the JSON buffer — skips all envelope fields without
//! parsing them. Searches are chained forward (each starts from the scanner's
//! current position) so the buffer is scanned at most once.
//!
//! Exchange JSON has a fixed schema (bids before asks, event before data) so
//! key-name collisions inside string values are impossible; if one ever occurred,
//! `read_levels` would fail and return `None`.
//!
//! Input `&str` (guaranteed UTF-8 from WS text frames) is scanned by byte offset;
//! price/qty strings are returned as `&str` slices — no buffer mutation, no heap
//! allocation.

use arrayvec::ArrayVec;
use memchr::memmem;

use crate::types::MAX_LEVELS;

/// Borrowed price/qty string pairs extracted from a JSON array.
type Levels<'a> = ArrayVec<[&'a str; 2], MAX_LEVELS>;

/// Pre-computed SIMD searchers for JSON key patterns. Built once, reused across
/// all calls — `memmem::Finder` selects the optimal SIMD algorithm (SSE2/AVX2)
/// at construction time based on pattern length and CPU features.
struct Patterns {
    bids: memmem::Finder<'static>,
    asks: memmem::Finder<'static>,
    event: memmem::Finder<'static>,
    last_update_id: memmem::Finder<'static>,
}

/// Singleton pattern table — zero runtime cost after first use.
fn patterns() -> &'static Patterns {
    use std::sync::OnceLock;
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        bids: memmem::Finder::new(b"\"bids\":"),
        asks: memmem::Finder::new(b"\"asks\":"),
        event: memmem::Finder::new(b"\"event\":"),
        last_update_id: memmem::Finder::new(b"\"lastUpdateId\":"),
    })
}

/// SIMD-accelerated pattern search in `buf[start..]`. Returns absolute position
/// after the pattern. Uses vectorized two-byte or multi-byte algorithms from
/// `memchr::memmem` — typically 4-8× faster than byte-by-byte scanning on `x86_64`.
#[inline]
fn find_after_simd(buf: &[u8], start: usize, finder: &memmem::Finder<'_>) -> Option<usize> {
    let needle_len = finder.needle().len();
    finder.find(&buf[start..]).map(|pos| start + pos + needle_len)
}

/// Byte scanner that tracks position in an input buffer.
struct Scanner<'a> {
    buf: &'a [u8],
    src: &'a str,
    pos: usize,
}

impl<'a> Scanner<'a> {
    #[inline]
    fn peek(&self) -> Option<u8> {
        self.buf.get(self.pos).copied()
    }

    #[inline]
    fn skip_ws(&mut self) {
        while self.pos < self.buf.len() {
            match self.buf[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => return,
            }
        }
    }

    #[inline]
    fn expect(&mut self, byte: u8) -> bool {
        self.skip_ws();
        if self.pos < self.buf.len() && self.buf[self.pos] == byte {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Extract the content of a JSON string value as a borrowed `&str`.
    #[inline]
    fn read_string(&mut self) -> Option<&'a str> {
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
                b'\\' => self.pos += 2,
                _ => self.pos += 1,
            }
        }
        None
    }
}

/// Read an array of `[price, qty]` pairs, keeping the first `N`.
///
/// Once at capacity, returns immediately — the caller's `find_after_simd` will
/// skip past the remaining elements to the next key. No drain loop needed:
/// level data contains only decimal strings, so `"asks":` can never
/// false-match inside it.
fn read_levels<'a, const N: usize>(s: &mut Scanner<'a>) -> Option<ArrayVec<[&'a str; 2], N>> {
    if !s.expect(b'[') {
        return None;
    }
    let mut levels = ArrayVec::new();
    s.skip_ws();
    if s.peek() == Some(b']') {
        s.pos += 1;
        return Some(levels);
    }
    loop {
        s.skip_ws();
        if !s.expect(b'[') {
            return None;
        }
        let price = s.read_string()?;
        if !s.expect(b',') {
            return None;
        }
        let qty = s.read_string()?;
        if !s.expect(b']') {
            return None;
        }
        levels.push([price, qty]);
        s.skip_ws();
        match s.peek() {
            Some(b',') => {
                s.pos += 1;
                if levels.len() == N {
                    // At capacity — bail out. Caller's find_after_simd skips the rest.
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

/// Walk a Binance `depth20` JSON frame and extract sequence ID + bids/asks.
///
/// Extracts `lastUpdateId` for sequence gap detection, then seeks to `"bids":`
/// and `"asks":` — single pass. Pattern search uses SIMD (SSE2/AVX2) via
/// `memchr::memmem`.
#[must_use]
pub fn walk_binance(json: &str) -> Option<(u64, Levels<'_>, Levels<'_>)> {
    let buf = json.as_bytes();
    let p = patterns();
    let mut s = Scanner { buf, src: json, pos: 0 };

    // Extract lastUpdateId for sequence tracking.
    let seq = parse_last_update_id(buf, p);

    s.pos = find_after_simd(buf, 0, &p.bids)?;
    let bids = read_levels(&mut s)?;

    // Forward search — starts after bids array, skips ~800 bytes of re-scan.
    s.pos = find_after_simd(buf, s.pos, &p.asks)?;
    let asks = read_levels(&mut s)?;

    Some((seq, bids, asks))
}

/// Parse `lastUpdateId` from Binance depth JSON. Returns 0 if not found.
///
/// Uses a lightweight byte scan (pattern is only in Binance payloads, never
/// ambiguous). Falls back to 0 so callers can still process the book even
/// if the field format changes.
#[inline]
fn parse_last_update_id(buf: &[u8], p: &Patterns) -> u64 {
    let Some(pos) = find_after_simd(buf, 0, &p.last_update_id) else {
        return 0;
    };
    // Skip whitespace, then parse decimal digits.
    let mut i = pos;
    while i < buf.len() && buf[i] == b' ' {
        i += 1;
    }
    let mut val: u64 = 0;
    while i < buf.len() {
        let d = buf[i].wrapping_sub(b'0');
        if d > 9 {
            break;
        }
        val = val.wrapping_mul(10).wrapping_add(u64::from(d));
        i += 1;
    }
    val
}

/// Walk a Bitstamp `order_book` JSON message and extract event + bids/asks.
///
/// Seeks to `"event":` then forward to `"bids":` and `"asks":` — single pass.
/// Pattern search uses SIMD (SSE2/AVX2) via `memchr::memmem`.
/// For non-data events (`subscription_succeeded`, error), bids/asks may not exist
/// in the payload — returns empty levels.
#[must_use]
pub fn walk_bitstamp(json: &str) -> Option<(&str, Levels<'_>, Levels<'_>)> {
    let buf = json.as_bytes();
    let p = patterns();
    let mut s = Scanner { buf, src: json, pos: 0 };

    // Extract event string.
    s.pos = find_after_simd(buf, 0, &p.event)?;
    let event = s.read_string()?;

    // For non-data events, bids/asks may not exist — return empty.
    let bids = if let Some(pos) = find_after_simd(buf, s.pos, &p.bids) {
        s.pos = pos;
        read_levels(&mut s)?
    } else {
        Levels::new()
    };

    let asks = if let Some(pos) = find_after_simd(buf, s.pos, &p.asks) {
        s.pos = pos;
        read_levels(&mut s)?
    } else {
        Levels::new()
    };

    Some((event, bids, asks))
}

/// Extract a string value by key pattern from a JSON payload (cold path).
///
/// Used for error messages, channel names, etc. — not on the hot path.
/// `pattern` should include the key and colon, e.g. `b"\"message\":"`.
#[must_use]
pub fn extract_string<'a>(json: &'a str, pattern: &[u8]) -> Option<&'a str> {
    let buf = json.as_bytes();
    let finder = memmem::Finder::new(pattern);
    let pos = find_after_simd(buf, 0, &finder)?;
    let mut s = Scanner { buf, src: json, pos };
    s.read_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Binance ──────────────────────────────────────────────────────────

    #[test]
    fn binance_happy_path() {
        let json = r#"{"lastUpdateId":123,"bids":[["0.06824","12.5"],["0.06823","8.3"],["0.06822","5.0"]],"asks":[["0.06825","10.0"],["0.06826","7.2"],["0.06827","3.5"]]}"#;
        let (seq, bids, asks) = walk_binance(json).expect("valid JSON");
        assert_eq!(seq, 123);
        assert_eq!(bids.len(), 3);
        assert_eq!(asks.len(), 3);
        assert_eq!(bids[0], ["0.06824", "12.5"]);
        assert_eq!(bids[2], ["0.06822", "5.0"]);
        assert_eq!(asks[0], ["0.06825", "10.0"]);
        assert_eq!(asks[2], ["0.06827", "3.5"]);
    }

    #[test]
    fn binance_unknown_fields() {
        // Extra fields before, between, and after bids/asks — all skipped by pattern seek.
        let json = r#"{"lastUpdateId":999,"E":1234567890,"bids":[["1.0","2.0"]],"extra":"value","asks":[["3.0","4.0"]],"trailing":true}"#;
        let (seq, bids, asks) = walk_binance(json).expect("should skip unknown fields");
        assert_eq!(seq, 999);
        assert_eq!(bids.len(), 1);
        assert_eq!(asks.len(), 1);
        assert_eq!(bids[0], ["1.0", "2.0"]);
        assert_eq!(asks[0], ["3.0", "4.0"]);
    }

    #[test]
    fn binance_empty_levels() {
        let json = r#"{"bids":[],"asks":[]}"#;
        let (seq, bids, asks) = walk_binance(json).expect("empty arrays are valid");
        assert_eq!(seq, 0); // no lastUpdateId field → 0
        assert!(bids.is_empty());
        assert!(asks.is_empty());
    }

    // ── Bitstamp ─────────────────────────────────────────────────────────

    #[test]
    fn bitstamp_data_event() {
        let json = r#"{
            "event": "data",
            "channel": "order_book_ethbtc",
            "data": {
                "timestamp": "1700000000",
                "microtimestamp": "1700000000000000",
                "bids": [["0.06824","12.5"],["0.06823","8.3"],["0.06822","5.0"]],
                "asks": [["0.06825","10.0"],["0.06826","7.2"],["0.06827","3.5"]]
            }
        }"#;
        let (event, bids, asks) = walk_bitstamp(json).expect("valid JSON");
        assert_eq!(event, "data");
        assert_eq!(bids.len(), 3);
        assert_eq!(asks.len(), 3);
        assert_eq!(bids[0], ["0.06824", "12.5"]);
        assert_eq!(asks[0], ["0.06825", "10.0"]);
    }

    #[test]
    fn bitstamp_non_data_event() {
        let json = r#"{
            "event": "bts:subscription_succeeded",
            "channel": "order_book_ethbtc",
            "data": {}
        }"#;
        let (event, bids, asks) = walk_bitstamp(json).expect("valid JSON");
        assert_eq!(event, "bts:subscription_succeeded");
        assert!(bids.is_empty());
        assert!(asks.is_empty());
    }

    #[test]
    fn bitstamp_level_capping() {
        // 50 levels — should keep only MAX_LEVELS (20).
        let levels: Vec<String> = (0..50)
            .map(|i| format!("[\"{}.0\", \"1.0\"]", 100 + i))
            .collect();
        let json_array = levels.join(",");
        let json = format!(
            r#"{{"event":"data","data":{{"bids":[{json_array}],"asks":[]}}}}"#,
        );
        let (event, bids, asks) = walk_bitstamp(&json).expect("valid JSON");
        assert_eq!(event, "data");
        assert_eq!(bids.len(), MAX_LEVELS);
        assert!(asks.is_empty());
        assert_eq!(bids[0], ["100.0", "1.0"]);
        assert_eq!(bids[MAX_LEVELS - 1], ["119.0", "1.0"]);
    }

    #[test]
    fn bitstamp_extra_fields_in_data() {
        // Extra fields in the data object — skipped by pattern seek.
        let json = r#"{"event":"data","channel":"order_book_ethbtc","data":{"timestamp":"1700000000","microtimestamp":"1700000000000000","bids":[["3.0","4.0"]],"asks":[["5.0","6.0"]]}}"#;
        let (event, bids, asks) = walk_bitstamp(json).expect("should skip extra data fields");
        assert_eq!(event, "data");
        assert_eq!(bids[0], ["3.0", "4.0"]);
        assert_eq!(asks[0], ["5.0", "6.0"]);
    }

    #[test]
    fn malformed_json_returns_none() {
        assert!(walk_binance("").is_none());
        assert!(walk_binance("{").is_none());
        assert!(walk_binance(r#"{"bids": not_an_array}"#).is_none());
        assert!(walk_binance("null").is_none());
        assert!(walk_bitstamp("").is_none());
        assert!(walk_bitstamp("{").is_none());
        assert!(walk_bitstamp(r#"{"event": 123}"#).is_none());
    }
}
