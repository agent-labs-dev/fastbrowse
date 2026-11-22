//! What the ported modules share and Python gets from its runtime: a typed `generate` through a `dyn` client, and
//! `asyncio.gather` over requests sent side by side.

use std::task::Poll;

use fastbrowse_core::BoxFuture;
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, Typed};
use fastbrowse_core::models::LLMPurpose;
use fastbrowse_core::schema::Schema;
use fastbrowse_core::telemetry::Ledger;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// `llm.generate(purpose, messages, T, ...)`. `LLMClientExt::generate` needs a sized client, and the modules hold
/// theirs as `dyn` so a failover can stand in for one provider.
pub(crate) async fn generate<T>(
    llm: &dyn LLMClient,
    purpose: LLMPurpose,
    messages: &[Message],
    ledger: Option<&Ledger>,
) -> Result<Generation<T>, LLMError>
where
    T: Schema + DeserializeOwned + Serialize,
{
    let schema = Typed::<T>::default();
    let generated = llm.generate_value(purpose, messages, &schema, ledger).await?;
    let data = serde_json::from_value(generated.data).map_err(|error| LLMError::schema(error.to_string()))?;
    Ok(Generation { data, cost: generated.cost })
}

/// `try_join_all` for passes that each reserve against the ledger. A dollar cap is checked against the receipts
/// recorded when a pass reserves, so passes sent side by side would each pass before any records: under one they run
/// one after another, each seeing the spend before it. Call caps need no wait, since reserving counts the calls.
pub(crate) async fn try_join_reserving<'a, T, E>(
    ledger: Option<&Ledger>,
    futures: Vec<BoxFuture<'a, Result<T, E>>>,
) -> Result<Vec<T>, E> {
    if !ledger.is_some_and(|ledger| ledger.limits.max_dollars.is_some()) {
        return try_join_all(futures).await;
    }
    let mut results = Vec::with_capacity(futures.len());
    for future in futures {
        results.push(future.await?);
    }
    Ok(results)
}

/// `asyncio.gather` that stops at the first error: every future is polled in the order given, so requests leave in
/// the order Python created their tasks, and the first failure drops the rest, as cancelling their tasks does.
pub(crate) async fn try_join_all<'a, T, E>(futures: Vec<BoxFuture<'a, Result<T, E>>>) -> Result<Vec<T>, E> {
    let mut pending: Vec<Option<BoxFuture<'a, Result<T, E>>>> = futures.into_iter().map(Some).collect();
    let mut results: Vec<Option<T>> = pending.iter().map(|_| None).collect();
    std::future::poll_fn(move |cx| {
        let mut waiting = false;
        for (slot, result) in pending.iter_mut().zip(results.iter_mut()) {
            if let Some(future) = slot {
                match future.as_mut().poll(cx) {
                    Poll::Ready(Ok(value)) => {
                        *result = Some(value);
                        *slot = None;
                    }
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Pending => waiting = true,
                }
            }
        }
        if waiting {
            return Poll::Pending;
        }
        Poll::Ready(Ok(std::mem::take(&mut results).into_iter().flatten().collect()))
    })
    .await
}

/// Runs long synchronous `work`, such as reading a page of megabytes, without holding up the runtime's other tasks.
/// A task this one woke last waits in this worker's next-task slot, which an idle worker cannot take: on the Python
/// module's two-worker runtime, one run's read held another run's clicks unpolled, their deadlines never armed, for
/// fifteen minutes (#1284). `block_in_place` hands that slot and the worker on before `work` starts. A current-thread
/// runtime has no other worker to hand them to, and refuses `block_in_place`; outside a runtime there is nothing to
/// hold up.
pub(crate) fn compute<R>(work: impl FnOnce() -> R) -> R {
    match tokio::runtime::Handle::try_current().map(|handle| handle.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(work),
        _ => work(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tokio::sync::Notify;

    use super::compute;

    #[test]
    fn long_work_leaves_a_task_it_woke_to_another_worker() {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        runtime.block_on(async {
            let notify = Arc::new(Notify::new());
            let woken = notify.clone();
            let waiter = tokio::spawn(async move {
                woken.notified().await;
                Instant::now()
            });
            tokio::time::sleep(Duration::from_millis(100)).await;
            let busy = tokio::spawn(async move {
                notify.notify_one();
                compute(|| {
                    let end = Instant::now() + Duration::from_secs(2);
                    while Instant::now() < end {
                        std::hint::spin_loop();
                    }
                });
                Instant::now()
            });
            let (woke, done) = (waiter.await.unwrap(), busy.await.unwrap());
            assert!(woke + Duration::from_secs(1) < done, "the woken task waited out the work");
        });
    }

    #[tokio::test]
    async fn long_work_runs_in_place_on_a_current_thread_runtime() {
        assert_eq!(compute(|| 1), 1);
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    /// Runs a future that never waits on I/O; the fakes' futures resolve on their first polls.
    pub fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return output;
            }
        }
    }
}
