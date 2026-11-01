use std::sync::{Mutex, MutexGuard, PoisonError};

use event_listener::Event;

pub(super) struct Slots {
    state: Mutex<State>,
    changed: Event,
}

struct State {
    active: usize,
    limit: usize,
}

pub(super) struct Permit<'a>(&'a Slots);

impl Slots {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            state: Mutex::new(State { active: 0, limit }),
            changed: Event::new(),
        }
    }
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
    pub(super) fn set_limit(&self, limit: usize) {
        self.state().limit = limit;
        self.changed.notify(usize::MAX);
    }
    pub(super) fn try_acquire(&self) -> Option<Permit<'_>> {
        let mut state = self.state();
        if state.active >= state.limit {
            return None;
        }
        state.active += 1;
        Some(Permit(self))
    }
    pub(super) async fn acquire(&self) -> Permit<'_> {
        loop {
            let changed = self.changed.listen();
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
        self.0.changed.notify(1);
    }
}
