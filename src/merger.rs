//! Order book merger.
//!
//! Runs on a **dedicated OS thread** with a busy-poll loop over per-exchange
//! SPSC ring buffers (`rtrb`). No tokio runtime, no async overhead -- just
//! `pop()` + `core::hint::spin_loop()` (PAUSE on x86). Burns one CPU core
//! for minimum wake-up latency.
//!
//! Maintains the latest book per exchange and publishes merged [`Summary`]
//! snapshots via a `tokio::watch` channel (`send()` is sync -- no runtime
//! needed).
//!
//! Uses a k-way merge of pre-sorted exchange books: `O(DEPTH × k)` comparisons
//! instead of `O(n log n)` concat+sort. For 2 exchanges × 10 levels → top 10,
//! that's ~20 comparisons vs ~106.

use std::cmp::Ordering;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use arrayvec::ArrayVec;
use rtrb::Consumer;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::metrics::Metrics;
use crate::types::{DEPTH, ExchangeId, Level, OrderBook, RawLevel, Summary};

/// Derived from [`crate::EXCHANGES`]. Sizes stack-allocated arrays in
/// `BookStore` and `merge_top_n`.
const MAX_EXCHANGES: usize = crate::EXCHANGES.len();

/// Books older than this are evicted before merging -- prevents stale data from
/// one exchange contaminating the merged output after a disconnect. A crossed
/// book (negative spread) from 5-second-old data is worse than a single-exchange
/// book from fresh data.
///
/// 5s tolerates brief network jitter and reconnection delays while remaining
/// well under the threshold where stale prices create false arbitrage signals.
/// At Binance's 100ms update cadence, 5s ≈ 50 missed snapshots -- clearly a
/// disconnect, not transient jitter.
const STALE_THRESHOLD: Duration = Duration::from_secs(5);

/// Latest book per exchange. Direct-indexed by [`ExchangeId`] -- O(1) insert,
/// no linear scan. `exchange_id` IS the array index.
pub struct BookStore {
    books: [Option<OrderBook>; MAX_EXCHANGES],
}

impl Default for BookStore {
    fn default() -> Self {
        Self::new()
    }
}

impl BookStore {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            books: [const { None }; MAX_EXCHANGES],
        }
    }

    /// O(1) insert: `exchange_id` is the direct array index. No scan needed.
    #[inline]
    pub fn insert(&mut self, book: OrderBook) {
        let idx = book.exchange_id as usize;
        if let Some(slot) = self.books.get_mut(idx) {
            *slot = Some(book);
        } else {
            warn!(
                exchange_id = book.exchange_id,
                max = MAX_EXCHANGES,
                "exchange_id out of range, dropping (increase MAX_EXCHANGES)"
            );
        }
    }

    /// Iterate over populated books (stale books already evicted as `None`).
    #[inline]
    fn iter(&self) -> impl Iterator<Item = &OrderBook> {
        self.books.iter().filter_map(|b| b.as_ref())
    }

    /// Evict books older than `threshold`. Prevents stale data from a
    /// disconnected exchange from contaminating the merged output.
    fn evict_stale(&mut self, now: Instant, threshold: Duration) {
        for slot in &mut self.books {
            if let Some(book) = slot {
                let age = now - book.decode_start;
                if age > threshold {
                    warn!(
                        exchange_id = book.exchange_id,
                        age_ms = age.as_millis() as u64,
                        "evicting stale book"
                    );
                    *slot = None;
                }
            }
        }
    }
}

