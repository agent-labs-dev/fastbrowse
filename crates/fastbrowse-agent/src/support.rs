//! What the ported modules share and Python gets from its runtime: `asyncio.gather` over requests sent side by side.

use std::task::Poll;

use fastbrowse_core::BoxFuture;

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
