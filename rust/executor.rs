//! Admission, cancellation and cleanup ownership, independent of MCP transport.
use crate::{
    browser_error,
    operations::{Operation, OperationResult},
    page_source::PageSource,
};
use anyhow::{Result, anyhow};
use std::{sync::Arc, time::Duration};
use tokio::sync::{Mutex, Semaphore};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

pub struct RequestExecutor<S> {
    source: Arc<Mutex<S>>,
    slots: Arc<Semaphore>,
    stop: CancellationToken,
    tasks: TaskTracker,
    deadline: Duration,
}

impl<S: PageSource + 'static> RequestExecutor<S> {
    pub fn new(source: S) -> Self {
        Self {
            source: Arc::new(Mutex::new(source)),
            slots: Arc::new(Semaphore::new(8)),
            stop: CancellationToken::new(),
            tasks: TaskTracker::new(),
            deadline: Duration::from_secs(55),
        }
    }

    pub async fn run(
        &self,
        operation: Operation,
        client_cancel: CancellationToken,
    ) -> Result<OperationResult> {
        // Invalid requests must not occupy the browser or change its lifecycle.
        let operation = operation.prepare()?;
        if self.stop.is_cancelled() {
            return Err(anyhow!("Server shutting down"));
        }
        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow!("SERVER_BUSY: the request queue is full"))?;
        let cancel = self.stop.child_token();
        // The transport may drop this future. The tracked task retains ownership
        // of both the queue permit and source until acquisition/cleanup finishes.
        let _cancel_on_drop = cancel.clone().drop_guard();
        let worker_cancel = cancel.clone();
        let source = self.source.clone();
        let work = self.tasks.spawn(async move {
            let _slot = slot;
            let mut source = tokio::select! {
                biased;
                _ = worker_cancel.cancelled() => return Err(anyhow!("Request cancelled")),
                guard = source.lock() => guard,
            };
            let result = operation.execute(&mut *source, &worker_cancel).await;
            if worker_cancel.is_cancelled()
                || result.as_ref().is_err_and(browser_error::requires_reset)
            {
                source.shutdown().await?;
            }
            result
        });
        tokio::select! {
            biased;
            _ = client_cancel.cancelled() => Err(anyhow!("Request cancelled")),
            _ = self.stop.cancelled() => Err(anyhow!("Server shutting down")),
            _ = tokio::time::sleep(self.deadline) => Err(anyhow!("TOOL_TIMEOUT: tool exceeded 55 seconds including queue time")),
            result = work => result.map_err(|_| anyhow!("Tool worker failed"))?,
        }
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.stop.cancel();
        self.tasks.close();
        self.tasks.wait().await;
        self.source.lock().await.shutdown().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::ReviewsArgs;
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ScriptedSource {
        calls: Arc<AtomicUsize>,
        closes: Arc<AtomicUsize>,
        entered: Arc<Semaphore>,
        cleanup_entered: Arc<Semaphore>,
        cleanup_release: Arc<Semaphore>,
        block_first: bool,
    }

    impl PageSource for ScriptedSource {
        async fn fetch_json(&mut self, _: &str, cancel: &CancellationToken) -> Result<Value> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.add_permits(1);
            if call == 0 && self.block_first {
                cancel.cancelled().await;
                return Err(anyhow!("Request cancelled"));
            }
            Ok(json!({"widgetStates": {}}))
        }

        async fn shutdown(&mut self) -> Result<()> {
            self.closes.fetch_add(1, Ordering::SeqCst);
            self.cleanup_entered.add_permits(1);
            self.cleanup_release.acquire().await.unwrap().forget();
            Ok(())
        }
    }

    fn request(limit: usize) -> Operation {
        Operation::Reviews(ReviewsArgs {
            product: "1".into(),
            limit,
        })
    }

    struct Harness {
        executor: Arc<RequestExecutor<ScriptedSource>>,
        calls: Arc<AtomicUsize>,
        closes: Arc<AtomicUsize>,
        entered: Arc<Semaphore>,
        cleanup_entered: Arc<Semaphore>,
        release: Arc<Semaphore>,
    }

    fn setup(deadline: Duration) -> Harness {
        let calls = Arc::new(AtomicUsize::new(0));
        let closes = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(Semaphore::new(0));
        let cleanup_entered = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let mut executor = RequestExecutor::new(ScriptedSource {
            calls: calls.clone(),
            closes: closes.clone(),
            entered: entered.clone(),
            cleanup_entered: cleanup_entered.clone(),
            cleanup_release: release.clone(),
            block_first: true,
        });
        executor.slots = Arc::new(Semaphore::new(2));
        executor.deadline = deadline;
        Harness {
            executor: Arc::new(executor),
            calls,
            closes,
            entered,
            cleanup_entered,
            release,
        }
    }

    async fn signal(semaphore: &Semaphore) {
        tokio::time::timeout(Duration::from_secs(2), semaphore.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
    }

    #[tokio::test]
    async fn invalid_and_cancelled_queued_requests_do_not_touch_active_browser() {
        let Harness {
            executor,
            calls,
            closes,
            entered,
            cleanup_entered,
            release,
        } = setup(Duration::from_secs(55));
        let active_cancel = CancellationToken::new();
        let active = tokio::spawn({
            let executor = executor.clone();
            let cancel = active_cancel.clone();
            async move { executor.run(request(1), cancel).await }
        });
        signal(&entered).await;
        assert!(
            executor
                .run(request(0), CancellationToken::new())
                .await
                .is_err()
        );
        let queued_cancel = CancellationToken::new();
        queued_cancel.cancel();
        assert!(executor.run(request(1), queued_cancel).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(closes.load(Ordering::SeqCst), 0);
        active_cancel.cancel();
        assert!(active.await.unwrap().is_err());
        signal(&cleanup_entered).await;
        release.add_permits(1);
        executor.tasks.close();
        executor.tasks.wait().await;
    }

    #[tokio::test]
    async fn dropped_handler_retains_source_and_admission_until_cleanup_finishes() {
        let Harness {
            executor,
            calls,
            entered,
            cleanup_entered,
            release,
            ..
        } = setup(Duration::from_secs(55));
        let active = tokio::spawn({
            let executor = executor.clone();
            async move { executor.run(request(1), CancellationToken::new()).await }
        });
        signal(&entered).await;
        active.abort();
        assert!(matches!(active.await, Err(error) if error.is_cancelled()));
        signal(&cleanup_entered).await;
        let next = tokio::spawn({
            let executor = executor.clone();
            async move { executor.run(request(1), CancellationToken::new()).await }
        });
        // Wait until the second request has been admitted, without guessing a delay.
        tokio::time::timeout(Duration::from_secs(2), async {
            while executor.slots.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            executor
                .run(request(1), CancellationToken::new())
                .await
                .err()
                .unwrap()
                .to_string()
                .starts_with("SERVER_BUSY:")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        release.add_permits(1);
        assert!(next.await.unwrap().is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        executor.tasks.close();
        executor.tasks.wait().await;
    }
    #[tokio::test]
    async fn deadline_returns_before_cleanup_but_shutdown_waits_for_it() {
        let Harness {
            executor,
            entered,
            cleanup_entered,
            release,
            ..
        } = setup(Duration::from_millis(25));
        let active = tokio::spawn({
            let executor = executor.clone();
            async move { executor.run(request(1), CancellationToken::new()).await }
        });
        signal(&entered).await;
        let error = active.await.unwrap().err().unwrap();
        assert!(error.to_string().starts_with("TOOL_TIMEOUT:"));
        signal(&cleanup_entered).await;
        let closing = tokio::spawn({
            let executor = executor.clone();
            async move { executor.shutdown().await }
        });
        tokio::task::yield_now().await;
        assert!(!closing.is_finished());
        release.add_permits(2); // active cleanup, then final idempotent shutdown
        assert!(closing.await.unwrap().is_ok());
        assert!(
            executor
                .run(request(1), CancellationToken::new())
                .await
                .is_err()
        );
    }
}
