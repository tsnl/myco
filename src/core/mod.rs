mod fs;
pub mod image;

pub(crate) use fs::myco_home_with;
pub use fs::{atomically_write, myco_home, validate_profile};

pub use myco_agent::Async;
pub use myco_model::AsyncStream;

/// Cooperative cancellation signal for in-flight agent turns.
///
/// Cheap to clone. Cancelling is sticky: once cancelled, all waiters wake,
/// [`CancelToken::is_cancelled`] stays true, and a waiter that subscribes
/// *after* the cancel still resolves immediately — a turn must never hang
/// because Ctrl-C raced ahead of the waiter. Nested work (tools, subagents)
/// shares one token so a single cancel aborts the whole turn;
/// [`CancelToken::child_token`] scopes cancellation to one branch of that work.
pub use myco_agent::CancelToken;

pub use futures::StreamExt;

/// Format a UUID as a 32-char lowercase hex string (no hyphens).
pub fn uuid_simple_hex(id: uuid::Uuid) -> String {
    id.as_simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Cancel is sticky: a waiter that arrives after the cancel resolves
    /// immediately instead of hanging.
    #[tokio::test]
    async fn cancel_is_sticky_and_wakes_late_waiter() {
        let token = CancelToken::new();
        token.cancel();
        tokio::time::timeout(Duration::from_millis(100), token.cancelled())
            .await
            .expect("late waiter hung on sticky cancel");
        assert!(token.is_cancelled());
    }

    /// A cancel racing ahead of `cancelled().await` is not lost.
    #[tokio::test]
    async fn cancel_before_wait_is_not_lost() {
        for _ in 0..100 {
            let token = CancelToken::new();
            let waiter = token.clone();
            let handle = tokio::spawn(async move {
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

    /// A child token cancels with its parent, so scoping a branch of the work
    /// never costs turn-wide cancellation.
    #[tokio::test]
    async fn child_token_cancels_with_parent() {
        let parent = CancelToken::new();
        let child = parent.child_token();
        parent.cancel();
        tokio::time::timeout(Duration::from_millis(100), child.cancelled())
            .await
            .expect("child did not wake with parent");
        assert!(child.is_cancelled());
    }
}
