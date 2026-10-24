//! What the ported modules share and Python gets from its runtime: a typed `generate` through a `dyn` client, and
//! `asyncio.gather` over requests sent side by side.

use std::future::Future;
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
    max_output_tokens: i64,
    ledger: Option<&Ledger>,
) -> Result<Generation<T>, LLMError>
where
    T: Schema + DeserializeOwned + Serialize,
{
    let schema = Typed::<T>::default();
    let generated = llm.generate_value(purpose, messages, &schema, max_output_tokens, ledger).await?;
    let data = serde_json::from_value(generated.data).map_err(|error| LLMError::Failed(error.to_string()))?;
    Ok(Generation { data, cost: generated.cost })
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

/// `asyncio.gather(..., return_exceptions=True)`: every future runs to its end, whatever its siblings did, and each
/// result comes back in the order given. A failure in one must not leave a sibling's request unsent or unbilled.
pub(crate) async fn join_all<'a, T>(futures: Vec<BoxFuture<'a, T>>) -> Vec<T> {
    let mut pending: Vec<Option<BoxFuture<'a, T>>> = futures.into_iter().map(Some).collect();
    let mut results: Vec<Option<T>> = pending.iter().map(|_| None).collect();
    std::future::poll_fn(move |cx| {
        let mut waiting = false;
        for (slot, result) in pending.iter_mut().zip(results.iter_mut()) {
            if let Some(future) = slot {
                match future.as_mut().poll(cx) {
                    Poll::Ready(value) => {
                        *result = Some(value);
                        *slot = None;
                    }
                    Poll::Pending => waiting = true,
                }
            }
        }
        if waiting {
            return Poll::Pending;
        }
        Poll::Ready(std::mem::take(&mut results).into_iter().flatten().collect())
    })
    .await
}

/// Two futures of different types run side by side to their ends: `gather` of two tasks with `return_exceptions`.
pub(crate) async fn join2<A, B>(a: impl Future<Output = A>, b: impl Future<Output = B>) -> (A, B) {
    let (mut a, mut b) = (std::pin::pin!(a), std::pin::pin!(b));
    let (mut first, mut second) = (None, None);
    std::future::poll_fn(move |cx| {
        if first.is_none()
            && let Poll::Ready(value) = a.as_mut().poll(cx)
        {
            first = Some(value);
        }
        if second.is_none()
            && let Poll::Ready(value) = b.as_mut().poll(cx)
        {
            second = Some(value);
        }
        if first.is_some() && second.is_some() {
            return Poll::Ready((first.take().expect("polled to its end"), second.take().expect("polled to its end")));
        }
        Poll::Pending
    })
    .await
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
