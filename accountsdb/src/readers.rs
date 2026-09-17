//! Reader admission for relocation of the live mapped store.
//!
//! Each thread writes only its own slot. Linux pairs compiler fences on entry/exit
//! with a process-wide membarrier during maintenance; other platforms use full
//! fences on both sides. The gate/slot handshake prevents a reader and the
//! compactor from both overlooking the other's announcement.
//!
//! Admission protects mapped images and cached index offsets from relocation,
//! not concurrent account writes. Callers must enter before opening an index
//! transaction and drop that transaction and all borrowed views before leaving.
//! Maintenance must independently exclude writers while holding its pause.

use std::{
    io,
    marker::PhantomData,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering::*, fence},
};

use parking_lot::{Condvar, Mutex, MutexGuard};
#[cfg(target_os = "linux")]
use rustix::thread::{MembarrierCommand, membarrier};
use thread_local::ThreadLocal;

/// Coordinates reader scopes with exclusive relocation of one database.
pub(crate) struct Readers {
    /// Stable nesting counters, written only by their owning threads and scanned
    /// by maintenance. First registration is serialized with that scan.
    slots: ThreadLocal<Slot>,
    /// Admission gate: true while maintenance drains readers or relocates data.
    closed: AtomicBool,
    /// Serializes pauses and first registration; retained throughout relocation,
    /// including while the compactor releases `idle` to wait for readers.
    maintenance: Mutex<()>,
    /// Couples the active-slot scan with exit notification to prevent lost
    /// wakeups. Separate from `maintenance` so waiting never admits a new pause.
    idle: Mutex<()>,
    /// Wakes the sole compactor when an outer reader exits or withdraws.
    drained: Condvar,
}

/// Isolates one thread's writes, including paired 64-byte cache-line prefetches
/// on x86-64. Padding is per thread, not per account.
#[derive(Default)]
#[repr(align(128))]
struct Slot {
    /// Number of live guards on the owning thread; zero means inactive.
    /// Single-writer ownership permits loads/stores rather than atomic RMWs.
    depth: AtomicUsize,
}

/// Keeps this thread admitted until its outermost synchronous read ends.
pub(crate) struct ReadGuard<'a> {
    /// This thread's stable slot, shared by its nested reader scopes.
    slot: &'a Slot,
    /// Admission state to notify when the final nested scope exits.
    readers: &'a Readers,
    /// Makes the guard neither Send nor Sync, preserving single-threaded nesting
    /// and preventing concurrent non-RMW updates to its slot.
    _local: PhantomData<*mut ()>,
}

/// Reopens admission on success, error, or unwind, before releasing the mutex.
pub(crate) struct Pause<'a> {
    /// Gate reopened by Drop, including on barrier failure or unwinding.
    readers: &'a Readers,
    /// Exclusive maintenance ownership, released only after Drop reopens admission.
    _maintenance: MutexGuard<'a, ()>,
}

impl Readers {
    /// Opens admission and registers Linux's process-wide expedited barrier.
    /// Registration failure aborts construction; the fence protocol never changes
    /// after readers begin using the database.
    pub(crate) fn new() -> io::Result<Self> {
        // Registration is idempotent across databases in the same process.
        #[cfg(target_os = "linux")]
        membarrier(MembarrierCommand::RegisterPrivateExpedited)?;
        Ok(Self {
            slots: ThreadLocal::new(),
            closed: AtomicBool::new(false),
            maintenance: Mutex::new(()),
            idle: Mutex::new(()),
            drained: Condvar::new(),
        })
    }

