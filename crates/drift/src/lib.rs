use std::{str::FromStr, sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Local, TimeDelta, Utc};
use std::future::Future;
use tokio::time;
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum DriftError {
    #[error("job failed with inner error: {0}")]
    JobError(#[source] anyhow::Error),
}

/// A scheduled job you can both stop AND wait for.
///
/// The `schedule*` functions return a bare [`CancellationToken`], which can ask
/// a job to stop but cannot tell you when it actually did — the scheduler runs
/// on a detached task and the handle is dropped. For anything that touches
/// external state that is not good enough: a caller that cancels and then exits
/// severs the job mid-write. Awaiting the handle waits for the loop to end,
/// which includes the currently-executing job.
///
/// ```
/// # use std::time::Duration;
/// # async fn example() -> anyhow::Result<()> {
/// let handle = nodrift::schedule_handle(Duration::from_secs(60), || async {
///     // ... do the work
///     Ok(())
/// });
///
/// // ... later, on shutdown:
/// handle.shutdown().await?; // cancel, then wait for the in-flight run
/// # Ok(())
/// # }
/// ```
#[must_use = "dropping the handle detaches the job; use schedule_drifter if that is what you want"]
pub struct DriftHandle {
    cancellation_token: CancellationToken,
    join: tokio::task::JoinHandle<()>,
}

impl DriftHandle {
    /// The token driving this job. Cloneable, and cancelling it is equivalent
    /// to [`DriftHandle::cancel`] — handed out so existing token-based code
    /// keeps working.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
    }

    /// Ask the job to stop. Returns immediately; the in-flight run may still be
    /// going. Pair with [`DriftHandle::wait`], or use
    /// [`DriftHandle::shutdown`].
    pub fn cancel(&self) {
        self.cancellation_token.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation_token.is_cancelled()
    }

    /// Wait for the scheduler to stop, including the job currently running.
    ///
    /// Does NOT cancel — on its own this waits forever on a live schedule.
    /// Cancel first, or use [`DriftHandle::shutdown`].
    pub async fn wait(self) -> Result<(), tokio::task::JoinError> {
        self.join.await
    }

    /// Cancel and wait for the in-flight job to finish. The graceful stop.
    ///
    /// Note the job also receives a child of the cancellation token, so a job
    /// that observes it can wind down early instead of running to completion.
    pub async fn shutdown(self) -> Result<(), tokio::task::JoinError> {
        self.cancellation_token.cancel();
        self.join.await
    }
}

/// Schedule `func` every `interval`, detached. See [`schedule_handle`] to be
/// able to wait for the in-flight run.
pub fn schedule<F, Fut>(interval: Duration, func: F) -> CancellationToken
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), DriftError>> + Send + 'static,
{
    schedule_handle(interval, func).into_detached_token()
}

/// Schedule `func` every `interval`, returning a handle that can wait for the
/// in-flight run to finish. See [`DriftHandle`].
pub fn schedule_handle<F, Fut>(interval: Duration, func: F) -> DriftHandle
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), DriftError>> + Send + 'static,
{
    schedule_drifter_handle(interval, FuncDrifter::new(func))
}

pub fn schedule_cron<F, Fut>(cron: &str, func: F) -> anyhow::Result<CancellationToken>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), DriftError>> + Send + 'static,
{
    let drifter = FuncDrifter::new(func);

    schedule_drifter_cron(cron, drifter)
}

pub fn schedule_drifter_cron<FDrifter>(
    cron: &str,
    drifter: FDrifter,
) -> anyhow::Result<CancellationToken>
where
    FDrifter: Drifter + Send + 'static,
    FDrifter: Clone,
{
    let schedule = ::cron::Schedule::from_str(cron)?;

    let cancellation_token = CancellationToken::new();

    tokio::spawn({
        let cancellation_token = cancellation_token.clone();
        let drifter = drifter.clone();

        async move {
            let upcoming = schedule.upcoming(Utc {});

            let child_token = cancellation_token.child_token();
            for datetime in upcoming {
                let now = Utc::now();

                let diff = datetime - now;
                if diff <= TimeDelta::zero() {
                    tracing::info!(
                        "job schedule for {} was in the past: {}, skipping iteration",
                        datetime.to_string(),
                        now.to_string()
                    );
                    continue;
                }

                let diff = diff.to_std().expect("to be able to get diff time");
                let sleep = time::sleep(diff);
                tokio::pin!(sleep);

                tracing::debug!(
                    "schedule job: {}, waiting: {}s for execution",
                    datetime.to_string(),
                    diff.as_secs()
                );

                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        tracing::trace!("stopping drift job");

                        break
                    }
                    _ = &mut sleep => {
                        let start = std::time::Instant::now();

                        tracing::debug!("running job");
                        if let Err(e) = drifter.execute(child_token.child_token()).await {
                            tracing::error!("drift job failed with error: {}", e);
                            continue
                        }

                        let elapsed = start.elapsed();

                        tracing::debug!("job took: {}ms ", elapsed.as_millis());
                    }

                }
            }
        }
    });

    Ok(cancellation_token)
}
/// Schedule `drifter` every `interval`, detached.
///
/// Returns only a [`CancellationToken`], so the caller can stop the job but
/// cannot wait for it. Prefer [`schedule_drifter_handle`] when the job touches
/// state that a half-finished run would corrupt.
pub fn schedule_drifter<FDrifter>(interval: Duration, drifter: FDrifter) -> CancellationToken
where
    FDrifter: Drifter + Send + 'static,
    FDrifter: Clone,
{
    schedule_drifter_handle(interval, drifter).into_detached_token()
}

