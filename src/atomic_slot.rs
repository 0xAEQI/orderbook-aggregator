//! Zero-allocation single-slot SPSC channel with latest-value overwrite semantics.
//!
//! Uses a **triple-buffer protocol**: producer and consumer each own one buffer
//! exclusively, the third serves as the atomic exchange point. One atomic swap
//! per operation, zero heap allocation after creation.
//!
//! This is the optimal primitive for streaming order book snapshots: the producer
//! always publishes the latest value, the consumer always reads the freshest
//! snapshot, and stale intermediates are silently overwritten in-place.
//!
//! Performance: no `Box::new` per send, no `Box::from_raw` per recv. The only
//! heap allocation is the one-time `Box::new(Shared)` at creation. Each `send()`
//! is a buffer write + one atomic swap. Each `recv()` is one atomic load (fast
//! check) + one atomic swap + a `.take()`.

use std::cell::{Cell, UnsafeCell};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU8, Ordering};

/// Dirty flag: bit 0 of the packed state byte.
const DIRTY: u8 = 1;

/// Sending half of the SPSC slot.
pub struct SlotSender<T> {
    shared: *mut Shared<T>,
    /// Producer's private buffer index (0, 1, or 2). Modified on each send
    /// via `Cell` for interior mutability (`send` takes `&self`).
    back: Cell<u8>,
    _marker: PhantomData<T>,
}

/// Receiving half of the SPSC slot.
pub struct SlotReceiver<T> {
    shared: *mut Shared<T>,
    /// Consumer's private buffer index (0, 1, or 2). Modified on each recv
    /// via `Cell` for interior mutability (`recv` takes `&self`).
    front: Cell<u8>,
    _marker: PhantomData<T>,
}

// Safety: T is Send. Only one thread uses the Sender, only one uses the Receiver.
// The triple-buffer protocol ensures no concurrent access to any buffer:
// producer exclusively owns `back`, consumer exclusively owns `front`,
// and `middle` is exchanged only via atomic swap.
#[expect(unsafe_code)]
unsafe impl<T: Send> Send for SlotSender<T> {}
#[expect(unsafe_code)]
unsafe impl<T: Send> Send for SlotReceiver<T> {}

/// Shared state: three pre-allocated buffers + packed atomic state.
struct Shared<T> {
    /// Triple buffer: three pre-allocated slots. Each is exclusively owned by
    /// the producer (back), the consumer (front), or the exchange point (middle).
    /// The triple-buffer invariant guarantees all three indices are always
    /// distinct -- no two roles share a buffer at any time.
    buffers: [UnsafeCell<Option<T>>; 3],
    /// Packed state byte:
    /// - bit 0: dirty flag (1 = new data available)
    /// - bits 1-2: middle buffer index (0, 1, or 2)
    state: AtomicU8,
    /// Reference count: starts at 2 (sender + receiver).
    refcount: AtomicU8,
}

/// Create a new SPSC slot, returning the sender and receiver halves.
///
/// Initial buffer assignment: back=0, middle=1, front=2.
#[must_use]
pub fn slot<T>() -> (SlotSender<T>, SlotReceiver<T>) {
    let shared = Box::into_raw(Box::new(Shared {
        buffers: [
            UnsafeCell::new(None),
            UnsafeCell::new(None),
            UnsafeCell::new(None),
        ],
        state: AtomicU8::new(1 << 1), // middle=1, dirty=0
        refcount: AtomicU8::new(2),
    }));
    let sender = SlotSender {
        shared,
        back: Cell::new(0),
        _marker: PhantomData,
    };
    let receiver = SlotReceiver {
        shared,
        front: Cell::new(2),
        _marker: PhantomData,
    };
    (sender, receiver)
}

#[expect(unsafe_code)]
fn drop_half<T>(shared: *mut Shared<T>) {
    // Safety: shared is valid until refcount hits 0.
    unsafe {
        let prev = (*shared).refcount.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            // Last half dropped -- clean up. Option<T> values in buffers
            // are dropped automatically by Box<Shared<T>>'s destructor.
            drop(Box::from_raw(shared));
        }
    }
}

impl<T> SlotSender<T> {
    /// Store a new value. If a previous value occupies the back buffer, it is
    /// dropped in-place before the new value is written (no heap allocation).
    ///
    /// Cost: 1 buffer write + 1 atomic swap. Zero allocation.
    #[inline]
    #[expect(unsafe_code)]
    pub fn send(&self, value: T) {
        let back = self.back.get();
        // Safety: only the producer accesses the back buffer. The triple-buffer
        // invariant guarantees back ≠ middle ≠ front. The assignment drops any
        // stale Option<T> that was previously in this slot.
        unsafe {
            *(*self.shared).buffers[back as usize].get() = Some(value);
        }
        // Publish: swap back↔middle, set dirty flag.
        // Release: ensures the buffer write above is visible before the swap
        // publishes the index to the consumer.
        #[expect(unsafe_code)]
        let old_state = unsafe {
            (*self.shared)
                .state
                .swap((back << 1) | DIRTY, Ordering::Release)
        };
        // Old middle becomes new back.
        self.back.set(old_state >> 1);
    }

