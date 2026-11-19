use std::sync::{Arc, Mutex};

type Listener = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct AbortState {
    aborted: bool,
    listeners: Vec<Listener>,
}

/// The owning half of a cancellation pair (JavaScript's `AbortController`): call [`abort`](Self::abort) once and every
/// [`AbortSignal`] handed out from it reads as aborted and fires its listeners.
#[derive(Clone, Default)]
pub struct AbortHandle(Arc<Mutex<AbortState>>);

impl AbortHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn signal(&self) -> AbortSignal {
        AbortSignal(self.0.clone())
    }

    /// Idempotent. Listeners run on the calling thread, after the internal lock is released, so they may freely call
    /// back into whatever they belong to.
    pub fn abort(&self) {
        let listeners = {
            let mut state = self.0.lock().unwrap();
            if state.aborted {
                return;
            }
            state.aborted = true;
            std::mem::take(&mut state.listeners)
        };
        for listener in listeners {
            listener();
        }
    }
}

/// The observing half (JavaScript's `AbortSignal`).
#[derive(Clone)]
pub struct AbortSignal(Arc<Mutex<AbortState>>);

impl AbortSignal {
    pub fn is_aborted(&self) -> bool {
        self.0.lock().unwrap().aborted
    }

    /// Runs `listener` once when the handle aborts. Like `addEventListener('abort', ...)`, it does NOT fire for a signal
    /// that was already aborted before this call.
    pub fn on_abort(&self, listener: impl FnOnce() + Send + 'static) {
        let mut state = self.0.lock().unwrap();
        if !state.aborted {
            state.listeners.push(Box::new(listener));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn abort_flips_every_signal_and_fires_each_listener_once() {
        let handle = AbortHandle::new();
        let (a, b) = (handle.signal(), handle.signal());
        let fired = Arc::new(AtomicUsize::new(0));
        for _ in 0..3 {
            let fired = fired.clone();
            a.on_abort(move || { let _ = fired.fetch_add(1, Ordering::SeqCst); });
        }
        assert!(!a.is_aborted());
        handle.abort();
        handle.abort();
        assert!(a.is_aborted() && b.is_aborted());
        assert_eq!(fired.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_listener_added_after_the_abort_never_fires() {
        let handle = AbortHandle::new();
        handle.abort();
        let fired = Arc::new(AtomicUsize::new(0));
        let f = fired.clone();
        handle.signal().on_abort(move || { let _ = f.fetch_add(1, Ordering::SeqCst); });
        assert_eq!(fired.load(Ordering::SeqCst), 0);
    }
}

/// The future was cancelled through its [`AbortSignal`] before it finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Aborted;

impl std::fmt::Display for Aborted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("aborted")
    }
}

impl std::error::Error for Aborted {}

/// Races `future` against `signal`: the future's output if it finishes first, [`Aborted`] if the signal aborts first
/// (or already had) - in which case the future is dropped, which is how a cancelled request stops. With no signal it is
/// just the future. The runtime-free version of passing an `AbortSignal` to `fetch`.
pub async fn abortable<T>(signal: Option<&AbortSignal>, future: impl Future<Output = T>) -> Result<T, Aborted> {
    use std::pin::pin;
    use std::task::Poll;

    let Some(signal) = signal else { return Ok(future.await) };
    if signal.is_aborted() {
        return Err(Aborted);
    }
    let gate = super::Gate::new();
    let opener = gate.clone();
    signal.on_abort(move || opener.open());

    let mut future = pin!(future);
    let mut aborted = pin!(gate.wait());
    std::future::poll_fn(|cx| {
        if let Poll::Ready(value) = future.as_mut().poll(cx) {
            return Poll::Ready(Ok(value));
        }
        match aborted.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(Aborted)),
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

#[cfg(test)]
mod abortable_tests {
    use super::super::test_executor::Pool;
    use super::super::yield_now::yield_now;
    use super::*;
    use crate::tasks::Gate;

    #[test]
    fn without_a_signal_it_is_just_the_future() {
        let pool = Pool::new();
        let r = pool.spawn(abortable(None, async { yield_now().await; 5 }));
        pool.run_until_stalled();
        assert_eq!(r.take(), Some(Ok(5)));
    }

    #[test]
    fn a_signal_that_aborts_first_drops_the_future() {
        let (pool, handle, never) = (Pool::new(), AbortHandle::new(), Gate::new());
        let signal = handle.signal();
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        struct SetOnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for SetOnDrop {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let flag = SetOnDrop(dropped.clone());
        let r = pool.spawn(async move { abortable(Some(&signal), async move { let _keep = flag; never.wait().await; }).await });
        pool.run_until_stalled();
        assert_eq!(r.take(), None);
        handle.abort();
        pool.run_until_stalled();
        assert_eq!(r.take(), Some(Err(Aborted)));
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst), "the cancelled future was dropped");
    }

    #[test]
    fn an_already_aborted_signal_fails_at_once_and_a_finished_future_wins_over_a_later_abort() {
        let pool = Pool::new();
        let handle = AbortHandle::new();
        handle.abort();
        let signal = handle.signal();
        let r = pool.spawn(async move { abortable(Some(&signal), async { 1 }).await });
        pool.run_until_stalled();
        assert_eq!(r.take(), Some(Err(Aborted)));

        let late = AbortHandle::new();
        let signal = late.signal();
        let r = pool.spawn(async move { abortable(Some(&signal), async { 2 }).await });
        pool.run_until_stalled();
        late.abort();
        assert_eq!(r.take(), Some(Ok(2)));
    }
}
