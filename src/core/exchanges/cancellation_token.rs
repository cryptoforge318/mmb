use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Result};
use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::core::exchanges::common::OPERATION_CANCELED_MSG;
use crate::core::nothing_to_do;

#[derive(Default)]
struct CancellationState {
    signal: Notify,
    handlers: Mutex<Vec<Box<dyn Fn() + Send>>>,
    is_cancellation_requested: AtomicBool,
}

/// Lightweight object for signalling about cancellation of operation
/// Note: expected passing through methods by owning value with cloning if we need checking cancellation in many places
#[derive(Default, Clone)]
pub struct CancellationToken {
    state: Arc<CancellationState>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        let state = &self.state;
        state
            .is_cancellation_requested
            .store(true, Ordering::SeqCst);

        state.handlers.lock().iter().for_each(|handler| handler());
        state.signal.notify_waiters();
    }

    /// Returns true if cancellation requested, otherwise false
    pub fn is_cancellation_requested(&self) -> bool {
        self.state.is_cancellation_requested.load(Ordering::SeqCst)
    }

    /// Returns Result::Err() if cancellation requested, otherwise Ok(())
    pub fn error_if_cancellation_requested(&self) -> Result<()> {
        match self.is_cancellation_requested() {
            true => bail!(OPERATION_CANCELED_MSG),
            false => Ok(()),
        }
    }

    pub async fn when_cancelled(&self) {
        if self.is_cancellation_requested() {
            return;
        }

        self.state.clone().signal.notified().await;
    }

    pub fn create_linked_token(&self) -> Self {
        let new_token = CancellationToken::new();

        {
            let weak_cancellation = Arc::downgrade(&new_token.state);
            self.register_handler(Box::new(move || match weak_cancellation.upgrade() {
                None => nothing_to_do(),
                Some(state) => CancellationToken { state }.cancel(),
            }))
        }

        if self.is_cancellation_requested() {
            new_token.cancel();
        }

        new_token
    }

    fn register_handler(&self, handler: Box<dyn Fn() + Send>) {
        self.state.handlers.lock().push(handler);
    }
}

#[cfg(test)]
mod tests {
    use crate::core::{exchanges::cancellation_token::CancellationToken, utils::custom_spawn};
    use parking_lot::Mutex;
    use std::sync::Arc;
    use tokio::time::Duration;

    #[test]
    fn just_cancel() {
        let token = CancellationToken::new();
        assert_eq!(token.is_cancellation_requested(), false);

        token.cancel();
        assert_eq!(token.is_cancellation_requested(), true);
    }

    #[tokio::test]
    async fn single_await() {
        let token = CancellationToken::new();

        let signal = Arc::new(Mutex::new(false));

        spawn_working_future(signal.clone(), token.clone());

        // make sure that we don't complete test too fast accidentally before method when_cancelled() completed
        tokio::time::sleep(Duration::from_millis(2)).await;

        assert_eq!(*signal.lock(), false);
        assert_eq!(token.is_cancellation_requested(), false);

        token.cancel();

        // we need a little wait while spawned `working_future` react for cancellation
        tokio::task::yield_now().await;

        assert_eq!(*signal.lock(), true);
        assert_eq!(token.is_cancellation_requested(), true);
    }

    #[tokio::test]
    async fn many_awaits() {
        let token = CancellationToken::new();

        let signal1 = Arc::new(Mutex::new(false));
        let signal2 = Arc::new(Mutex::new(false));

        spawn_working_future(signal1.clone(), token.clone());
        spawn_working_future(signal2.clone(), token.clone());

        // make sure that we don't complete test too fast accidentally before method when_cancelled() completed
        tokio::time::sleep(Duration::from_millis(2)).await;

        assert_eq!(*signal1.lock(), false);
        assert_eq!(*signal2.lock(), false);
        assert_eq!(token.is_cancellation_requested(), false);

        token.cancel();

        // we need a little wait while spawned `working_future` react for cancellation
        tokio::task::yield_now().await;

        assert_eq!(*signal1.lock(), true);
        assert_eq!(*signal2.lock(), true);
        assert_eq!(token.is_cancellation_requested(), true);
    }

    #[test]
    fn double_cancel_call() {
        let token = CancellationToken::new();
        assert_eq!(token.is_cancellation_requested(), false);

        token.cancel();
        assert_eq!(token.is_cancellation_requested(), true);

        token.cancel();
        assert_eq!(token.is_cancellation_requested(), true);
    }