/// Schedule `drifter` every `interval`, returning a handle that can wait for
/// the in-flight run to finish. See [`DriftHandle`].
pub fn schedule_drifter_handle<FDrifter>(interval: Duration, drifter: FDrifter) -> DriftHandle
where
    FDrifter: Drifter + Send + 'static,
    FDrifter: Clone,
{
    let cancellation_token = CancellationToken::new();

    let join = tokio::spawn({
        let cancellation_token = cancellation_token.clone();
        let drifter = drifter.clone();

        async move {
            let mut wait = Duration::default();

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
                            let elapsed = start.elapsed();
                            wait = interval.saturating_sub(elapsed);
                            tracing::error!("drift job failed with error: {}, waiting: {}s before trying again", e, wait.as_secs());
                            continue
                        }

                        let elapsed = start.elapsed();
                        wait = interval.saturating_sub(elapsed);

                        let now: DateTime<Local> = Local::now();
                        let next: Option<DateTime<Local>> = now.checked_add_signed(TimeDelta::from_std(wait).expect("to be able to convert duration into time delta"));

                        tracing::debug!(now=now.to_string(), next=next.map(|n| n.to_string()), "job took: {}ms, waiting: {}ms for next run", elapsed.as_millis(), wait.as_millis() );
                    }

                }
            }
        }
    });

    DriftHandle {
        cancellation_token,
        join,
    }
}

impl DriftHandle {
    /// Drop the join handle, keeping only the token — the pre-handle
    /// behaviour. Used by the `schedule*` functions that still return a bare
    /// token, so both APIs share one implementation.
    fn into_detached_token(self) -> CancellationToken {
        self.cancellation_token
    }
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

    /// A drifter whose job takes a while, so "did shutdown wait for it?" is
    /// observable rather than a race.
    #[derive(Default, Clone)]
    pub struct SlowDrifter {
        started: Arc<Mutex<usize>>,
        finished: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl Drifter for SlowDrifter {
        async fn execute(&self, _cancellation_token: CancellationToken) -> anyhow::Result<()> {
            *self.started.lock().unwrap() += 1;
            tokio::time::sleep(Duration::from_millis(200)).await;
            *self.finished.lock().unwrap() += 1;
            Ok(())
        }
    }

    /// The point of DriftHandle: after shutdown() returns, the job that was
    /// running has finished. With a bare CancellationToken the caller could
    /// only cancel and hope.
    #[tokio::test]
    async fn test_shutdown_waits_for_the_running_job() -> anyhow::Result<()> {
        let drifter = SlowDrifter::default();
        let handle = schedule_drifter_handle(Duration::from_millis(50), drifter.clone());

        // Let the first job get underway, then stop mid-run.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(*drifter.started.lock().unwrap(), 1);
        assert_eq!(
            *drifter.finished.lock().unwrap(),
            0,
            "job should still be running, otherwise this test proves nothing"
        );

        handle.shutdown().await?;

        assert_eq!(
            *drifter.finished.lock().unwrap(),
            1,
            "shutdown must not return before the in-flight job completes"
        );

        Ok(())
    }

    /// wait() on its own must not cancel — otherwise callers holding a handle
    /// for observability would silently stop their own schedule.
    #[tokio::test]
    async fn test_wait_does_not_cancel() -> anyhow::Result<()> {
        let drifter = CounterDrifter::default();
        let handle = schedule_drifter_handle(Duration::from_millis(30), drifter.clone());

        let token = handle.cancellation_token();
        assert!(!token.is_cancelled());

        // Waiting forever is the correct behaviour on a live schedule, so only
        // assert that it does not resolve on its own.
        let waited = tokio::time::timeout(Duration::from_millis(120), handle.wait()).await;
        assert!(
            waited.is_err(),
            "wait() should not return while the schedule is live"
        );
        assert!(!token.is_cancelled(), "wait() must not cancel the schedule");

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
    #[tokio::test]
    #[traced_test]
    async fn test_calls_trace_on_start_and_end_long() -> anyhow::Result<()> {
        let token = schedule(Duration::from_millis(100), || async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            Ok(())
        });
        tokio::time::sleep(Duration::from_millis(500)).await;

        assert!(!token.is_cancelled());

        assert!(logs_contain("running job"));
        assert!(logs_contain("job took:"));

        Ok(())
    }

    #[tokio::test]
    #[traced_test]
    async fn test_cron() -> anyhow::Result<()> {
        let token = schedule_cron("* * * * * *", || async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            Ok(())
        })?;

        tokio::time::sleep(Duration::from_secs(5)).await;

        assert!(!token.is_cancelled());

        assert!(logs_contain("running job"));
        assert!(logs_contain("job took:"));

        Ok(())
    }

    #[tokio::test]
    #[traced_test]
    async fn test_cron_no_wait() -> anyhow::Result<()> {
        let token = schedule_cron("* * * * * *", || async { Ok(()) })?;

        tokio::time::sleep(Duration::from_secs(5)).await;

        assert!(!token.is_cancelled());

        assert!(logs_contain("running job"));
        assert!(logs_contain("job took:"));

        Ok(())
    }

    #[tokio::test]
    #[traced_test]
    async fn test_cron_job_taking_longer_than_cycle() -> anyhow::Result<()> {
        let token = schedule_cron("* * * * * *", || async {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

            Ok(())
        })?;

        tokio::time::sleep(Duration::from_secs(5)).await;

        assert!(!token.is_cancelled());

        Ok(())
    }
}
