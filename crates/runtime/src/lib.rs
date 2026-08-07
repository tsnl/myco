//! L0: the actor runtime. A [`Cell`] owns one piece of state and applies
//! commands to it strictly in arrival order — that serialization is the whole
//! consistency story, so nothing above this layer ever needs a lock to stay
//! coherent. Knows nothing of kinds, verbs, or HTTP.
//!
//! Alongside the mailbox, a cell carries the two broadcast channels every
//! observer relies on:
//!
//! - a **watermark** (`u64`, monotone) bumped on every observable change;
//!   watchers wait for it to pass a value they have seen, then re-read
//!   whatever representation they care about, so any number of them can
//!   watch without coordination;
//! - an **event** stream for significant moments ("exited", "crashed") —
//!   the record of what *happened*, distinct from reads of what *is*.
//!
//! Commands apply asynchronously (a handler may await), but one at a time.
//! High-throughput sources (a pty pumping bytes) should not queue a command
//! per chunk: the sanctioned side-feed pattern is to mutate kind-internal
//! shared state from an I/O task and publish through [`Signals`] — commands
//! serialize, streams flow.
//!
//! Supervision is deliberately minimal: a panicking handler kills the cell's
//! task, a monitor marks the cell crashed, emits a `crashed` event, and
//! bumps the watermark so blocked watchers wake and discover the corpse.
//! Later calls fail with [`CellGone`] rather than hanging.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::future::BoxFuture;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

/// Monotone change counter. `0` is "never changed"; watchers hold the last
/// value they acted on and wait for anything greater.
pub type Watermark = u64;

/// A significant moment in a cell's life, broadcast to whoever listens.
/// Events are history, not state — reads answer "what is", events "what
/// happened".
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Event {
    pub name: String,
    #[serde(default)]
    pub data: serde_json::Value,
}

/// The handle a command handler gets alongside the state: bump the
/// watermark, emit events, or clone [`Signals`] for a side-feeding task.
pub struct Ctx {
    mark: watch::Sender<Watermark>,
    events: broadcast::Sender<Event>,
}

impl Ctx {
    /// Declare "something observable changed": watchers wake and re-read.
    pub fn bump(&self) {
        self.mark.send_modify(|w| *w += 1);
    }

    pub fn emit(&self, name: impl Into<String>, data: serde_json::Value) {
        let _ = self.events.send(Event {
            name: name.into(),
            data,
        });
    }

    /// A cloneable publisher for tasks that feed state outside the mailbox.
    pub fn signals(&self) -> Signals {
        Signals {
            mark: self.mark.clone(),
            events: self.events.clone(),
        }
    }
}

/// [`Ctx`]'s publishing half, detached from the mailbox. Held by side-feed
/// tasks (a pty reader) that change state a cell merely fronts.
#[derive(Clone)]
pub struct Signals {
    mark: watch::Sender<Watermark>,
    events: broadcast::Sender<Event>,
}

impl Signals {
    pub fn bump(&self) {
        self.mark.send_modify(|w| *w += 1);
    }

    pub fn emit(&self, name: impl Into<String>, data: serde_json::Value) {
        let _ = self.events.send(Event {
            name: name.into(),
            data,
        });
    }
}

/// The cell is dead: its task ended (closed or panicked) before — or while —
/// the command could run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellGone;

impl std::fmt::Display for CellGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the instance's task is gone")
    }
}

impl std::error::Error for CellGone {}

type Apply<S> = Box<dyn for<'a> FnOnce(&'a mut S, &'a mut Ctx) -> BoxFuture<'a, ()> + Send>;

/// One actor: state `S`, a mailbox applying commands serially, a watermark,
/// an event stream, and a crashed flag. Cheap to clone; the state lives in
/// the task, never in the handle.
pub struct Cell<S> {
    tx: mpsc::Sender<Apply<S>>,
    mark: watch::Receiver<Watermark>,
    events: broadcast::Sender<Event>,
    signals: Signals,
    crashed: Arc<AtomicBool>,
}

impl<S> Clone for Cell<S> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            mark: self.mark.clone(),
            events: self.events.clone(),
            signals: self.signals.clone(),
            crashed: Arc::clone(&self.crashed),
        }
    }
}