    fn spawn_working_future(signal: Arc<Mutex<bool>>, token: CancellationToken) {
        let action = async move {
            token.when_cancelled().await;
            *signal.lock() = true;

            Ok(())
        };
        custom_spawn(
            "handle_inner for schedule_handler()",
            Box::pin(action),
            true,
        );
    }

    #[tokio::test]
    async fn cancel_source_token_when_linked_source_token_is_not_cancelled() {
        let source_token = CancellationToken::new();
        assert_eq!(source_token.is_cancellation_requested(), false);

        let new_token = source_token.create_linked_token();
        assert_eq!(source_token.is_cancellation_requested(), false);
        assert_eq!(new_token.is_cancellation_requested(), false);

        source_token.cancel();
        assert_eq!(source_token.is_cancellation_requested(), true);
        assert_eq!(new_token.is_cancellation_requested(), true);
    }

    #[tokio::test]
    async fn create_linked_token_when_source_token_is_cancelled() {
        let source_token = CancellationToken::new();
        source_token.cancel();
        assert_eq!(source_token.is_cancellation_requested(), true);

        let new_token = source_token.create_linked_token();
        assert_eq!(source_token.is_cancellation_requested(), true);
        assert_eq!(new_token.is_cancellation_requested(), true);
    }

    #[tokio::test]
    async fn cancel_new_linked_token_when_source_token_is_not_cancelled() {
        let source_token = CancellationToken::new();
        assert_eq!(source_token.is_cancellation_requested(), false);

        let new_token = source_token.create_linked_token();
        assert_eq!(source_token.is_cancellation_requested(), false);
        assert_eq!(new_token.is_cancellation_requested(), false);

        new_token.cancel();
        assert_eq!(source_token.is_cancellation_requested(), false);
        assert_eq!(new_token.is_cancellation_requested(), true);
    }

    #[tokio::test]
    async fn cancel_when_2_new_linked_tokens_to_single_source() {
        // source -> token1
        //      \--> token2

        let source_token = CancellationToken::new();
        assert_eq!(source_token.is_cancellation_requested(), false);

        let new_token1 = source_token.create_linked_token();
        assert_eq!(source_token.is_cancellation_requested(), false);
        assert_eq!(new_token1.is_cancellation_requested(), false);

        let new_token2 = source_token.create_linked_token();
        assert_eq!(source_token.is_cancellation_requested(), false);
        assert_eq!(new_token1.is_cancellation_requested(), false);
        assert_eq!(new_token2.is_cancellation_requested(), false);

        source_token.cancel();
        assert_eq!(source_token.is_cancellation_requested(), true);
        assert_eq!(new_token1.is_cancellation_requested(), true);
        assert_eq!(new_token2.is_cancellation_requested(), true);
    }

    #[tokio::test]
    async fn cancel_source_when_2_sequentially_new_linked_tokens() {
        // source -> token1 -> token2
        let source_token = CancellationToken::new();
        assert_eq!(source_token.is_cancellation_requested(), false);

        let new_token1 = source_token.create_linked_token();
        assert_eq!(source_token.is_cancellation_requested(), false);
        assert_eq!(new_token1.is_cancellation_requested(), false);

        let new_token2 = new_token1.create_linked_token();
        assert_eq!(source_token.is_cancellation_requested(), false);
        assert_eq!(new_token1.is_cancellation_requested(), false);
        assert_eq!(new_token2.is_cancellation_requested(), false);

        source_token.cancel();
        assert_eq!(source_token.is_cancellation_requested(), true);
        assert_eq!(new_token1.is_cancellation_requested(), true);
        assert_eq!(new_token2.is_cancellation_requested(), true);
    }

    #[tokio::test]
    async fn cancel_token1_when_2_sequentially_new_linked_tokens() {
        // source -> token1 -> token2
        let source_token = CancellationToken::new();
        assert_eq!(source_token.is_cancellation_requested(), false);

        let new_token1 = source_token.create_linked_token();
        assert_eq!(source_token.is_cancellation_requested(), false);
        assert_eq!(new_token1.is_cancellation_requested(), false);

        let new_token2 = new_token1.create_linked_token();
        assert_eq!(source_token.is_cancellation_requested(), false);
        assert_eq!(new_token1.is_cancellation_requested(), false);
        assert_eq!(new_token2.is_cancellation_requested(), false);

        new_token1.cancel();
        assert_eq!(source_token.is_cancellation_requested(), false);
        assert_eq!(new_token1.is_cancellation_requested(), true);
        assert_eq!(new_token2.is_cancellation_requested(), true);
    }
}
