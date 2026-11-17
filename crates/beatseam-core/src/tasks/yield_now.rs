use std::future::poll_fn;
use std::task::Poll;

/// Returns `Pending` once, waking itself, so other ready tasks get a turn before this one continues - the runtime-free
/// stand-in for the TypeScript's `await new Promise((r) => setTimeout(r, 0))`.
pub async fn yield_now() {
    let mut yielded = false;
    poll_fn(move |cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await
}
