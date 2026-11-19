use std::pin::pin;
use std::sync::{Mutex, MutexGuard, PoisonError};

use tokio::sync::Notify;

pub(super) struct Slots {
    state: Mutex<State>,
    changed: Notify,
}

struct State {
    active: usize,
    limit: usize,
}

pub(super) struct Permit<'a>(&'a Slots);

impl Slots {
    pub(super) const fn new(limit: usize) -> Self {
        Self {
            state: Mutex::new(State { active: 0, limit }),
            changed: Notify::const_new(),
        }
    }
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
    pub(super) fn set_limit(&self, limit: usize) {
        self.state().limit = limit;
        self.changed.notify_waiters();
    }
    pub(super) fn try_acquire(&self) -> Option<Permit<'_>> {
        let mut state = self.state();
        if state.active >= state.limit {
            return None;
        }
        state.active += 1;
        Some(Permit(self))
    }
    /// Registered before the check, so a release between the check and the
    /// wait still reaches this waiter.
    pub(super) async fn acquire(&self) -> Permit<'_> {
        loop {
            let mut changed = pin!(self.changed.notified());
            changed.as_mut().enable();
            if let Some(permit) = self.try_acquire() {
                return permit;
            }
            changed.await;
        }
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.0.state().active -= 1;
        self.0.changed.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use futures::{FutureExt, pin_mut};
    use test_case::test_case;

    use super::Slots;

    #[test_case(false ; "release")]
    #[test_case(true ; "raise_limit")]
    fn every_available_slot_wakes_a_waiter(raise_limit: bool) {
        let slots = Slots::new(2);
        let first = slots.try_acquire().unwrap();
        let second = slots.try_acquire().unwrap();
        let a = slots.acquire();
        let b = slots.acquire();
        pin_mut!(a, b);
        assert!(a.as_mut().now_or_never().is_none());
        assert!(b.as_mut().now_or_never().is_none());
        if raise_limit {
            slots.set_limit(4);
        } else {
            drop(first);
            drop(second);
        }
        let a = a.now_or_never().expect("first slot available");
        let b = b.now_or_never().expect("second slot available");
        assert!(slots.try_acquire().is_none());
        drop((a, b));
    }

    #[test]
    fn dropping_a_notified_waiter_passes_the_slot_to_another_waiter() {
        let slots = Slots::new(1);
        let held = slots.try_acquire().unwrap();
        let mut first = Box::pin(slots.acquire());
        let mut second = Box::pin(slots.acquire());
        assert!(first.as_mut().now_or_never().is_none());
        assert!(second.as_mut().now_or_never().is_none());
        drop(held);
        drop(first);
        let acquired = second
            .now_or_never()
            .expect("waiter inherited the notification");
        assert!(slots.try_acquire().is_none());
        drop(acquired);
        assert!(slots.try_acquire().is_some());
    }
}
