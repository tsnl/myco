use futures::Stream;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

pub use futures::StreamExt;

pub type Async<T> = Pin<Box<dyn Future<Output = T> + Send>>;
pub type AsyncStream<T> = Pin<Box<dyn Stream<Item = T> + Send>>;

/// Cooperative cancellation signal for in-flight agent turns.
///
/// Cheap to clone. Cancelling is sticky: once cancelled, all waiters wake and
/// [`Self::is_cancelled`] stays true. Nested work (tools, subagents) should share
/// the same token so Ctrl-C aborts the whole turn.
#[derive(Clone, Default, Debug)]
pub struct CancelToken {
    inner: Arc<CancelInner>,
}

#[derive(Default, Debug)]
struct CancelInner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        // Set first so late waiters observe cancelled without racing notify.
        self.inner.cancelled.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Resolves when [`Self::cancel`] is called (or immediately if already cancelled).
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        // If cancel races between the check and wait, notify_waiters may have
        // already fired — re-check after enabling the permit path via notified().
        let notified = self.inner.notify.notified();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}
