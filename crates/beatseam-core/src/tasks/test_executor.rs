//! A tiny deterministic single-threaded executor for the task queue's tests: spawn futures, then poll whichever ones
//! have been woken until nothing is runnable.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};

struct Flag(AtomicBool);

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct Task {
    future: Option<Pin<Box<dyn Future<Output = ()>>>>,
    woken: Arc<Flag>,
}

#[derive(Default)]
pub struct Pool {
    tasks: RefCell<Vec<Rc<RefCell<Task>>>>,
}

/// The result slot of a spawned future, and a way to drop it before it finishes.
pub struct Handle<T> {
    result: Rc<RefCell<Option<T>>>,
    task: Rc<RefCell<Task>>,
}

impl<T> Handle<T> {
    pub fn take(&self) -> Option<T> {
        self.result.borrow_mut().take()
    }

    pub fn is_done(&self) -> bool {
        self.task.borrow().future.is_none()
    }

    /// Drops the future where it stands (the caller "giving up"), running its destructors.
    pub fn cancel(&self) {
        let dropped = self.task.borrow_mut().future.take();
        drop(dropped);
    }
}

impl Pool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn spawn<T: 'static>(&self, future: impl Future<Output = T> + 'static) -> Handle<T> {
        let result = Rc::new(RefCell::new(None));
        let slot = result.clone();
        let task = Rc::new(RefCell::new(Task {
            future: Some(Box::pin(async move { *slot.borrow_mut() = Some(future.await) })),
            woken: Arc::new(Flag(AtomicBool::new(true))),
        }));
        self.tasks.borrow_mut().push(task.clone());
        Handle { result, task }
    }

    /// One pass: polls every task that has been woken. Returns whether anything was polled.
    pub fn step(&self) -> bool {
        let tasks: Vec<_> = self.tasks.borrow().clone();
        let mut polled = false;
        for task in tasks {
            let (mut future, flag) = {
                let mut t = task.borrow_mut();
                if !t.woken.0.swap(false, Ordering::SeqCst) {
                    continue;
                }
                match t.future.take() {
                    Some(f) => (f, t.woken.clone()),
                    None => continue,
                }
            };
            polled = true;
            let waker = Waker::from(flag);
            if future.as_mut().poll(&mut Context::from_waker(&waker)) == Poll::Pending {
                // Put it back unless the future was cancelled meanwhile (nothing can cancel it mid-poll here).
                task.borrow_mut().future = Some(future);
            }
        }
        polled
    }

    pub fn run_until_stalled(&self) {
        for _ in 0..10_000 {
            if !self.step() {
                return;
            }
        }
        panic!("the pool kept finding runnable tasks: a task is spinning");
    }
}

/// Returns `Pending` once (waking itself), like `setTimeout(0)` letting other tasks run.
pub async fn yield_now() {
    let mut yielded = false;
    std::future::poll_fn(move |cx| {
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
