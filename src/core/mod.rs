use futures::Stream;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::watch;

pub use futures::StreamExt;

pub mod image;

pub type Async<T> = Pin<Box<dyn Future<Output = T> + Send>>;
pub type AsyncStream<T> = Pin<Box<dyn Stream<Item = T> + Send>>;

/// Cooperative cancellation signal for in-flight agent turns.
///
/// Cheap to clone. Cancelling is sticky: once cancelled, all waiters wake and
/// [`Self::is_cancelled`] stays true. Nested work (tools, subagents) should share
/// the same token so Ctrl-C aborts the whole turn.
///
/// Implemented with [`watch`] so a cancel that races ahead of a waiter still
/// wakes it (unlike `Notify::notify_waiters`, which is lost if no waiter is
/// registered yet — a flake under suite load).
#[derive(Clone, Debug)]
pub struct CancelToken {
    tx: Arc<watch::Sender<bool>>,
}

impl Default for CancelToken {
    fn default() -> Self {
        let (tx, _rx) = watch::channel(false);
        Self { tx: Arc::new(tx) }
    }
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        // `send_replace` always updates; subscribers see the new value even if
        // they subscribe later (sticky cancel).
        self.tx.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    /// Resolves when [`Self::cancel`] is called (or immediately if already cancelled).
    pub async fn cancelled(&self) {
        let mut rx = self.tx.subscribe();
        loop {
            if *rx.borrow_and_update() {
                return;
            }
            // Sender is held by Arc clones of this token for the turn lifetime;
            // if it ever drops, treat as cancelled so waiters don't hang.
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn cancel_is_sticky_and_wakes_late_waiter() {
        let token = CancelToken::new();
        token.cancel();
        // Already cancelled: must not hang.
        tokio::time::timeout(Duration::from_millis(100), token.cancelled())
            .await
            .expect("late waiter hung on sticky cancel");
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn cancel_wakes_concurrent_waiters() {
        let token = CancelToken::new();
        let t1 = token.clone();
        let t2 = token.clone();
        let w1 = tokio::spawn(async move { t1.cancelled().await });
        let w2 = tokio::spawn(async move { t2.cancelled().await });
        tokio::task::yield_now().await;
        token.cancel();
        tokio::time::timeout(Duration::from_secs(1), async {
            w1.await.unwrap();
            w2.await.unwrap();
        })
        .await
        .expect("waiters did not wake");
    }

    #[tokio::test]
    async fn cancel_before_wait_is_not_lost() {
        // Regression: Notify::notify_waiters was a no-op with zero waiters, so a
        // cancel that raced ahead of cancelled().await never woke the waiter.
        for _ in 0..100 {
            let token = CancelToken::new();
            let waiter = token.clone();
            let handle = tokio::spawn(async move {
                // Tiny delay so cancel often wins the race.
                tokio::task::yield_now().await;
                waiter.cancelled().await;
            });
            token.cancel();
            tokio::time::timeout(Duration::from_millis(200), handle)
                .await
                .expect("cancel-before-wait hung")
                .unwrap();
        }
    }
}
