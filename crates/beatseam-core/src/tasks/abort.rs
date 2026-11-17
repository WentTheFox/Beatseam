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
