//! Shared scanner utilities for zero-copy JSON parsing.
//!
//! Provides a byte-level `Scanner` and `read_levels` helper used by per-exchange
//! parsers in their respective adapter modules. Uses SIMD-accelerated substring
//! search (`memchr::memmem`) for key lookup.
//!
//! Input `&str` (guaranteed UTF-8 from WS text frames) is scanned by byte offset;
//! price/qty strings are returned as `&str` slices -- no buffer mutation, no heap
//! allocation.

use arrayvec::ArrayVec;
use memchr::memmem;

use crate::types::MAX_LEVELS;

/// Borrowed price/qty string pairs extracted from a JSON array.
pub(crate) type Levels<'a> = ArrayVec<[&'a str; 2], MAX_LEVELS>;

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

    #[inline]
    pub(crate) fn expect(&mut self, byte: u8) -> bool {
        self.skip_ws();
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
                b'\\' => self.pos += 2,
                _ => self.pos += 1,
            }
        }
        None
    }

    /// Read an unsigned integer value (JSON number). Returns 0 if not found.
    #[inline]
    pub(crate) fn read_u64(&mut self) -> u64 {
        self.skip_ws();
        let mut val: u64 = 0;
        while self.pos < self.buf.len() {
            let d = self.buf[self.pos].wrapping_sub(b'0');
            if d > 9 {
                break;
            }
            val = val.wrapping_mul(10).wrapping_add(u64::from(d));
            self.pos += 1;
        }
        val
    }

    /// Seek to a key pattern and read levels. Returns empty if key is absent.
    #[inline]
    pub(crate) fn read_optional_levels(&mut self, finder: &memmem::Finder<'_>) -> Option<Levels<'a>> {
        if self.seek(finder).is_some() {
            read_levels(self)
        } else {
            Some(Levels::new())
        }
    }
}

/// Read an array of `[price, qty]` pairs, keeping the first `N`.
///
/// Once at capacity, returns immediately -- the caller's `seek()` will
/// skip past the remaining elements to the next key. No drain loop needed:
/// level data contains only decimal strings, so `"asks":` can never
/// false-match inside it.
pub(crate) fn read_levels<'a, const N: usize>(s: &mut Scanner<'a>) -> Option<ArrayVec<[&'a str; 2], N>> {
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
                    // At capacity -- bail out. Caller's seek() skips the rest.
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