impl<S: Send + 'static> Cell<S> {
    pub fn spawn(state: S) -> Self {
        Self::try_spawn_with(|_| Ok::<S, std::convert::Infallible>(state))
            .unwrap_or_else(|never| match never {})
    }

    /// Spawn with a fallible builder that receives the cell's [`Signals`]
    /// before the task exists — how a kind wires side-feed tasks (a pty
    /// reader) to the cell they publish through, without a second
    /// post-construction step.
    pub fn try_spawn_with<E>(build: impl FnOnce(Signals) -> Result<S, E>) -> Result<Self, E> {
        let (tx, mut rx) = mpsc::channel::<Apply<S>>(64);
        let (mark_tx, mark_rx) = watch::channel(0u64);
        let (event_tx, _) = broadcast::channel(256);
        let signals = Signals {
            mark: mark_tx.clone(),
            events: event_tx.clone(),
        };
        let crashed = Arc::new(AtomicBool::new(false));

        let state = build(signals.clone())?;
        let mut cx = Ctx {
            mark: mark_tx,
            events: event_tx.clone(),
        };
        let task = tokio::spawn(async move {
            let mut state = state;
            while let Some(apply) = rx.recv().await {
                apply(&mut state, &mut cx).await;
            }
        });

        // The monitor is the supervision story: on panic, mark the corpse,
        // tell listeners, and bump so watchers blocked on the watermark wake
        // up to discover it (the signals clone keeps the channel itself
        // alive, so a bump — not a channel close — is what reaches them).
        let flag = Arc::clone(&crashed);
        let post = signals.clone();
        tokio::spawn(async move {
            if task.await.is_err() {
                flag.store(true, Ordering::SeqCst);
                post.emit("crashed", serde_json::Value::Null);
                post.bump();
            }
        });

        Ok(Self {
            tx,
            mark: mark_rx,
            events: event_tx,
            signals,
            crashed,
        })
    }

    /// Apply a command and wait for its result. Commands run in arrival
    /// order, one at a time; the closure may await.
    pub async fn call<R, F>(&self, f: F) -> Result<R, CellGone>
    where
        R: Send + 'static,
        F: for<'a> FnOnce(&'a mut S, &'a mut Ctx) -> BoxFuture<'a, R> + Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        let apply: Apply<S> = Box::new(move |state, cx| {
            Box::pin(async move {
                let value = f(state, cx).await;
                let _ = reply_tx.send(value);
            })
        });
        self.tx.send(apply).await.map_err(|_| CellGone)?;
        reply_rx.await.map_err(|_| CellGone)
    }

    /// The current watermark.
    pub fn watermark(&self) -> Watermark {
        *self.mark.borrow()
    }

    /// Wait until the watermark exceeds `since`, returning the new value.
    /// Returns `Err(CellGone)` only if the channel itself is gone; a crash
    /// resolves normally (the monitor bumps), and the caller discovers the
    /// corpse on its next [`Cell::call`].
    pub async fn changed(&self, since: Watermark) -> Result<Watermark, CellGone> {
        let mut mark = self.mark.clone();
        let seen = mark.wait_for(|w| *w > since).await.map_err(|_| CellGone)?;
        Ok(*seen)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// The publishing half, for side-feed tasks spawned at instance birth.
    pub fn signals(&self) -> Signals {
        self.signals.clone()
    }

    pub fn is_crashed(&self) -> bool {
        self.crashed.load(Ordering::SeqCst)
    }
}

// There is deliberately no `close()`: a cell ends when its last handle
// drops (queued commands drain, the task ends, the state drops — which is
// where kinds kill their children, e.g. `kill_on_drop` pty processes). The
// registry above owns the canonical handle, so "remove" is "forget".

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed<'a, R: Send + 'a>(
        fut: impl std::future::Future<Output = R> + Send + 'a,
    ) -> BoxFuture<'a, R> {
        Box::pin(fut)
    }

    #[tokio::test]
    async fn commands_apply_in_order_one_at_a_time() {
        let cell = Cell::spawn(Vec::<u32>::new());
        // Interleave-prone shape: each command reads the length, awaits, then
        // pushes. Serialized application makes the result exactly 0..N.
        let mut handles = Vec::new();
        for _ in 0..32u32 {
            let c = cell.clone();
            handles.push(tokio::spawn(async move {
                c.call(|v, _cx| {
                    boxed(async move {
                        let n = v.len() as u32;
                        tokio::task::yield_now().await;
                        v.push(n);
                    })
                })
                .await
                .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let got = cell
            .call(|v, _| boxed(async move { v.clone() }))
            .await
            .unwrap();
        assert_eq!(got, (0..32).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn watchers_wake_on_bump_and_see_a_monotone_watermark() {
        let cell = Cell::spawn(0u32);
        assert_eq!(cell.watermark(), 0);

        let watcher = {
            let c = cell.clone();
            tokio::spawn(async move { c.changed(0).await.unwrap() })
        };
        cell.call(|n, cx| {
            boxed(async move {
                *n += 1;
                cx.bump();
            })
        })
        .await
        .unwrap();
        assert_eq!(watcher.await.unwrap(), 1);
        // A watcher that has seen 1 does not wake for it again.
        assert_eq!(cell.watermark(), 1);
    }

    #[tokio::test]
    async fn events_reach_subscribers() {
        let cell = Cell::spawn(());
        let mut rx = cell.subscribe();
        cell.call(|(), cx| {
            boxed(async move {
                cx.emit("ping", serde_json::json!({"n": 1}));
            })
        })
        .await
        .unwrap();
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.name, "ping");
        assert_eq!(ev.data["n"], 1);
    }

    #[tokio::test]
    async fn a_panicking_command_crashes_the_cell_not_the_caller() {
        let cell = Cell::spawn(0u32);
        let mut events = cell.subscribe();
        let watcher = {
            let c = cell.clone();
            tokio::spawn(async move { c.changed(0).await })
        };

        let dead = cell
            .call(|_, _| boxed(async move { panic!("kind bug") }))
            .await;
        assert_eq!(dead, Err(CellGone), "the panicked command reports gone");

        // The monitor marks, emits, and bumps — so both the event listener
        // and the blocked watcher learn without polling.
        assert_eq!(events.recv().await.unwrap().name, "crashed");
        assert!(watcher.await.unwrap().is_ok());
        assert!(cell.is_crashed());
        let later = cell.call(|n, _| boxed(async move { *n })).await;
        assert_eq!(later, Err(CellGone), "a corpse refuses new commands");
    }

    #[tokio::test]
    async fn side_feeds_publish_through_signals() {
        let cell = Cell::spawn(0u32);
        let signals = cell.signals();
        let watcher = {
            let c = cell.clone();
            tokio::spawn(async move { c.changed(0).await.unwrap() })
        };
        // An "I/O task" bumps without going through the mailbox.
        tokio::spawn(async move {
            signals.bump();
        });
        assert!(watcher.await.unwrap() >= 1);
    }
}