    /// Returns true if the receiver has been dropped.
    #[must_use]
    pub fn is_abandoned(&self) -> bool {
        #[expect(unsafe_code)]
        unsafe {
            (*self.shared).refcount.load(Ordering::Relaxed) == 1
        }
    }
}

impl<T> SlotReceiver<T> {
    /// Take the latest value, if any.
    ///
    /// Returns `None` if no new value since the last `recv`.
    ///
    /// Cost: 1 atomic load (fast path) + 1 atomic swap + 1 `.take()`.
    /// Zero allocation.
    #[inline]
    #[must_use]
    #[expect(unsafe_code)]
    pub fn recv(&self) -> Option<T> {
        // Fast path: no new data → return without swapping.
        // Relaxed is sufficient: only the consumer clears the dirty flag,
        // so between this load and the swap below, dirty can only stay set
        // or be re-set by the producer. It cannot become 0 spuriously.
        if unsafe { (*self.shared).state.load(Ordering::Relaxed) } & DIRTY == 0 {
            return None;
        }
        let front = self.front.get();
        // Take: swap front↔middle, clear dirty flag.
        // Acquire: ensures we see all buffer writes the producer made before
        // its Release swap.
        let old_state =
            unsafe { (*self.shared).state.swap(front << 1, Ordering::Acquire) };
        // Old middle becomes new front (holds the latest value).
        let new_front = old_state >> 1;
        self.front.set(new_front);
        // Safety: only the consumer accesses the front buffer.
        unsafe { (*(*self.shared).buffers[new_front as usize].get()).take() }
    }

    /// Returns true if the sender has been dropped.
    #[must_use]
    pub fn is_abandoned(&self) -> bool {
        #[expect(unsafe_code)]
        unsafe {
            (*self.shared).refcount.load(Ordering::Relaxed) == 1
        }
    }
}

impl<T> Drop for SlotSender<T> {
    fn drop(&mut self) {
        drop_half(self.shared);
    }
}

impl<T> Drop for SlotReceiver<T> {
    fn drop(&mut self) {
        drop_half(self.shared);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct Tracked(Arc<AtomicUsize>);

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn send_recv_basic() {
        let (tx, rx) = slot::<u64>();
        assert!(rx.recv().is_none());
        tx.send(42);
        assert_eq!(rx.recv(), Some(42));
        assert!(rx.recv().is_none());
    }

    #[test]
    fn latest_value_wins() {
        let (tx, rx) = slot();
        tx.send(1u64);
        tx.send(2);
        tx.send(3);
        // Only the latest value should be returned.
        assert_eq!(rx.recv(), Some(3));
        assert!(rx.recv().is_none());
    }

    #[test]
    fn abandoned_detection() {
        let (tx, rx) = slot::<u64>();
        assert!(!tx.is_abandoned());
        assert!(!rx.is_abandoned());
        drop(rx);
        assert!(tx.is_abandoned());
    }

    #[test]
    fn abandoned_receiver_side() {
        let (tx, rx) = slot::<u64>();
        drop(tx);
        assert!(rx.is_abandoned());
    }

    #[test]
    fn drop_cleans_up_unread_value() {
        // Send a value and drop both halves without reading.
        // This should not leak (run under miri to verify).
        let (tx, rx) = slot::<String>();
        tx.send("hello".to_string());
        drop(tx);
        drop(rx);
    }

    #[test]
    fn overwrite_drops_stale() {
        let drop_count = Arc::new(AtomicUsize::new(0));

        let (tx, rx) = slot();

        // Triple buffer has 3 pre-allocated slots. The first overwrite occurs
        // on the 3rd send, when the back index wraps to a buffer that already
        // held a value.
        tx.send(Tracked(drop_count.clone())); // v1 → buf A
        tx.send(Tracked(drop_count.clone())); // v2 → buf B
        assert_eq!(drop_count.load(Ordering::SeqCst), 0); // 3 slots, no reuse yet

        tx.send(Tracked(drop_count.clone())); // v3 → buf A, drops v1
        assert_eq!(drop_count.load(Ordering::SeqCst), 1);

        let val = rx.recv(); // takes v3 (latest)
        assert!(val.is_some());
        assert_eq!(drop_count.load(Ordering::SeqCst), 1); // v3 owned by val

        drop(val); // drops v3
        assert_eq!(drop_count.load(Ordering::SeqCst), 2);

        drop(tx);
        drop(rx);
        // Shared cleanup drops v2 (unconsumed, still in a buffer).
        assert_eq!(drop_count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn cross_thread() {
        let (tx, rx) = slot();
        let handle = std::thread::spawn(move || {
            for i in 0..1000u64 {
                tx.send(i);
            }
        });

        handle.join().unwrap();
        // After producer is done, we should get the latest value (999 or close).
        let val = rx.recv().unwrap();
        assert!(val >= 990, "expected latest value near 999, got {val}");
    }
}
