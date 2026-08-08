//! The side-feed state pattern, spelled once.
//!
//! Commands serialize through the cell; streams flow outside it through
//! [`Shared`]. Every kind with a side-feed — a pty pumping bytes, a model
//! turn streaming tokens, an event feed filling an inbox — needs the same
//! three-part discipline: mutate under the lock, drop the lock, then bump
//! the watermark so watchers re-read. Hand-rolled, that discipline is three
//! chances to get it wrong, and the two that matter fail silently: a
//! mutation with no bump leaves watchers asleep on stale state, and a lock
//! held across an await stalls every reader behind the slowest writer.
//!
//! This type removes both. [`Shared::with`] takes a *synchronous* closure,
//! so the guard cannot outlive it and no await can happen inside it; the
//! bump is on the far side of the same call, so a mutation that forgets to
//! publish is not a thing anyone can write. [`Shared::read`] is the
//! deliberate other half: reads observe, so they do not bump.
//!
//! Thin on purpose. It is a lock and a watermark, not a framework: state
//! that wants a mutation with no bump does not belong here.

use std::sync::{Arc, Mutex};

use myco_runtime::Signals;

/// Kind-internal state shared between a cell's verbs and its side-feed
/// tasks. Clone it into every task that writes; the clone shares the state.
pub struct Shared<S> {
    state: Arc<Mutex<S>>,
    signals: Signals,
}

// Hand-written: the state is behind an `Arc`, so cloning a `Shared` never
// requires `S: Clone` (a derive would demand it).
impl<S> Clone for Shared<S> {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            signals: self.signals.clone(),
        }
    }
}

impl<S> Shared<S> {
    /// Wrap state a kind's side-feeds will write. `signals` is the cell's,
    /// handed to `Kind::create`.
    pub fn new(state: S, signals: Signals) -> Self {
        Self {
            state: Arc::new(Mutex::new(state)),
            signals,
        }
    }

    /// Mutate, then publish. The lock is released before the bump, so a
    /// watcher woken by it never contends with the writer that woke it. A
    /// poisoned lock is not fatal here: kind state is rebuildable by
    /// re-reading, and refusing to serve a whole terminal because one
    /// handler panicked is the worse failure.
    pub fn with<R>(&self, f: impl FnOnce(&mut S) -> R) -> R {
        let out = {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut guard)
        };
        self.signals.bump();
        out
    }

    /// Observe without publishing. Reads change nothing, so they owe
    /// watchers nothing.
    pub fn read<R>(&self, f: impl FnOnce(&S) -> R) -> R {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        f(&guard)
    }

    /// The cell's signals, for the moments that are history rather than
    /// state — an `exited` event alongside the mutation that recorded it.
    pub fn signals(&self) -> &Signals {
        &self.signals
    }
}
