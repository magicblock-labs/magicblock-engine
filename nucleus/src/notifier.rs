//! One-shot async event notification.

use std::sync::atomic::{AtomicBool, Ordering::*};

use tokio::sync::Notify;

/// A one-shot latch that wakes every waiter once notified.
///
/// Waiters that arrive after notification return immediately. The event cannot
/// be reset.
#[derive(Default)]
pub struct EventNotifier {
    /// Set after notification so future waiters can return immediately.
    done: AtomicBool,
    /// Outcome published before `done` makes the event observable.
    success: AtomicBool,
    /// Wakes tasks that registered before notification.
    notify: Notify,
}

impl EventNotifier {
    /// Completes the event with `success` and wakes all current waiters.
    pub fn notify(&self, success: bool) {
        self.success.store(success, Release);
        self.done.store(true, Release);
        self.notify.notify_waiters();
    }

    /// Waits until completion and returns the published outcome.
    pub async fn notified(&self) -> bool {
        loop {
            let notified = self.notify.notified();
            // The waiter is created before the second load, so a concurrent
            // notify cannot land between observing `false` and registering.
            if self.done.load(Acquire) {
                return self.success.load(Acquire);
            }

            notified.await;
        }
    }
}
