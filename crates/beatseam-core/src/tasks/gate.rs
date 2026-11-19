use std::future::poll_fn;
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};

#[derive(Default)]
struct GateState {
    open: bool,
    wakers: Vec<Waker>,
}

/// A one-shot latch: any number of tasks can `wait()` on it, and `open()` releases all of them (and every later
/// waiter) for good. The runtime-free replacement for the TypeScript's "promise plus its resolver". Cloning shares it.
#[derive(Clone, Default)]
pub struct Gate(Arc<Mutex<GateState>>);

impl Gate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&self) {
        let wakers = {
            let mut state = self.0.lock().unwrap();
            state.open = true;
            std::mem::take(&mut state.wakers)
        };
        for waker in wakers {
            waker.wake();
        }
    }

    pub fn is_open(&self) -> bool {
        self.0.lock().unwrap().open
    }

    pub fn wait(&self) -> impl Future<Output = ()> + Send + use<> {
        let state = self.0.clone();
        poll_fn(move |cx| {
            let mut state = state.lock().unwrap();
            if state.open {
                Poll::Ready(())
            } else {
                state.wakers.push(cx.waker().clone());
                Poll::Pending
            }
        })
    }
}
