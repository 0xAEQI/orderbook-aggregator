//! Exchange WebSocket adapters.
//!
//! Each adapter runs on a **dedicated OS thread** with its own single-threaded
//! tokio runtime — isolates WS I/O from the main runtime, eliminates
//! work-stealing scheduler jitter. Both exchanges provide full snapshots
//! (not incremental diffs), so no local order book maintenance or sequence
//! reconciliation is required.
//!
//! Adapters push [`OrderBook`] snapshots into a per-exchange **SPSC ring buffer**
//! (`rtrb`). The ring uses only `store(Release)` / `load(Acquire)` — no CAS,
//! no contention with the merger thread.
//!
//! Connections use `TCP_NODELAY` and `write_buffer_size: 0` for immediate frame
//! flushing.

pub mod binance;
pub mod bitstamp;

use std::time::{Duration, Instant};

use arrayvec::ArrayVec;
use rtrb::Producer;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::types::{FixedPoint, Level, MAX_LEVELS, OrderBook};

/// Connection timeout for WebSocket handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum reconnection backoff (shared across all adapters).
const MAX_BACKOFF_MS: u64 = 30_000;

/// Initial reconnection backoff before exponential increase.
const INITIAL_BACKOFF_MS: u64 = 1_000;

/// Exchange adapter. Handles reconnection internally; respects cancellation.
pub trait Exchange: Send + Sync + 'static {
    fn connect(
        &self,
        symbol: String,
        producer: Producer<OrderBook>,
        cancel: CancellationToken,
    ) -> impl std::future::Future<Output = Result<()>> + Send;
}

/// WebSocket config: `write_buffer_size: 0` for immediate frame flushing.
#[must_use]
pub fn ws_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
        write_buffer_size: 0, // Flush every frame immediately.
        ..Default::default()
    }
}

/// `[price, qty]` string pairs → `ArrayVec<Level>`. Returns `None` on any malformed pair.
#[inline]
fn parse_levels(exchange: &'static str, raw: &[[&str; 2]]) -> Option<ArrayVec<Level, MAX_LEVELS>> {
    let mut levels = ArrayVec::new();
    for &[price, amount] in raw {
        let p = FixedPoint::parse(price)?;
        let a = FixedPoint::parse(amount)?;
        if levels
            .try_push(Level {
                exchange,
                price: p,
                amount: a,
            })
            .is_err()
        {
            break;
        }
    }
    Some(levels)
}

/// Parse walker output into an `OrderBook`. Returns `None` if any level is malformed.
#[inline]
#[must_use]
pub fn build_book(
    exchange: &'static str,
    bids: &[[&str; 2]],
    asks: &[[&str; 2]],
    decode_start: Instant,
) -> Option<OrderBook> {
    Some(OrderBook {
        exchange,
        bids: parse_levels(exchange, bids)?,
        asks: parse_levels(exchange, asks)?,
        decode_start,
    })
}

/// Push book into SPSC ring. Drops on full (stale data is correct to drop for
/// order books — next snapshot supersedes). Returns `false` if consumer dropped.
#[inline]
pub fn try_send_book(
    producer: &mut Producer<OrderBook>,
    book: OrderBook,
    exchange: &'static str,
) -> bool {
    if producer.is_abandoned() {
        tracing::warn!(exchange, "consumer dropped, stopping");
        return false;
    }
    match producer.push(book) {
        Ok(()) => true,
        Err(rtrb::PushError::Full(_)) => {
            tracing::warn!(exchange, "ring full, dropping snapshot");
            true
        }
    }
}

/// Exponential backoff with jitter. Returns `false` on cancellation.
pub async fn backoff_sleep(
    backoff_ms: &mut u64,
    max_ms: u64,
    exchange: &'static str,
    cancel: &CancellationToken,
) -> bool {
    let jitter = fastrand::u64(0..(*backoff_ms / 2).max(1));
    let delay = *backoff_ms + jitter;
    tracing::warn!(exchange, delay_ms = delay, "reconnecting");
    tokio::select! {
        _ = cancel.cancelled() => return false,
        _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
    }
    *backoff_ms = (*backoff_ms * 2).min(max_ms);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FixedPoint;

    fn test_book(exchange: &'static str) -> OrderBook {
        let mut bids = ArrayVec::new();
        bids.push(Level {
            exchange,
            price: FixedPoint::from_f64(100.0),
            amount: FixedPoint::from_f64(1.0),
        });
        let mut asks = ArrayVec::new();
        asks.push(Level {
            exchange,
            price: FixedPoint::from_f64(101.0),
            amount: FixedPoint::from_f64(1.0),
        });
        OrderBook {
            exchange,
            bids,
            asks,
            decode_start: Instant::now(),
        }
    }

    #[test]
    fn try_send_ring_full_drops() {
        // 1-slot ring: push one to fill it, then push another — should drop, not error.
        let (mut producer, _consumer) = rtrb::RingBuffer::new(1);
        assert!(try_send_book(&mut producer, test_book("a"), "a"));
        // Ring is now full — next push drops the book but returns true.
        assert!(try_send_book(&mut producer, test_book("a"), "a"));
    }

    #[test]
    fn try_send_abandoned_returns_false() {
        let (mut producer, consumer) = rtrb::RingBuffer::new(4);
        drop(consumer); // Abandon the consumer.
        assert!(!try_send_book(&mut producer, test_book("a"), "a"));
    }
}