    /// Enters a synchronous read scope, waiting if maintenance has closed admission.
    /// Nested scopes inherit admission so an existing reader can finish while
    /// maintenance waits. Registered, uncontended readers take no mutex.
    #[inline]
    pub(crate) fn enter(&self) -> ReadGuard<'_> {
        let slot = self.slots.get().unwrap_or_else(|| self.register());
        loop {
            let depth = slot.depth.load(Relaxed);
            slot.depth.store(depth + 1, Relaxed);
            let guard = ReadGuard {
                slot,
                readers: self,
                _local: PhantomData,
            };
            // Nested scopes inherit outer admission even while maintenance
            // is waiting for that outer scope to finish.
            if depth != 0 {
                return guard;
            }

            light_fence();
            if !self.closed.load(Acquire) {
                return guard;
            }
            drop(guard);
            // Only readers that meet maintenance touch the mutex. Never wait
            // while marked active: the compactor is waiting for these slots.
            drop(self.maintenance.lock());
        }
    }

    /// Closes admission and drains existing scopes before granting relocation.
    /// Returns WouldBlock if this thread holds a reader, avoiding self-deadlock.
    /// Barrier failure reopens admission through the temporary pause guard.
    pub(crate) fn pause(&self) -> io::Result<Pause<'_>> {
        if self.slots.get().is_some_and(|slot| slot.depth.load(Relaxed) != 0) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "cannot compact accountsdb from an active reader scope",
            ));
        }
        let maintenance = self.maintenance.lock();
        let pause = Pause {
            readers: self,
            _maintenance: maintenance,
        };
        self.closed.store(true, Relaxed);

        #[cfg(target_os = "linux")]
        membarrier(MembarrierCommand::PrivateExpedited)?;
        #[cfg(not(target_os = "linux"))]
        fence(SeqCst);

        // Paired entry barriers guarantee that a racing reader either appears
        // active here or observes the closed gate before touching the index.
        // Registration stays excluded for the full pause, so this set is fixed.
        // The separate wait mutex lets exiting readers notify without releasing
        // exclusive maintenance ownership or allowing a second compactor in.
        let mut idle = self.idle.lock();
        while self.slots.iter().any(|slot| slot.depth.load(Acquire) != 0) {
            self.drained.wait(&mut idle);
        }
        // Keep subsequent relocation after the complete slot scan, including
        // on weakly ordered platforms when a slot was already inactive.
        fence(SeqCst);
        Ok(pause)
    }

    /// Registers a thread once, synchronized with the closed gate and slot scan.
    /// Waiting on maintenance prevents adding a slot to an in-progress pause.
    #[cold]
    fn register(&self) -> &Slot {
        let _registration = self.maintenance.lock();
        self.slots.get_or_default()
    }

    /// Wakes the sole compactor after an outer reader exits or withdraws.
    /// Holding `idle` pairs notification with its scan-and-wait transition.
    #[cold]
    fn notify(&self) {
        let _idle = self.idle.lock();
        self.drained.notify_one();
    }
}

impl Drop for ReadGuard<'_> {
    /// Releases one nesting level; the last exit publishes completed accesses
    /// and notifies a waiting compactor after the gate/slot handshake.
    #[inline]
    fn drop(&mut self) {
        // Publish the last mmap access before allowing reclamation. Only this
        // thread changes its nesting depth, so no atomic RMW is needed.
        let depth = self.slot.depth.load(Relaxed);
        self.slot.depth.store(depth - 1, Release);
        if depth != 1 {
            return;
        }
        // Pair with maintenance's gate publication and heavy fence: either
        // its scan sees our exit or we see the gate and notify. The mutex
        // then closes the race between the scan and entering the wait.
        light_fence();
        if self.readers.closed.load(Acquire) {
            self.readers.notify();
        }
    }
}

impl Drop for Pause<'_> {
    /// Publishes completed relocation and reopens admission before unlocking
    /// maintenance, so blocked readers can retry against the new layout.
    fn drop(&mut self) {
        self.readers.closed.store(false, Release);
    }
}

/// Orders the slot announcement before inspecting the admission gate.
/// Linux pairs a compiler fence with maintenance's process-wide membarrier;
/// macOS and other platforms need a full hardware fence on the reader side too.
#[inline]
fn light_fence() {
    #[cfg(target_os = "linux")]
    std::sync::atomic::compiler_fence(SeqCst);
    #[cfg(not(target_os = "linux"))]
    fence(SeqCst);
}