/// Runs the merger on a dedicated OS thread. Spin-polls SPSC ring buffers,
/// merges, and publishes via `watch` (sync send -- no tokio runtime needed).
///
/// Exits when all producers are dropped or cancellation is signalled.
pub fn run_spsc(
    mut consumers: Vec<Consumer<OrderBook>>,
    summary_tx: &watch::Sender<Summary>,
    metrics: &Metrics,
    cancel: &CancellationToken,
) {
    // Pin to the last available core -- isolates the merger from exchange threads
    // and tokio workers which naturally spread across the remaining cores.
    assert!(
        consumers.len() <= MAX_EXCHANGES,
        "too many exchanges ({}) for MAX_EXCHANGES ({MAX_EXCHANGES})",
        consumers.len(),
    );

    pin_to_last_core();

    let mut books = BookStore::new();

    info!(
        "merger started (SPSC busy-poll, {} consumers)",
        consumers.len()
    );

    loop {
        if cancel.is_cancelled() {
            info!("merger cancelled");
            break;
        }

        // All producers dropped → no more data.
        if consumers.iter().all(Consumer::is_abandoned) {
            info!("all producers dropped, merger exiting");
            break;
        }

        // One merge + publish per input: every exchange update produces a new
        // merged summary on the gRPC stream. This ensures no updates are silently
        // collapsed and every book's E2E latency is individually recorded.
        // Sequential scan across consumers -- for k=2, ordering bias is negligible
        // and sequential access is more cache-friendly than true round-robin.
        let mut got_any = false;
        for consumer in &mut consumers {
            if let Ok(book) = consumer.pop() {
                got_any = true;
                let decode_start = book.decode_start;
                books.insert(book);

                let t0 = Instant::now();
                books.evict_stale(t0, STALE_THRESHOLD);
                let summary = merge(&books);
                let _ = summary_tx.send(summary);
                // Merge latency measured offline by Criterion benchmarks -- keeping
                // it off the hot path saves 3 atomic ops + bucket scan (~25ns).
                metrics.e2e_latency.record(decode_start.elapsed());
                metrics.merges.fetch_add(1, Relaxed);
            }
        }

        if !got_any {
            core::hint::spin_loop();
        }
    }

    info!("merger exiting");
}

/// Tagged slice: exchange ID + its `RawLevel` slice. Avoids storing exchange
/// on every level pre-merge; the merge stamps it onto the output `Level`.
struct TaggedSlice<'a> {
    exchange_id: ExchangeId,
    levels: &'a [RawLevel],
}

