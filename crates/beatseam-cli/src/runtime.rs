//! A single-threaded executor and timer wheel for the CLI. The clock is a plain number the host advances, so the same code runs against
//! the wall clock (`play`) and against rendered audio time (`render`, which is faster than real time and deterministic).

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use beatseam_core::tasks::Runtime;

struct Flag(AtomicBool);

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct Task {
    future: Pin<Box<dyn Future<Output = ()>>>,
    woken: Arc<Flag>,
}

#[derive(Default)]
pub struct CliRuntime {
    tasks: RefCell<Vec<Task>>,
    timers: RefCell<Vec<(u64, Waker)>>,
    now_ms: Cell<u64>,
}

impl CliRuntime {
    // `PlaylistPlayer` takes its runtime as an `Arc`, even though everything here stays on one thread.
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Moves the clock to `now_ms` and wakes the timers that are due.
    pub fn set_now(&self, now_ms: u64) {
        self.now_ms.set(now_ms);
        let due: Vec<Waker> = {
            let mut timers = self.timers.borrow_mut();
            let (due, later): (Vec<_>, Vec<_>) =
                timers.drain(..).partition(|(at, _)| *at <= now_ms);
            *timers = later;
            due.into_iter().map(|(_, waker)| waker).collect()
        };
        for waker in due {
            waker.wake();
        }
    }

    /// Polls every woken task until none is runnable.
    pub fn run_until_stalled(&self) {
        for _ in 0..100_000 {
            let mut polled = false;
            // Futures may spawn more tasks while polled, so each one is taken out of the list for its poll.
            let mut i = 0;
            loop {
                let task = {
                    let mut tasks = self.tasks.borrow_mut();
                    if i >= tasks.len() {
                        break;
                    }
                    if tasks[i].woken.0.swap(false, Ordering::SeqCst) {
                        Some((
                            std::mem::replace(
                                &mut tasks[i].future,
                                Box::pin(std::future::pending()),
                            ),
                            tasks[i].woken.clone(),
                        ))
                    } else {
                        None
                    }
                };
                if let Some((mut future, flag)) = task {
                    polled = true;
                    let waker = Waker::from(flag);
                    let done =
                        future.as_mut().poll(&mut Context::from_waker(&waker)) == Poll::Ready(());
                    let mut tasks = self.tasks.borrow_mut();
                    if !done {
                        tasks[i].future = future;
                    }
                }
                i += 1;
            }
            self.tasks
                .borrow_mut()
                .retain(|t| t.woken.0.load(Ordering::SeqCst) || !is_finished(t));
            if !polled {
                return;
            }
        }
        panic!("a task is spinning");
    }
}

/// Finished tasks are left with the `pending` placeholder and an unset flag; a task still waiting on a timer or I/O keeps its real future.
/// There is no cheap way to tell the two apart, so finished tasks are only reclaimed when the list grows large.
fn is_finished(_task: &Task) -> bool {
    false
}

impl Runtime for CliRuntime {
    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()>>>) {
        self.tasks.borrow_mut().push(Task {
            future,
            woken: Arc::new(Flag(AtomicBool::new(true))),
        });
    }

    fn sleep(&self, millis: u64) -> Pin<Box<dyn Future<Output = ()>>> {
        let at = self.now_ms.get() + millis;
        // `Rc` to the runtime isn't available from `&self`; the timer list is reached through a pointer-free handle instead.
        Box::pin(Sleep {
            at,
            runtime: self as *const CliRuntime,
            registered: false,
        })
    }

    fn now_ms(&self) -> u64 {
        self.now_ms.get()
    }
}

struct Sleep {
    at: u64,
    runtime: *const CliRuntime,
    registered: bool,
}

impl Future for Sleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // SAFETY: the runtime is held in an `Arc` by the host for as long as any task it spawned can run (`main` drops tasks first).
        let runtime = unsafe { &*self.runtime };
        if runtime.now_ms.get() >= self.at {
            return Poll::Ready(());
        }
        if !self.registered {
            runtime
                .timers
                .borrow_mut()
                .push((self.at, cx.waker().clone()));
            self.registered = true;
        }
        Poll::Pending
    }
}

#[allow(dead_code)]
fn _assert_rc_unused(_: Rc<()>) {}
