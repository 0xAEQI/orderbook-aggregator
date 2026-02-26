//! Custom byte walker for zero-copy JSON parsing.
//!
//! Uses byte pattern search (`find_after`) to seek directly to `"bids":` and
//! `"asks":` in the JSON buffer — skips all envelope fields without parsing them.
//! Searches are chained forward (each starts from the scanner's current position)
//! so the buffer is scanned at most once.
//!
//! Exchange JSON has a fixed schema (bids before asks, event before data) so
//! key-name collisions inside string values are impossible; if one ever occurred,
//! `read_levels` would fail and return `None`.
//!
//! Input `&str` (guaranteed UTF-8 from WS text frames) is scanned by byte offset;
//! price/qty strings are returned as `&str` slices — no buffer mutation, no heap
//! allocation.

use arrayvec::ArrayVec;

use crate::types::MAX_LEVELS;

/// Borrowed price/qty string pairs extracted from a JSON array.
type Levels<'a> = ArrayVec<[&'a str; 2], MAX_LEVELS>;

/// Find `pattern` in `buf[start..]` and return the absolute position after it.
///
/// First-byte gate ensures the full comparison only fires on `"` boundaries.
#[inline]
fn find_after(buf: &[u8], start: usize, pattern: &[u8]) -> Option<usize> {
    let first = *pattern.first()?;
    let plen = pattern.len();
    let mut i = start;
    while i + plen <= buf.len() {
        if buf[i] == first && buf[i..i + plen] == *pattern {
            return Some(i + plen);
        }
        i += 1;
    }
    None
}

/// Byte scanner that tracks position in an input buffer.
struct Scanner<'a> {
    buf: &'a [u8],
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
                    let s = &self.buf[start..self.pos];
                    self.pos += 1;
                    // SAFETY: `buf` originates from `&str::as_bytes()` (guaranteed UTF-8
                    // by WebSocket text frame contract). `s` is a subslice of `buf`, so
                    // it is valid UTF-8. Debug builds verify this invariant.
                    #[allow(unsafe_code)]
                    {
                        debug_assert!(std::str::from_utf8(s).is_ok());
                        return Some(unsafe { std::str::from_utf8_unchecked(s) });
                    }
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
/// Once at capacity, returns immediately — the caller's `find_after` will
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
                    // At capacity — bail out. Caller's find_after skips the rest.
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

/// Walk a Binance `depth20` JSON frame and extract bids/asks string pairs.
///
/// Seeks directly to `"bids":` then forward to `"asks":` — single pass,
/// skips `lastUpdateId` and any other envelope fields without parsing them.
#[must_use]
pub fn walk_binance(json: &str) -> Option<(Levels<'_>, Levels<'_>)> {
    let buf = json.as_bytes();
    let mut s = Scanner { buf, pos: 0 };

    s.pos = find_after(buf, 0, b"\"bids\":")?;
    let bids = read_levels(&mut s)?;

    // Forward search — starts after bids array, skips ~800 bytes of re-scan.
    s.pos = find_after(buf, s.pos, b"\"asks\":")?;
    let asks = read_levels(&mut s)?;

    Some((bids, asks))
}

/// Walk a Bitstamp `order_book` JSON message and extract event + bids/asks.
///
/// Seeks to `"event":` then forward to `"bids":` and `"asks":` — single pass.
/// For non-data events (`subscription_succeeded`, error), bids/asks may not exist
/// in the payload — returns empty levels.
#[must_use]
pub fn walk_bitstamp(json: &str) -> Option<(&str, Levels<'_>, Levels<'_>)> {
    let buf = json.as_bytes();
    let mut s = Scanner { buf, pos: 0 };

    // Extract event string.
    s.pos = find_after(buf, 0, b"\"event\":")?;
    let event = s.read_string()?;

    // For non-data events, bids/asks may not exist — return empty.
    let bids = if let Some(pos) = find_after(buf, s.pos, b"\"bids\":") {
        s.pos = pos;
        read_levels(&mut s)?
    } else {
        Levels::new()
    };

    let asks = if let Some(pos) = find_after(buf, s.pos, b"\"asks\":") {
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
    let pos = find_after(buf, 0, pattern)?;
    let mut s = Scanner { buf, pos };
    s.read_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Binance ──────────────────────────────────────────────────────────

    #[test]
    fn binance_happy_path() {
        let json = r#"{"lastUpdateId":123,"bids":[["0.06824","12.5"],["0.06823","8.3"],["0.06822","5.0"]],"asks":[["0.06825","10.0"],["0.06826","7.2"],["0.06827","3.5"]]}"#;
        let (bids, asks) = walk_binance(json).expect("valid JSON");
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
        let (bids, asks) = walk_binance(json).expect("should skip unknown fields");
        assert_eq!(bids.len(), 1);
        assert_eq!(asks.len(), 1);
        assert_eq!(bids[0], ["1.0", "2.0"]);
        assert_eq!(asks[0], ["3.0", "4.0"]);
    }

    #[test]
    fn binance_empty_levels() {
        let json = r#"{"bids":[],"asks":[]}"#;
        let (bids, asks) = walk_binance(json).expect("empty arrays are valid");
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
