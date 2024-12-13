use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use std::future::Future;
use tokio::time;
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum DriftError {
    #[error("job failed with inner error: {0}")]
    JobError(#[source] anyhow::Error),
}

pub fn schedule<F, Fut>(interval: Duration, func: F) -> CancellationToken
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), DriftError>> + Send + 'static,
{
    let drifter = FuncDrifter::new(func);

    schedule_drifter(interval, drifter)
}

pub fn schedule_drifter<FDrifter>(interval: Duration, drifter: FDrifter) -> CancellationToken
where
    FDrifter: Drifter + Send + 'static,
    FDrifter: Clone,
{
    let cancellation_token = CancellationToken::new();

    tokio::spawn({
        let cancellation_token = cancellation_token.clone();
        let drifter = drifter.clone();

        async move {
            let mut wait = interval;
            let start = std::time::Instant::now();

            tracing::debug!("running job");
            let child_token = cancellation_token.child_token();
            if let Err(e) = drifter.execute(child_token).await {
                tracing::error!("drift job failed with error: {}, stopping routine", e);
                cancellation_token.cancel();
            }

            let elapsed = start.elapsed();
            wait = interval.saturating_sub(elapsed);
            tracing::debug!(
                "job took: {}ms, waiting: {}ms for next run",
                elapsed.as_millis(),
                wait.as_millis()
            );

            loop {
                let child_token = cancellation_token.child_token();
                let sleep = time::sleep(wait);
                tokio::pin!(sleep);

                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        tracing::trace!("stopping drift job");

                        break
                    }
                    _ = &mut sleep => {
                        let start = std::time::Instant::now();

                        tracing::debug!("running job");
                        if let Err(e) = drifter.execute(child_token).await {
                            tracing::error!("drift job failed with error: {}, stopping routine", e);
                            cancellation_token.cancel();
                            continue
                        }

                        let elapsed = start.elapsed();
                        wait = interval.saturating_sub(elapsed);
                        tracing::debug!("job took: {}ms, waiting: {}ms for next run", elapsed.as_millis(), wait.as_millis());
                    }

                }
            }
        }
    });

    cancellation_token
}

struct FuncDrifter<F, Fut>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), DriftError>> + Send + 'static,
{
    func: Arc<F>,
}

impl<F, Fut> Clone for FuncDrifter<F, Fut>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), DriftError>> + Send + 'static,
{
    fn clone(&self) -> Self {
        Self {
            func: self.func.clone(),
        }
    }
}

impl<F, Fut> FuncDrifter<F, Fut>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), DriftError>> + Send + 'static,
{
    fn new(func: F) -> Self {
        Self {
            func: Arc::new(func),
        }
    }

    async fn execute_func(&self) -> anyhow::Result<()> {
        if let Err(e) = (self.func)().await {
            anyhow::bail!(e)
        }

        Ok(())
    }
}

#[async_trait]
impl<F, Fut> Drifter for FuncDrifter<F, Fut>
where
    F: Fn() -> Fut + Send + Sync,
    Fut: Future<Output = Result<(), DriftError>> + Send,
{
    async fn execute(&self, token: CancellationToken) -> anyhow::Result<()> {
        self.execute_func().await?;

        Ok(())
    }
}

#[async_trait]
pub trait Drifter {
    async fn execute(&self, token: CancellationToken) -> anyhow::Result<()>;
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing_test::traced_test;

    use super::*;

    #[tokio::test]
    async fn test_can_schedule_jobs() -> anyhow::Result<()> {
        let token = schedule(Duration::from_millis(50), || async move { Ok(()) });

        tokio::time::sleep(Duration::from_millis(150)).await;

        assert!(!token.is_cancelled());

        Ok(())
    }

    #[derive(Default, Clone)]
    pub struct CounterDrifter {
        counter: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl Drifter for CounterDrifter {
        async fn execute(&self, _cancellation_token: CancellationToken) -> anyhow::Result<()> {
            let mut counter = self.counter.lock().unwrap();
            *counter += 1;

            Ok(())
        }
    }

    #[tokio::test]
    async fn test_can_call_job_multiple_times() -> anyhow::Result<()> {
        let drifter = CounterDrifter::default();

        let token = schedule_drifter(Duration::from_millis(50), drifter.clone());
        tokio::time::sleep(Duration::from_millis(150)).await;

        assert!(!token.is_cancelled());

        let counter = drifter.counter.lock().unwrap();
        assert!(*counter >= 2);

        Ok(())
    }

    #[tokio::test]
    async fn test_cancelled() -> anyhow::Result<()> {
        let drifter = CounterDrifter::default();

        let token = schedule_drifter(Duration::from_millis(50), drifter.clone());
        tokio::time::sleep(Duration::from_millis(75)).await;
        token.cancel();

        assert!(token.is_cancelled());

        let counter = drifter.counter.lock().unwrap();
        assert_eq!(*counter, 2);

        Ok(())
    }

    #[tokio::test]
    #[traced_test]
    async fn test_calls_trace_on_start_and_end() -> anyhow::Result<()> {
        let token = schedule(Duration::from_millis(10), || async {
            tokio::time::sleep(std::time::Duration::from_nanos(1000)).await;

            Ok(())
        });
        tokio::time::sleep(Duration::from_millis(150)).await;

        assert!(!token.is_cancelled());

        assert!(logs_contain("running job"));
        assert!(logs_contain("job took:"));

        Ok(())
    }
}