/// K-way merge of pre-sorted `RawLevel` slices → top `n` `Level`s (with exchange).
/// O(n × k) comparisons, zero alloc.
#[inline]
fn merge_top_n(
    slices: &[TaggedSlice<'_>],
    cmp: impl Fn(&RawLevel, &RawLevel) -> Ordering,
    n: usize,
) -> ArrayVec<Level, DEPTH> {
    let k = slices.len();
    debug_assert!(k <= MAX_EXCHANGES);
    let mut cursors = [0usize; MAX_EXCHANGES];
    let mut result = ArrayVec::new();

    for _ in 0..n {
        let mut best: Option<usize> = None;
        for i in 0..k {
            if cursors[i] < slices[i].levels.len() {
                best = Some(match best {
                    None => i,
                    Some(b)
                        if cmp(&slices[i].levels[cursors[i]], &slices[b].levels[cursors[b]])
                            .is_lt() =>
                    {
                        i
                    }
                    Some(b) => b,
                });
            }
        }
        let Some(i) = best else { break };
        let raw = &slices[i].levels[cursors[i]];
        if result
            .try_push(Level {
                price: raw.price,
                amount: raw.amount,
                exchange_id: slices[i].exchange_id,
            })
            .is_err()
        {
            break;
        }
        cursors[i] += 1;
    }

    result
}

/// Merge all exchange order books into a single [`Summary`].
#[inline]
#[must_use]
pub fn merge(books: &BookStore) -> Summary {
    let mut bid_slices: ArrayVec<TaggedSlice<'_>, MAX_EXCHANGES> = ArrayVec::new();
    let mut ask_slices: ArrayVec<TaggedSlice<'_>, MAX_EXCHANGES> = ArrayVec::new();
    for book in books.iter() {
        let _ = bid_slices.try_push(TaggedSlice {
            exchange_id: book.exchange_id,
            levels: &book.bids,
        });
        let _ = ask_slices.try_push(TaggedSlice {
            exchange_id: book.exchange_id,
            levels: &book.asks,
        });
    }

    // Bids: highest price first, then largest amount as tiebreaker.
    let bids = merge_top_n(
        &bid_slices,
        |a, b| b.price.cmp(&a.price).then(b.amount.cmp(&a.amount)),
        DEPTH,
    );

    // Asks: lowest price first, then largest amount as tiebreaker.
    let asks = merge_top_n(
        &ask_slices,
        |a, b| a.price.cmp(&b.price).then(b.amount.cmp(&a.amount)),
        DEPTH,
    );

    // Spread in raw FixedPoint units -- no f64 until proto conversion (cold path).
    let spread_raw = match (asks.first(), bids.first()) {
        #[expect(clippy::cast_possible_wrap)] // Intentional: prices fit in i63.
        (Some(ask), Some(bid)) => ask.price.raw() as i64 - bid.price.raw() as i64,
        _ => 0,
    };

    Summary {
        spread_raw,
        bids,
        asks,
    }
}

/// Pin the current thread to the last available CPU core.
///
/// Uses `libc::sched_setaffinity` directly on Linux -- no external crate needed.
/// No-op on non-Linux platforms.
///
/// # Safety
/// All `libc` calls are well-formed: `sysconf` with a valid constant,
/// `CPU_SET` with a valid core index < CPU count, `sched_setaffinity` on
/// the current thread (pid=0) with a properly zeroed `cpu_set_t`.
#[expect(unsafe_code)]
#[cold]
fn pin_to_last_core() {
    #[cfg(target_os = "linux")]
    {
        let cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
        if cpus > 0 {
            #[expect(clippy::cast_sign_loss)] // Guarded by cpus > 0 check above.
            let core = (cpus - 1) as usize;
            let mut set = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
            unsafe { libc::CPU_SET(core, &mut set) };
            let ret =
                unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &raw const set) };
            if ret == 0 {
                info!(core_id = core, "merger pinned to core");
            } else {
                warn!(core_id = core, "failed to pin merger to core");
            }
        } else {
            warn!("could not determine CPU count; merger running unpinned");
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        warn!("core pinning not supported on this platform; merger running unpinned");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{book, raw};
    use crate::types::{DEPTH, FixedPoint};

    #[test]
    fn merge_two_exchanges() {
        let mut books = BookStore::new();

        books.insert(book(
            0,
            &[raw(100.0, 5.0), raw(99.0, 3.0)],
            &[raw(101.0, 4.0), raw(102.0, 2.0)],
        ));

        books.insert(book(
            1,
            &[raw(100.5, 2.0), raw(99.5, 1.0)],
            &[raw(100.8, 3.0), raw(101.5, 1.0)],
        ));

        let summary = merge(&books);

        // Best bid should be exchange 1 at 100.5.
        assert_eq!(summary.bids[0].exchange_id, 1);
        assert_eq!(summary.bids[0].price, FixedPoint::from_f64(100.5));

        // Best ask should be exchange 1 at 100.8.
        assert_eq!(summary.asks[0].exchange_id, 1);
        assert_eq!(summary.asks[0].price, FixedPoint::from_f64(100.8));

        // Spread = best ask - best bid = 100.8 - 100.5 = 0.3 (30_000_000 raw units).
        assert_eq!(summary.spread_raw, 30_000_000);

        // All 4 levels on each side.
        assert_eq!(summary.bids.len(), 4);
        assert_eq!(summary.asks.len(), 4);
    }

    #[test]
    fn merge_truncates_to_top_n() {
        let mut books = BookStore::new();

        // Two exchanges each at DEPTH capacity → 2×DEPTH input → DEPTH output.
        books.insert(book(
            0,
            &(0..DEPTH)
                .map(|i| raw(100.0 - i as f64, 1.0))
                .collect::<Vec<_>>(),
            &(0..DEPTH)
                .map(|i| raw(101.0 + i as f64, 1.0))
                .collect::<Vec<_>>(),
        ));
        books.insert(book(
            1,
            &(0..DEPTH)
                .map(|i| raw(99.5 - i as f64, 1.0))
                .collect::<Vec<_>>(),
            &(0..DEPTH)
                .map(|i| raw(101.5 + i as f64, 1.0))
                .collect::<Vec<_>>(),
        ));

        let summary = merge(&books);
        assert_eq!(summary.bids.len(), DEPTH);
        assert_eq!(summary.asks.len(), DEPTH);
    }

    #[test]
    fn merge_empty() {
        let books = BookStore::new();

        let summary = merge(&books);
        assert!(summary.bids.is_empty());
        assert!(summary.asks.is_empty());
        assert_eq!(summary.spread_raw, 0);
    }

    #[test]
    fn bid_tiebreak_by_amount() {
        let mut books = BookStore::new();

        // Same price from two exchanges -- larger amount should come first.
        books.insert(book(0, &[raw(100.0, 1.0)], &[]));
        books.insert(book(1, &[raw(100.0, 5.0)], &[]));

        let summary = merge(&books);
        assert_eq!(summary.bids[0].amount, FixedPoint::from_f64(5.0));
        assert_eq!(summary.bids[1].amount, FixedPoint::from_f64(1.0));
    }

    #[test]
    fn single_exchange_only() {
        let mut books = BookStore::new();

        // Only one exchange connected -- common during startup and reconnection.
        books.insert(book(
            0,
            &[raw(100.0, 5.0), raw(99.0, 3.0)],
            &[raw(101.0, 4.0), raw(102.0, 2.0)],
        ));

        let summary = merge(&books);
        assert_eq!(summary.bids.len(), 2);
        assert_eq!(summary.asks.len(), 2);
        assert_eq!(summary.bids[0].price, FixedPoint::from_f64(100.0));
        assert_eq!(summary.asks[0].price, FixedPoint::from_f64(101.0));
        // Spread = 101.0 - 100.0 = 1.0 (100_000_000 raw units).
        assert_eq!(summary.spread_raw, 100_000_000);
    }

    #[test]
    fn crossed_book_negative_spread() {
        let mut books = BookStore::new();

        // Exchange 0's best bid (102) exceeds Exchange 1's best ask (101) -- crossed book.
        books.insert(book(0, &[raw(102.0, 1.0)], &[raw(103.0, 1.0)]));
        books.insert(book(1, &[raw(99.0, 1.0)], &[raw(101.0, 1.0)]));

        let summary = merge(&books);
        assert_eq!(summary.bids[0].price, FixedPoint::from_f64(102.0));
        assert_eq!(summary.asks[0].price, FixedPoint::from_f64(101.0));
        // Spread is negative -- signals an arbitrage opportunity.
        assert!(summary.spread_raw < 0);
        // 101.0 - 102.0 = -1.0 (-100_000_000 raw units).
        assert_eq!(summary.spread_raw, -100_000_000);
    }

    #[test]
    fn ask_tiebreak_by_amount() {
        let mut books = BookStore::new();

        // Same ask price from two exchanges -- larger amount should come first.
        books.insert(book(0, &[], &[raw(100.0, 1.0)]));
        books.insert(book(1, &[], &[raw(100.0, 5.0)]));

        let summary = merge(&books);
        assert_eq!(summary.asks[0].amount, FixedPoint::from_f64(5.0));
        assert_eq!(summary.asks[1].amount, FixedPoint::from_f64(1.0));
    }

    #[test]
    fn kway_merge_interleaves() {
        let mut books = BookStore::new();

        // Exchange 0: 100, 98, 96   Exchange 1: 99, 97, 95
        // Expected merged bids: 100, 99, 98, 97, 96, 95
        books.insert(book(
            0,
            &[raw(100.0, 1.0), raw(98.0, 1.0), raw(96.0, 1.0)],
            &[],
        ));
        books.insert(book(
            1,
            &[raw(99.0, 1.0), raw(97.0, 1.0), raw(95.0, 1.0)],
            &[],
        ));

        let summary = merge(&books);
        let prices: Vec<f64> = summary.bids.iter().map(|l| l.price.to_f64()).collect();
        assert_eq!(prices, vec![100.0, 99.0, 98.0, 97.0, 96.0, 95.0]);
    }

    #[test]
    fn stale_book_eviction() {
        let mut books = BookStore::new();

        // Insert a book with a very old decode_start.
        let old_book = OrderBook {
            exchange_id: 0,
            bids: [raw(100.0, 1.0)].into_iter().collect(),
            asks: [raw(101.0, 1.0)].into_iter().collect(),
            decode_start: Instant::now().checked_sub(Duration::from_secs(10)).unwrap(),
        };
        books.insert(old_book);

        // Fresh book.
        books.insert(book(1, &[raw(99.0, 2.0)], &[raw(102.0, 3.0)]));

        books.evict_stale(Instant::now(), STALE_THRESHOLD);
        let summary = merge(&books);

        // Only fresh exchange should survive.
        assert_eq!(summary.bids.len(), 1);
        assert_eq!(summary.bids[0].exchange_id, 1);
    }

    #[test]
    fn bookstore_insert_updates_existing() {
        let mut books = BookStore::new();

        books.insert(book(0, &[raw(100.0, 1.0)], &[raw(101.0, 1.0)]));
        // Second insert with same exchange ID should overwrite.
        books.insert(book(0, &[raw(200.0, 5.0)], &[raw(201.0, 5.0)]));

        let summary = merge(&books);
        // Should see the updated price, not the original.
        assert_eq!(summary.bids.len(), 1);
        assert_eq!(summary.bids[0].price, FixedPoint::from_f64(200.0));
        assert_eq!(summary.asks[0].price, FixedPoint::from_f64(201.0));
    }

    #[test]
    fn bookstore_insert_out_of_range() {
        let mut books = BookStore::new();

        // Valid insert.
        books.insert(book(0, &[raw(100.0, 1.0)], &[]));

        // exchange_id >= MAX_EXCHANGES should be silently dropped.
        books.insert(book(99, &[raw(50.0, 1.0)], &[]));

        let summary = merge(&books);
        assert_eq!(summary.bids.len(), 1);
        assert_eq!(summary.bids[0].exchange_id, 0);
    }

    #[test]
    fn bookstore_evict_stale_keeps_fresh() {
        let mut books = BookStore::new();

        // Insert two fresh books (decode_start = now).
        books.insert(book(0, &[raw(100.0, 1.0)], &[raw(101.0, 1.0)]));
        books.insert(book(1, &[raw(99.0, 2.0)], &[raw(102.0, 2.0)]));

        // Evict with current time -- neither should be evicted.
        books.evict_stale(Instant::now(), STALE_THRESHOLD);
        let summary = merge(&books);
        assert_eq!(summary.bids.len(), 2);
        assert_eq!(summary.asks.len(), 2);
    }

    #[test]
    fn bookstore_reinsert_after_eviction() {
        let mut books = BookStore::new();

        // Insert a stale book.
        let old_book = OrderBook {
            exchange_id: 0,
            bids: [raw(100.0, 1.0)].into_iter().collect(),
            asks: [raw(101.0, 1.0)].into_iter().collect(),
            decode_start: Instant::now().checked_sub(Duration::from_secs(10)).unwrap(),
        };
        books.insert(old_book);

        // Evict it.
        books.evict_stale(Instant::now(), STALE_THRESHOLD);
        let summary = merge(&books);
        assert!(summary.bids.is_empty());

        // Re-insert a fresh book for the same exchange.
        books.insert(book(0, &[raw(200.0, 5.0)], &[raw(201.0, 5.0)]));
        let summary = merge(&books);
        assert_eq!(summary.bids.len(), 1);
        assert_eq!(summary.bids[0].price, FixedPoint::from_f64(200.0));
    }

    #[test]
    fn merge_asymmetric_empty_sides() {
        let mut books = BookStore::new();

        // Exchange 0: bids only, no asks.
        books.insert(book(0, &[raw(100.0, 1.0)], &[]));
        // Exchange 1: asks only, no bids.
        books.insert(book(1, &[], &[raw(101.0, 1.0)]));

        let summary = merge(&books);
        assert_eq!(summary.bids.len(), 1);
        assert_eq!(summary.asks.len(), 1);
        // Spread still computed correctly from the only bid and only ask.
        assert_eq!(summary.spread_raw, 100_000_000); // 101 - 100 = 1.0
    }

    #[test]
    fn merge_single_side_only() {
        let mut books = BookStore::new();

        // Only bids, no asks from any exchange → spread = 0 (no ask).
        books.insert(book(0, &[raw(100.0, 1.0), raw(99.0, 2.0)], &[]));
        books.insert(book(1, &[raw(100.5, 3.0)], &[]));

        let summary = merge(&books);
        assert_eq!(summary.bids.len(), 3);
        assert!(summary.asks.is_empty());
        assert_eq!(summary.spread_raw, 0);
    }
}
