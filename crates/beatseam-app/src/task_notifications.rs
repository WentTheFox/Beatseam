//! Port of `store/notifications/syncTaskNotifications`: mirrors the shared task queue into the notification list.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use beatseam_core::audio_engine::AudioEngine;
use beatseam_core::file_access::FileAccess;
use beatseam_core::library_store::LibraryStore;
use beatseam_core::tasks::{Runtime, Subscription, TaskInfo, TaskQueue, TaskState};

use crate::app::{App, LibraryJobs};
use crate::app_actions::ProgressNotification;
use crate::notifications::{NotificationProgress, NotificationStatus, ProgressUpsert};

/// How long a finished task with a known total stays on screen as "done" before it's dismissed on its own.
pub const DONE_AUTO_DISMISS_MS: u64 = 4000;

/// Lower is more active - which of several same-id tasks a notification shows.
fn state_rank(state: TaskState) -> u8 {
    match state {
        TaskState::Running => 0,
        TaskState::Paused => 1,
        TaskState::Queued => 2,
    }
}

/// Mirrors the shared task queue into the notification list: one entry per queued/running/paused task - folder scans, the Unplaylisted
/// walk, metadata and lyrics passes - with live progress, a Queued/Paused marker while it's waiting its turn, and Cancel where the task
/// supports it. Driven by the queue itself rather than per-feature wiring, so every new queued task gets a notification for free.
///
/// A finished task that had a known total is left on screen as "done" briefly (so a completed pass doesn't just vanish); anything else
/// is dismissed as soon as it finishes.
///
/// The queue is thread-safe but the app state is not, so the queue's change listener only raises a flag (and calls `wake`, which the
/// host uses to schedule a call to [`process`](Self::process) on its UI thread). Dropping this (or [`stop`](Self::stop)) unsubscribes
/// and cancels pending auto-dismisses.
pub struct TaskNotificationSync<F, BF, S, E: AudioEngine + 'static, R: Runtime + 'static, J> {
    app: App<F, BF, S, E, R, J>,
    queue: TaskQueue,
    dirty: Arc<AtomicBool>,
    previous: RefCell<HashMap<String, TaskInfo>>,
    /// Pending "done" auto-dismisses by id - cancelled if a new task with the same id (e.g. rescanning the same folder) starts before it
    /// fires. The value is a generation so a sleeping dismisser knows it is stale.
    dismiss_generations: Rc<RefCell<HashMap<String, u64>>>,
    next_generation: RefCell<u64>,
    subscription: RefCell<Option<Subscription>>,
}

impl<F, BF, S, E, R, J> TaskNotificationSync<F, BF, S, E, R, J>
where
    F: FileAccess + 'static,
    BF: FileAccess + 'static,
    S: LibraryStore + 'static,
    E: AudioEngine + 'static,
    R: Runtime + 'static,
    J: LibraryJobs + 'static,
{
    pub fn start(app: App<F, BF, S, E, R, J>, queue: TaskQueue, wake: Arc<dyn Fn() + Send + Sync>) -> Rc<Self> {
        let dirty = Arc::new(AtomicBool::new(true));
        let flag = dirty.clone();
        let subscription = queue.subscribe(move || {
            flag.store(true, Ordering::SeqCst);
            wake();
        });
        let sync = Rc::new(Self {
            app,
            queue,
            dirty,
            previous: RefCell::new(HashMap::new()),
            dismiss_generations: Rc::default(),
            next_generation: RefCell::new(0),
            subscription: RefCell::new(Some(subscription)),
        });
        sync.process();
        sync
    }

    pub fn stop(&self) {
        self.subscription.borrow_mut().take();
        self.dismiss_generations.borrow_mut().clear();
    }

    /// Brings the notification list in step with the queue, if it changed since the last call.
    ///
    /// The queue holds back a progress update that arrives inside its throttle window (there is no timer for the trailing one), so
    /// this publishes any such update first - call it on the host's own tick as well as when woken, and a progress bar never sticks.
    pub fn process(self: &Rc<Self>) {
        self.queue.flush_progress();
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        // Several tasks can share an id - e.g. a refresh requesting a new lyrics pass while one is already running queues a second
        // lyrics-match behind it. Show the most active one: otherwise the queued duplicate (no progress yet) would overwrite the running
        // pass's entry, so the notification would read "Queued" while matching was visibly progressing elsewhere.
        let mut current: HashMap<String, TaskInfo> = HashMap::new();
        for task in self.queue.tasks().iter() {
            match current.get(&task.id) {
                Some(existing) if state_rank(task.state) >= state_rank(existing.state) => {}
                _ => {
                    current.insert(task.id.clone(), task.clone());
                }
            }
        }
        let previous = std::mem::take(&mut *self.previous.borrow_mut());
        for (id, task) in &previous {
            if current.contains_key(id) {
                continue;
            }
            let total = task.progress.as_ref().map_or(0, |p| p.total);
            if total > 0 {
                self.app.upsert_notification(ProgressNotification { entry: ProgressUpsert { id: id.clone(), title: task.label.clone(), detail: None, progress: Some(NotificationProgress { current: total, total, done: true }), action_label: None, status: None }, action: None });
                self.schedule_dismiss(id);
            } else {
                self.app.dismiss_notification(id);
            }
        }
        for task in current.values() {
            self.dismiss_generations.borrow_mut().remove(&task.id);
            let title = match task.progress.as_ref().map(|p| p.detail.as_str()).filter(|d| !d.is_empty()) {
                Some(detail) => format!("{} - {detail}", task.label),
                None => task.label.clone(),
            };
            let progress = NotificationProgress { current: task.progress.as_ref().map_or(0, |p| p.current), total: task.progress.as_ref().map_or(0, |p| p.total), done: false };
            let status = match task.state {
                TaskState::Running => None,
                TaskState::Queued => Some(NotificationStatus::Queued),
                TaskState::Paused => Some(NotificationStatus::Paused),
            };
            let action = task.cancel.clone().map(|cancel| ("Cancel".to_owned(), Rc::new(move || cancel()) as Rc<dyn Fn()>));
            self.app.upsert_notification(ProgressNotification { entry: ProgressUpsert { id: task.id.clone(), title, detail: None, progress: Some(progress), action_label: None, status }, action });
        }
        *self.previous.borrow_mut() = current;
    }

    fn schedule_dismiss(self: &Rc<Self>, id: &str) {
        let generation = {
            let mut next = self.next_generation.borrow_mut();
            *next += 1;
            *next
        };
        self.dismiss_generations.borrow_mut().insert(id.to_owned(), generation);
        let (this, id) = (self.clone(), id.to_owned());
        let runtime = self.app.cfg().runtime.clone();
        self.app.spawn(async move {
            runtime.sleep(DONE_AUTO_DISMISS_MS).await;
            if this.dismiss_generations.borrow().get(&id) == Some(&generation) {
                this.dismiss_generations.borrow_mut().remove(&id);
                this.app.dismiss_notification(&id);
            }
        });
    }
}
