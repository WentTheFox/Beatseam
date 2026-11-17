//! Process-wide queue for every long-running library task (folder scans, the Unplaylisted walk, metadata/lyrics
//! passes), so they take turns instead of racing each other for the same file APIs - on Windows in particular,
//! overlapping storage-broker reads and full-library store writes made the whole system sluggish.
//!
//! Exactly one task holds the queue at a time. Three priorities, highest first - the highest-priority waiting task
//! always goes next, and tasks of the same priority run in request order:
//!  - [`Foreground`](TaskPriority::Foreground): something the user is actively waiting on (Add Folder, Rescan, opening
//!    Unplaylisted). Runs to completion once started.
//!  - [`Visible`](TaskPriority::Visible): resumable work for what's on screen right now (e.g. tag reads for the tracks an
//!    Unplaylisted view just surfaced) - shouldn't wait behind a whole-library pass.
//!  - [`Background`](TaskPriority::Background): long, resumable whole-library passes (metadata scan, lyrics matching).
//!
//! Resumable tasks call [`TaskContext::checkpoint`] between items: if something of higher priority is waiting at that
//! moment, the task pauses there - handing the queue over - and resumes where it left off once that work is done. They
//! never run at the same time as anything else.
//!
//! Not for the startup restore path (see CLAUDE.md) - resolving the last-played playlist and its current track must
//! never wait on this. Don't await one task from inside another's body - the inner one can't start until the outer one
//! finishes, so that would deadlock.
//!
//! # Differences from the TypeScript
//!
//! * A task joins the queue when its future is first polled (spawned or awaited), not at the moment `run_task` is
//!   called as in the TypeScript, where the call itself enqueues. "Request order" is therefore first-poll order: spawn
//!   the future promptly if its place in line matters.
//! * It is an instance (`TaskQueue`, a cheap `Clone` handle), not a module-level global; the app creates one and passes
//!   it around. Nothing here needs a specific async runtime or thread: it is `Send + Sync` and only waits on its own
//!   [`Gate`](super::Gate)s.
//! * Dropping a task's future (e.g. the caller gives up) removes it from the queue and hands the turn on, which the
//!   promise-based original could not express.
//! * Progress updates are throttled by an injected clock, but there is no timer for the trailing update: when a report
//!   arrives inside the throttle window it is held until the next state change or [`TaskQueue::flush_progress`] (the UI
//!   frame loop can call it). Every state change publishes immediately, so a finishing task always shows its final
//!   progress.

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::abort::{AbortHandle, AbortSignal};
use super::gate::Gate;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskPriority {
    Foreground,
    Visible,
    Background,
}

impl TaskPriority {
    fn rank(self) -> i32 {
        match self {
            Self::Foreground => 2,
            Self::Visible => 1,
            Self::Background => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskState {
    Queued,
    Running,
    Paused,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskProgress {
    /// Short status text appended to the label (e.g. "1234 files in 56 folders"), or empty for none.
    pub detail: String,
    pub current: u64,
    /// 0 when there's no known total yet.
    pub total: u64,
}

type Cancel = Arc<dyn Fn() + Send + Sync>;

/// A task as the UI sees it (a notification row).
#[derive(Clone)]
pub struct TaskInfo {
    pub id: String,
    pub label: String,
    pub priority: TaskPriority,
    pub state: TaskState,
    pub progress: Option<TaskProgress>,
    /// Present when the task can be cancelled (see [`TaskOptions::cancel`]).
    pub cancel: Option<Cancel>,
}

impl std::fmt::Debug for TaskInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskInfo")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("priority", &self.priority)
            .field("state", &self.state)
            .field("progress", &self.progress)
            .field("cancellable", &self.cancel.is_some())
            .finish()
    }
}

pub struct TaskOptions {
    /// For display only (e.g. a notification id) - two tasks with the same id are still two separate tasks, except
    /// through [`TaskQueue::run_latest_task`] and [`TaskQueue::run_joinable_task`], which key on it.
    pub id: String,
    pub label: String,
    pub priority: TaskPriority,
    cancel: Option<Cancel>,
    signal: Option<AbortSignal>,
}

impl TaskOptions {
    pub fn new(id: impl Into<String>, label: impl Into<String>, priority: TaskPriority) -> Self {
        Self { id: id.into(), label: label.into(), priority, cancel: None, signal: None }
    }

    /// Exposed on [`TaskInfo`] for a Cancel button; making the body actually stop is the caller's job, via
    /// [`signal`](Self::signal).
    pub fn cancel(mut self, cancel: impl Fn() + Send + Sync + 'static) -> Self {
        self.cancel = Some(Arc::new(cancel));
        self
    }

    /// If it aborts while the task is still queued, the task leaves the queue right away and the body runs once,
    /// unqueued, so it can check the signal and return its own cancellation (instead of a cancelled task waiting out
    /// everything ahead of it).
    pub fn signal(mut self, signal: AbortSignal) -> Self {
        self.signal = Some(signal);
        self
    }
}

type Listener = Arc<dyn Fn() + Send + Sync>;

/// Minimum time between progress publications (see the module docs).
const PROGRESS_THROTTLE_MS: u64 = 250;

struct Entry {
    id: String,
    label: String,
    priority: TaskPriority,
    state: TaskState,
    progress: Option<TaskProgress>,
    cancel: Option<Cancel>,
    signal: Option<AbortSignal>,
    /// Opened when this entry is (re)granted the queue.
    gate: Gate,
}

struct Pass {
    pass_id: u64,
    join_key: String,
    /// `Arc<Mutex<Box<dyn FnMut(&[Item]) -> bool + Send>>>` for the pass's item type, type-erased.
    add_items: Arc<dyn Any + Send + Sync>,
    done: Gate,
}

#[derive(Default)]
struct Inner {
    next_key: u64,
    entries: HashMap<u64, Entry>,
    holder: Option<u64>,
    /// Waiting entries (queued or paused), in the order they'll be granted within their priority.
    waiting: Vec<u64>,
    listeners: Vec<(u64, Listener)>,
    next_listener: u64,
    snapshot: Arc<Vec<TaskInfo>>,
    last_publish_at: u64,
    pending_publish: bool,
    next_token: u64,
    latest_by_id: HashMap<String, (u64, AbortHandle)>,
    next_pass: u64,
    running_passes: HashMap<String, Pass>,
}

/// Things to do once the lock is released: open gates, tell listeners.
#[derive(Default)]
struct Effects {
    gates: Vec<Gate>,
    notify: Option<Vec<Listener>>,
}

impl Inner {
    fn publish(&mut self, now: u64, fx: &mut Effects) {
        self.last_publish_at = now;
        self.pending_publish = false;
        let ordered = self.holder.iter().chain(self.waiting.iter());
        self.snapshot = Arc::new(
            ordered
                .filter_map(|k| self.entries.get(k))
                .map(|e| TaskInfo {
                    id: e.id.clone(),
                    label: e.label.clone(),
                    priority: e.priority,
                    state: e.state,
                    progress: e.progress.clone(),
                    cancel: e.cancel.clone(),
                })
                .collect(),
        );
        fx.notify = Some(self.listeners.iter().map(|(_, l)| l.clone()).collect());
    }

    /// Grants the queue to the highest-priority waiting task if it is free; always publishes.
    fn pump(&mut self, now: u64, fx: &mut Effects) {
        if self.holder.is_none() {
            // Highest priority first; the earliest entry wins within a priority.
            let mut next: Option<u64> = None;
            for key in &self.waiting {
                if next.is_none_or(|n| self.entries[key].priority.rank() > self.entries[&n].priority.rank()) {
                    next = Some(*key);
                }
            }
            if let Some(next) = next {
                self.waiting.retain(|k| *k != next);
                self.holder = Some(next);
                let entry = self.entries.get_mut(&next).expect("a waiting key has an entry");
                entry.state = TaskState::Running;
                fx.gates.push(entry.gate.clone());
            }
        }
        self.publish(now, fx);
    }
}

struct Shared {
    inner: Mutex<Inner>,
    now_ms: Box<dyn Fn() -> u64 + Send + Sync>,
}

/// See the module docs. Clone it freely; every clone is the same queue.
#[derive(Clone)]
pub struct TaskQueue {
    shared: Arc<Shared>,
}

/// Keeps a listener registered until dropped (or [`detach`](Self::detach)ed).
pub struct Subscription {
    queue: TaskQueue,
    id: Option<u64>,
}

impl Subscription {
    /// Keep the listener for the life of the queue.
    pub fn detach(mut self) {
        self.id = None;
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.queue.shared.inner.lock().unwrap().listeners.retain(|(l, _)| *l != id);
        }
    }
}

impl TaskQueue {
    /// `now_ms` is a monotonic-enough millisecond clock (only differences are used).
    pub fn new(now_ms: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        Self { shared: Arc::new(Shared { inner: Mutex::new(Inner::default()), now_ms: Box::new(now_ms) }) }
    }

    /// Runs `f` under the lock, then performs the side effects it queued up (opening gates, notifying listeners) with
    /// the lock released, so none of them can deadlock by calling back into the queue.
    fn with<R>(&self, f: impl FnOnce(&mut Inner, &mut Effects, u64) -> R) -> R {
        let now = (self.shared.now_ms)();
        let mut fx = Effects::default();
        let result = f(&mut self.shared.inner.lock().unwrap(), &mut fx, now);
        for gate in fx.gates {
            gate.open();
        }
        for listener in fx.notify.into_iter().flatten() {
            listener();
        }
        result
    }

    /// Every task currently queued, running or paused, holder first - the same `Arc` until something changes.
    pub fn tasks(&self) -> Arc<Vec<TaskInfo>> {
        self.shared.inner.lock().unwrap().snapshot.clone()
    }

    /// Fires on every published change. Dropping the returned [`Subscription`] unsubscribes.
    pub fn subscribe(&self, listener: impl Fn() + Send + Sync + 'static) -> Subscription {
        let mut inner = self.shared.inner.lock().unwrap();
        inner.next_listener += 1;
        let id = inner.next_listener;
        inner.listeners.push((id, Arc::new(listener)));
        Subscription { queue: self.clone(), id: Some(id) }
    }

    /// Publishes a progress update that was held back by the throttle, if there is one.
    pub fn flush_progress(&self) {
        self.with(|inner, fx, now| {
            if inner.pending_publish {
                inner.publish(now, fx);
            }
        });
    }

    fn pump(&self) {
        self.with(|inner, fx, now| inner.pump(now, fx));
    }

    /// Queues `body` and runs it once it's this task's turn (see the module docs for ordering).
    ///
    /// Not `Send`-bounded itself: the returned future is `Send` exactly when `body`'s future is.
    pub async fn run_task<T, F, Fut>(&self, options: TaskOptions, body: F) -> T
    where
        F: FnOnce(TaskContext) -> Fut,
        Fut: Future<Output = T>,
    {
        let TaskOptions { id, label, priority, cancel, signal } = options;
        if signal.as_ref().is_some_and(AbortSignal::is_aborted) {
            return body(TaskContext { link: None }).await;
        }

        let gate = Gate::new();
        let key = self.with(|inner, _fx, _now| {
            let key = inner.next_key;
            inner.next_key += 1;
            inner.entries.insert(key, Entry { id, label, priority, state: TaskState::Queued, progress: None, cancel, signal: signal.clone(), gate: gate.clone() });
            inner.waiting.push(key);
            key
        });
        let guard = EntryGuard { queue: self.clone(), key };
        if let Some(signal) = &signal {
            let queue = self.clone();
            signal.on_abort(move || queue.dequeue_aborted(key));
        }
        self.pump();

        gate.wait().await;
        let output = body(TaskContext { link: Some(Link { queue: self.clone(), key }) }).await;
        drop(guard);
        output
    }

    /// The abort listener of a task that is still waiting for its first turn: pull it out of the queue and let its body
    /// run once, unqueued.
    fn dequeue_aborted(&self, key: u64) {
        self.with(|inner, fx, now| {
            let Some(entry) = inner.entries.get(&key) else { return };
            if entry.state != TaskState::Queued || !inner.waiting.contains(&key) {
                return;
            }
            inner.waiting.retain(|k| *k != key);
            fx.gates.push(entry.gate.clone());
            inner.publish(now, fx);
        });
    }

    /// `run_task` for repeatable passes where only the newest request matters (e.g. the metadata scan every refresh
    /// kicks off): a newer request with the same id supersedes an older one that hasn't started yet - it leaves the
    /// queue immediately and resolves without running. One that's already running is left to finish (but yields at its
    /// next checkpoint to anything waiting, since it has been superseded).
    pub async fn run_latest_task<F, Fut>(&self, options: TaskOptions, body: F)
    where
        F: FnOnce(TaskContext) -> Fut,
        Fut: Future<Output = ()>,
    {
        let controller = AbortHandle::new();
        let id = options.id.clone();
        let (token, previous) = self.with(|inner, _fx, _now| {
            inner.next_token += 1;
            let token = inner.next_token;
            (token, inner.latest_by_id.insert(id.clone(), (token, controller.clone())))
        });
        if let Some((_, previous)) = previous {
            previous.abort(); // outside the lock: its listeners call back into the queue
        }
        let _forget = LatestGuard { queue: self.clone(), id: id.clone(), token };

        let queue = self.clone();
        self.run_task(options.signal(controller.signal()), move |context| async move {
            let current = queue.shared.inner.lock().unwrap().latest_by_id.get(&id).map(|(t, _)| *t);
            if current != Some(token) {
                return;
            }
            body(context).await;
        })
        .await;
    }

    /// `run_latest_task` for passes over a list of items (tracks) that can take on more items while running: a request
    /// that arrives while a pass with the same id and `join_key` is running adds its items to that pass - anything the
    /// pass hasn't seen yet gets processed there - and returns when that pass does, instead of queueing a second full
    /// pass that would redo everything. A request with a different `join_key` (e.g. a lyrics pass whose lyrics folders
    /// changed) can't join, and queues as usual.
    ///
    /// `body` must call [`PassRegistrar::register`] once it starts with the pass's add-items function; that function
    /// returns `false` once the pass has finished, so a request that arrives too late starts a new pass instead. If the
    /// future driving a pass is dropped, joiners return too (the pass is gone).
    pub async fn run_joinable_task<Item, F, Fut>(&self, options: TaskOptions, join_key: impl Into<String>, items: Vec<Item>, body: F)
    where
        Item: Send + 'static,
        F: FnOnce(TaskContext, PassRegistrar<Item>) -> Fut,
        Fut: Future<Output = ()>,
    {
        let join_key = join_key.into();
        // Try to join a running pass. The add-items function is user code, so it is called with the lock released.
        let joinable = self.with(|inner, _fx, _now| {
            inner.running_passes.get(&options.id).filter(|p| p.join_key == join_key).map(|p| (p.add_items.clone(), p.done.clone()))
        });
        if let Some((add_items, done)) = joinable {
            if let Ok(add_items) = add_items.downcast::<Mutex<Box<dyn FnMut(&[Item]) -> bool + Send>>>() {
                let accepted = (add_items.lock().unwrap())(&items);
                if accepted {
                    done.wait().await;
                    return;
                }
            }
        }

        let (pass_id, done) = self.with(|inner, _fx, _now| {
            inner.next_pass += 1;
            (inner.next_pass, Gate::new())
        });
        let _pass_guard = PassGuard { queue: self.clone(), id: options.id.clone(), pass_id, done: done.clone() };
        let registrar = PassRegistrar { queue: self.clone(), id: options.id.clone(), join_key, pass_id, done, _items: std::marker::PhantomData };
        self.run_latest_task(options, move |context| body(context, registrar)).await;
    }
}

/// Removes the entry when the task finishes, fails or its future is dropped - and hands the turn on.
struct EntryGuard {
    queue: TaskQueue,
    key: u64,
}

impl Drop for EntryGuard {
    fn drop(&mut self) {
        self.queue.with(|inner, fx, now| {
            if inner.holder == Some(self.key) {
                inner.holder = None;
            }
            inner.waiting.retain(|k| *k != self.key);
            inner.entries.remove(&self.key);
            inner.pump(now, fx);
        });
    }
}

struct LatestGuard {
    queue: TaskQueue,
    id: String,
    token: u64,
}

impl Drop for LatestGuard {
    fn drop(&mut self) {
        let mut inner = self.queue.shared.inner.lock().unwrap();
        if inner.latest_by_id.get(&self.id).is_some_and(|(t, _)| *t == self.token) {
            inner.latest_by_id.remove(&self.id);
        }
    }
}

/// Ends a pass: forgets it (if it is still the registered one) and releases everyone waiting on it.
struct PassGuard {
    queue: TaskQueue,
    id: String,
    pass_id: u64,
    done: Gate,
}

impl Drop for PassGuard {
    fn drop(&mut self) {
        {
            let mut inner = self.queue.shared.inner.lock().unwrap();
            if inner.running_passes.get(&self.id).is_some_and(|p| p.pass_id == self.pass_id) {
                inner.running_passes.remove(&self.id);
            }
        }
        self.done.open();
    }
}

/// How a joinable pass tells the queue how to add items to it while it runs.
pub struct PassRegistrar<Item> {
    queue: TaskQueue,
    id: String,
    join_key: String,
    pass_id: u64,
    done: Gate,
    _items: std::marker::PhantomData<fn(Item)>,
}

impl<Item: Send + 'static> PassRegistrar<Item> {
    /// `add_items` receives the items of a later request and returns whether the pass took them (`false` once it has
    /// finished, so that request starts a new pass).
    pub fn register(self, add_items: impl FnMut(&[Item]) -> bool + Send + 'static) {
        let add_items: Box<dyn FnMut(&[Item]) -> bool + Send> = Box::new(add_items);
        let pass = Pass { pass_id: self.pass_id, join_key: self.join_key, add_items: Arc::new(Mutex::new(add_items)), done: self.done };
        self.queue.shared.inner.lock().unwrap().running_passes.insert(self.id, pass);
    }
}

#[derive(Clone)]
struct Link {
    queue: TaskQueue,
    key: u64,
}

/// What a running task body gets: cooperative yielding and progress reporting.
#[derive(Clone)]
pub struct TaskContext {
    /// `None` for a body that runs unqueued (its signal had already aborted), where both operations do nothing.
    link: Option<Link>,
}

impl TaskContext {
    /// Resumable (`Visible`/`Background`) tasks: await between units of work - returns immediately unless higher-priority
    /// work is waiting, in which case it pauses until that's done. A no-op for foreground tasks.
    ///
    /// A task whose signal has been aborted (a newer request with the same id superseded it, see
    /// [`TaskQueue::run_latest_task`]) yields to literally anything waiting, not just strictly-higher priority work -
    /// it's been superseded, so it has no more claim to the queue than ordinary background filler work does, regardless
    /// of its own nominal priority. Without this, two same-priority `Visible` passes (e.g. metadata scans for two
    /// playlists opened one after another) never preempt each other.
    pub async fn checkpoint(&self) {
        let Some(link) = &self.link else { return };
        let resumed = link.queue.with(|inner, fx, now| {
            let entry = inner.entries.get(&link.key)?;
            if entry.priority == TaskPriority::Foreground || inner.holder != Some(link.key) {
                return None;
            }
            let aborted = entry.signal.as_ref().is_some_and(AbortSignal::is_aborted);
            let own = if aborted { -1 } else { entry.priority.rank() };
            if !inner.waiting.iter().any(|k| inner.entries[k].priority.rank() > own) {
                return None;
            }
            let priority = entry.priority;

            // Yield: go back to the front of this priority's line (ahead of any same-priority task that hasn't started
            // yet), let the higher-priority work run, and continue once pump() hands the queue back. A superseded task
            // instead goes to the very back of the whole queue, not the front of its own (nominal, no-longer-meaningful)
            // priority tier - otherwise it would immediately jump back in front of the very task whose request
            // superseded it, since pump() itself only ever looks at an entry's own priority, not this demotion.
            inner.holder = None;
            let gate = Gate::new();
            let entry = inner.entries.get_mut(&link.key).expect("checked above");
            entry.state = TaskState::Paused;
            entry.gate = gate.clone();
            if aborted {
                inner.waiting.push(link.key);
            } else {
                let first_same = inner.waiting.iter().position(|k| inner.entries[k].priority == priority).unwrap_or(inner.waiting.len());
                inner.waiting.insert(first_same, link.key);
            }
            inner.pump(now, fx);
            Some(gate)
        });
        if let Some(gate) = resumed {
            gate.wait().await;
        }
    }

    /// Updates this task's displayed progress. Cheap to call per item - publication is throttled.
    pub fn report_progress(&self, progress: TaskProgress) {
        let Some(link) = &self.link else { return };
        link.queue.with(|inner, fx, now| {
            if let Some(entry) = inner.entries.get_mut(&link.key) {
                entry.progress = Some(progress);
            }
            if now.saturating_sub(inner.last_publish_at) >= PROGRESS_THROTTLE_MS {
                inner.publish(now, fx);
            } else {
                inner.pending_publish = true;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::super::test_executor::{Pool, yield_now};
    use super::*;

    fn queue_with_clock() -> (TaskQueue, Arc<AtomicU64>) {
        let clock = Arc::new(AtomicU64::new(1_000));
        let c = clock.clone();
        (TaskQueue::new(move || c.load(Ordering::SeqCst)), clock)
    }

    fn queue() -> TaskQueue {
        queue_with_clock().0
    }

    fn opts(id: &str, priority: TaskPriority) -> TaskOptions {
        TaskOptions::new(id, id.to_uppercase(), priority)
    }

    fn state_of(q: &TaskQueue, id: &str) -> Option<TaskState> {
        q.tasks().iter().find(|t| t.id == id).map(|t| t.state)
    }

    type Log = Rc<RefCell<Vec<&'static str>>>;

    fn log() -> Log {
        Rc::default()
    }

    use TaskPriority::{Background, Foreground, Visible};

    #[test]
    fn runs_foreground_tasks_one_at_a_time_in_request_order() {
        let (q, pool, order, first_gate) = (queue(), Pool::new(), log(), Gate::new());
        let (qa, o, g) = (q.clone(), order.clone(), first_gate.clone());
        pool.spawn(async move {
            qa.run_task(opts("a", Foreground), |_| async move {
                o.borrow_mut().push("a:start");
                g.wait().await;
                o.borrow_mut().push("a:end");
            })
            .await
        });
        let (qb, o) = (q.clone(), order.clone());
        pool.spawn(async move { qb.run_task(opts("b", Foreground), |_| async move { o.borrow_mut().push("b:start") }).await });

        pool.run_until_stalled();
        assert_eq!((state_of(&q, "a"), state_of(&q, "b")), (Some(TaskState::Running), Some(TaskState::Queued)));
        first_gate.open();
        pool.run_until_stalled();
        assert_eq!(*order.borrow(), ["a:start", "a:end", "b:start"]);
        assert!(q.tasks().is_empty());
    }

    #[test]
    fn pauses_a_background_task_at_its_next_checkpoint_while_foreground_work_runs_then_resumes_it() {
        let (q, pool, order) = (queue(), Pool::new(), log());
        let (reached_first, cont) = (Gate::new(), Gate::new());
        let (qm, o, r, c) = (q.clone(), order.clone(), reached_first.clone(), cont.clone());
        pool.spawn(async move {
            qm.run_task(opts("meta", Background), |ctx| async move {
                o.borrow_mut().push("meta:item1");
                r.open();
                c.wait().await;
                ctx.checkpoint().await;
                o.borrow_mut().push("meta:item2");
            })
            .await
        });
        pool.run_until_stalled();
        assert!(reached_first.is_open());

        let (qs, o) = (q.clone(), order.clone());
        pool.spawn(async move { qs.run_task(opts("scan", Foreground), |_| async move { o.borrow_mut().push("scan") }).await });
        pool.run_until_stalled();
        assert_eq!(state_of(&q, "scan"), Some(TaskState::Queued), "doesn't preempt mid-item");
        cont.open();
        pool.run_until_stalled();
        assert_eq!(*order.borrow(), ["meta:item1", "scan", "meta:item2"]);
    }

    #[test]
    fn reports_a_yielded_background_task_as_paused() {
        let (q, pool) = (queue(), Pool::new());
        let (hold_fg, bg_started) = (Gate::new(), Gate::new());
        let (qb, s) = (q.clone(), bg_started.clone());
        pool.spawn(async move {
            qb.run_task(opts("bg", Background), |ctx| async move {
                s.open();
                yield_now().await;
                yield_now().await; // by now the foreground task has been polled into the queue
                ctx.checkpoint().await;
            })
            .await
        });
        pool.step(); // the background task starts; the foreground one has not been requested yet
        assert!(bg_started.is_open());
        let (qf, h) = (q.clone(), hold_fg.clone());
        pool.spawn(async move { qf.run_task(opts("fg", Foreground), |_| async move { h.wait().await }).await });
        pool.run_until_stalled();
        assert_eq!((state_of(&q, "fg"), state_of(&q, "bg")), (Some(TaskState::Running), Some(TaskState::Paused)));
        hold_fg.open();
        pool.run_until_stalled();
        assert!(q.tasks().is_empty());
    }

    #[test]
    fn starts_waiting_foreground_work_before_an_earlier_queued_background_task() {
        let (q, pool, order, holder) = (queue(), Pool::new(), log(), Gate::new());
        let (q1, h) = (q.clone(), holder.clone());
        pool.spawn(async move { q1.run_task(opts("first", Foreground), |_| async move { h.wait().await }).await });
        let (q2, o) = (q.clone(), order.clone());
        pool.spawn(async move { q2.run_task(opts("bg", Background), |_| async move { o.borrow_mut().push("bg") }).await });
        let (q3, o) = (q.clone(), order.clone());
        pool.spawn(async move { q3.run_task(opts("fg", Foreground), |_| async move { o.borrow_mut().push("fg") }).await });
        pool.run_until_stalled();
        holder.open();
        pool.run_until_stalled();
        assert_eq!(*order.borrow(), ["fg", "bg"]);
    }

    #[test]
    fn pulls_a_task_out_of_the_queue_as_soon_as_its_signal_aborts_running_its_body_once_so_it_can_bail() {
        let (q, pool, holder) = (queue(), Pool::new(), Gate::new());
        let (q1, h) = (q.clone(), holder.clone());
        pool.spawn(async move { q1.run_task(opts("first", Foreground), |_| async move { h.wait().await }).await });
        let controller = AbortHandle::new();
        let (q2, signal) = (q.clone(), controller.signal());
        let result = pool.spawn(async move {
            let s = signal.clone();
            q2.run_task(opts("queued", Foreground).signal(signal), |_| async move { if s.is_aborted() { "cancelled" } else { "ran" } }).await
        });
        pool.run_until_stalled();
        assert_eq!(state_of(&q, "queued"), Some(TaskState::Queued));

        controller.abort();
        pool.run_until_stalled();
        assert_eq!(result.take(), Some("cancelled"));
        assert_eq!(state_of(&q, "queued"), None);
        holder.open();
        pool.run_until_stalled();
        assert!(q.tasks().is_empty());
    }

    #[test]
    fn a_task_whose_signal_is_already_aborted_runs_unqueued_right_away() {
        let (q, pool) = (queue(), Pool::new());
        let controller = AbortHandle::new();
        controller.abort();
        let q2 = q.clone();
        let ran = pool.spawn(async move { q2.run_task(opts("x", Foreground).signal(controller.signal()), |ctx| async move { ctx.checkpoint().await; ctx.report_progress(TaskProgress::default()); 7 }).await });
        pool.run_until_stalled();
        assert_eq!(ran.take(), Some(7));
        assert!(q.tasks().is_empty());
    }

    #[test]
    fn exposes_reported_progress_on_the_task() {
        let (q, pool, holder) = (queue(), Pool::new(), Gate::new());
        let (q1, h) = (q.clone(), holder.clone());
        pool.spawn(async move {
            q1.run_task(opts("p", Foreground), |ctx| async move {
                ctx.report_progress(TaskProgress { detail: "working".into(), current: 3, total: 10 });
                h.wait().await;
            })
            .await
        });
        pool.run_until_stalled();
        // The report landed inside the throttle window of the start-of-task publish, so it is held (the TypeScript's
        // timer would publish it ~250 ms later) until a flush.
        q.flush_progress();
        assert_eq!(q.tasks()[0].progress, Some(TaskProgress { detail: "working".into(), current: 3, total: 10 }));
        holder.open();
        pool.run_until_stalled();
    }

    #[test]
    fn progress_is_throttled_but_a_held_update_is_published_by_flush_or_the_next_state_change() {
        let (q, clock) = queue_with_clock();
        let (pool, ctx_slot, holder) = (Pool::new(), Rc::new(RefCell::new(None::<TaskContext>)), Gate::new());
        let publishes = Arc::new(AtomicU64::new(0));
        let p = publishes.clone();
        let _sub = q.subscribe(move || { let _ = p.fetch_add(1, Ordering::SeqCst); });
        let (q1, slot, h) = (q.clone(), ctx_slot.clone(), holder.clone());
        pool.spawn(async move {
            q1.run_task(opts("t", Foreground), |ctx| async move {
                *slot.borrow_mut() = Some(ctx);
                h.wait().await;
            })
            .await
        });
        pool.run_until_stalled();
        let ctx = ctx_slot.borrow().clone().unwrap();
        let progress = |n: u64| TaskProgress { detail: String::new(), current: n, total: 100 };
        let current = |q: &TaskQueue| q.tasks()[0].progress.as_ref().map(|p| p.current);

        clock.fetch_add(300, Ordering::SeqCst);
        ctx.report_progress(progress(1)); // past the window: published at once
        assert_eq!(current(&q), Some(1));
        clock.fetch_add(100, Ordering::SeqCst);
        ctx.report_progress(progress(2)); // inside the window: held
        ctx.report_progress(progress(3));
        assert_eq!(current(&q), Some(1));
        q.flush_progress(); // still inside the window, but flush publishes what is held
        assert_eq!(current(&q), Some(3));
        let before = publishes.load(Ordering::SeqCst);
        q.flush_progress(); // nothing held: no publish
        assert_eq!(publishes.load(Ordering::SeqCst), before);

        ctx.report_progress(progress(4)); // held again...
        holder.open();
        pool.run_until_stalled(); // ...and the task finishing is a state change that publishes
        assert!(q.tasks().is_empty());
    }

    #[test]
    fn subscribers_hear_about_changes_until_they_unsubscribe() {
        let (q, pool) = (queue(), Pool::new());
        let heard = Arc::new(AtomicU64::new(0));
        let h = heard.clone();
        let sub = q.subscribe(move || { let _ = h.fetch_add(1, Ordering::SeqCst); });
        let q1 = q.clone();
        pool.spawn(async move { q1.run_task(opts("a", Foreground), |_| async {}).await });
        pool.run_until_stalled();
        let after_first = heard.load(Ordering::SeqCst);
        assert!(after_first >= 2, "queued -> running, then finished: {after_first}");
        drop(sub);
        let q2 = q.clone();
        pool.spawn(async move { q2.run_task(opts("b", Foreground), |_| async {}).await });
        pool.run_until_stalled();
        assert_eq!(heard.load(Ordering::SeqCst), after_first);
    }

    #[test]
    fn run_latest_task_a_newer_request_drops_an_older_one_still_waiting_but_not_one_already_running() {
        let (q, pool, ran, holder, running_gate) = (queue(), Pool::new(), log(), Gate::new(), Gate::new());
        let (qb, h) = (q.clone(), holder.clone());
        pool.spawn(async move { qb.run_task(opts("blocker", Foreground), |_| async move { h.wait().await }).await });
        let (qo, r) = (q.clone(), ran.clone());
        pool.spawn(async move { qo.run_latest_task(opts("meta", Background), |_| async move { r.borrow_mut().push("older") }).await });
        let (qn, r, g) = (q.clone(), ran.clone(), running_gate.clone());
        pool.spawn(async move {
            qn.run_latest_task(opts("meta", Background), |_| async move {
                r.borrow_mut().push("newer");
                g.wait().await;
            })
            .await
        });
        pool.run_until_stalled();
        assert_eq!(q.tasks().iter().filter(|t| t.id == "meta").count(), 1, "the older one left the queue right away");

        holder.open();
        pool.run_until_stalled();
        assert_eq!(*ran.borrow(), ["newer"], "the older one resolved without running");
        // A request arriving while 'newer' runs doesn't stop it - it just queues.
        let (qw, r) = (q.clone(), ran.clone());
        pool.spawn(async move { qw.run_latest_task(opts("meta", Background), |_| async move { r.borrow_mut().push("newest") }).await });
        pool.run_until_stalled();
        running_gate.open();
        pool.run_until_stalled();
        assert_eq!(*ran.borrow(), ["newer", "newest"]);
    }

    #[test]
    fn run_latest_task_a_superseded_running_task_yields_at_its_next_checkpoint_even_to_an_equal_priority_request() {
        let (q, pool, order, own_checkpoint, newer_gate) = (queue(), Pool::new(), log(), Gate::new(), Gate::new());
        let (qo, o, g) = (q.clone(), order.clone(), own_checkpoint.clone());
        pool.spawn(async move {
            qo.run_latest_task(opts("meta", Visible), |ctx| async move {
                o.borrow_mut().push("older:1");
                g.wait().await;
                ctx.checkpoint().await;
                o.borrow_mut().push("older:2");
            })
            .await
        });
        pool.run_until_stalled();
        assert_eq!(state_of(&q, "meta"), Some(TaskState::Running));

        let (qn, o, g) = (q.clone(), order.clone(), newer_gate.clone());
        pool.spawn(async move {
            qn.run_latest_task(opts("meta", Visible), |_| async move {
                o.borrow_mut().push("newer:1");
                g.wait().await;
            })
            .await
        });
        pool.run_until_stalled(); // 'newer' is requested: it supersedes 'older' and queues behind it
        own_checkpoint.open();
        pool.run_until_stalled();
        assert_eq!(*order.borrow(), ["older:1", "newer:1"]);
        assert_eq!(state_of(&q, "meta"), Some(TaskState::Running), "'newer' holds the queue now");
        assert_eq!(q.tasks().iter().filter(|t| t.id == "meta").count(), 2, "'older', paused, is still tracked");

        newer_gate.open();
        pool.run_until_stalled();
        assert_eq!(*order.borrow(), ["older:1", "newer:1", "older:2"]);
    }

    #[test]
    fn ranks_foreground_over_visible_over_background_each_pausing_at_a_checkpoint_for_anything_above_it() {
        let (q, pool, order) = (queue(), Pool::new(), log());
        let (bg_started, bg_continue, vis_started, vis_continue) = (Gate::new(), Gate::new(), Gate::new(), Gate::new());
        let (qb, o, s, c) = (q.clone(), order.clone(), bg_started.clone(), bg_continue.clone());
        pool.spawn(async move {
            qb.run_task(opts("library-meta", Background), |ctx| async move {
                o.borrow_mut().push("bg:1");
                s.open();
                c.wait().await;
                ctx.checkpoint().await;
                o.borrow_mut().push("bg:2");
            })
            .await
        });
        pool.run_until_stalled();

        let (qv, o, s, c) = (q.clone(), order.clone(), vis_started.clone(), vis_continue.clone());
        pool.spawn(async move {
            qv.run_task(opts("on-screen-meta", Visible), |ctx| async move {
                o.borrow_mut().push("vis:1");
                s.open();
                c.wait().await;
                ctx.checkpoint().await;
                o.borrow_mut().push("vis:2");
            })
            .await
        });
        pool.run_until_stalled(); // 'visible' is now queued behind the running background task
        assert_eq!(state_of(&q, "on-screen-meta"), Some(TaskState::Queued));
        bg_continue.open(); // background reaches its checkpoint and yields to 'visible'
        pool.run_until_stalled();
        assert!(vis_started.is_open());
        assert_eq!(state_of(&q, "library-meta"), Some(TaskState::Paused));

        let (qf, o) = (q.clone(), order.clone());
        pool.spawn(async move { qf.run_task(opts("scan", Foreground), |_| async move { o.borrow_mut().push("fg") }).await });
        pool.run_until_stalled(); // 'scan' is queued behind the running 'visible' task
        vis_continue.open(); // visible yields to foreground, then resumes before background does
        pool.run_until_stalled();
        assert_eq!(*order.borrow(), ["bg:1", "vis:1", "fg", "vis:2", "bg:2"]);
    }

    #[test]
    fn dropping_a_running_task_future_frees_the_queue_for_the_next_one() {
        let (q, pool, order, hold) = (queue(), Pool::new(), log(), Gate::new());
        let (q1, h) = (q.clone(), hold.clone());
        let first = pool.spawn(async move { q1.run_task(opts("first", Foreground), |_| async move { h.wait().await }).await });
        let (q2, o) = (q.clone(), order.clone());
        pool.spawn(async move { q2.run_task(opts("second", Foreground), |_| async move { o.borrow_mut().push("second") }).await });
        pool.run_until_stalled();
        assert!(order.borrow().is_empty());
        first.cancel(); // the caller gives up on the first task
        pool.run_until_stalled();
        assert_eq!(*order.borrow(), ["second"]);
        assert!(q.tasks().is_empty());
    }

    #[test]
    fn dropping_a_queued_task_future_removes_it_from_the_queue() {
        let (q, pool, hold) = (queue(), Pool::new(), Gate::new());
        let (q1, h) = (q.clone(), hold.clone());
        pool.spawn(async move { q1.run_task(opts("first", Foreground), |_| async move { h.wait().await }).await });
        let q2 = q.clone();
        let queued = pool.spawn(async move { q2.run_task(opts("queued", Foreground), |_| async {}).await });
        pool.run_until_stalled();
        assert_eq!(state_of(&q, "queued"), Some(TaskState::Queued));
        queued.cancel();
        assert_eq!(state_of(&q, "queued"), None);
        hold.open();
        pool.run_until_stalled();
    }

    #[test]
    fn the_cancel_callback_is_exposed_on_the_task() {
        let (q, pool, hold) = (queue(), Pool::new(), Gate::new());
        let cancelled = Arc::new(AtomicU64::new(0));
        let c = cancelled.clone();
        let (q1, h) = (q.clone(), hold.clone());
        pool.spawn(async move { q1.run_task(opts("c", Foreground).cancel(move || { let _ = c.fetch_add(1, Ordering::SeqCst); }), |_| async move { h.wait().await }).await });
        pool.run_until_stalled();
        (q.tasks()[0].cancel.as_ref().expect("cancellable"))();
        assert_eq!(cancelled.load(Ordering::SeqCst), 1);
        hold.open();
        pool.run_until_stalled();
    }

    mod joinable {
        use super::*;

        /// A minimal extendable pass over string items, like the metadata/lyrics scans: processes items one at a time,
        /// accepts new ones while running, and records what it processed.
        fn start_pass(q: &TaskQueue, pool: &Pool, items: &[&str], processed: &Rc<RefCell<Vec<String>>>, join_key: &str, hold: Option<Gate>) -> super::super::super::test_executor::Handle<()> {
            let (q, processed) = (q.clone(), processed.clone());
            let (items, join_key): (Vec<String>, String) = (items.iter().map(|s| s.to_string()).collect(), join_key.to_string());
            pool.spawn(async move {
                let first = items.clone();
                q.run_joinable_task(opts("pass", Background), join_key, items, |_ctx, registrar| async move {
                    let remaining = Arc::new(Mutex::new(first.clone()));
                    let seen = Arc::new(Mutex::new(first.into_iter().collect::<std::collections::HashSet<_>>()));
                    let finished = Arc::new(Mutex::new(false));
                    let (r, s, f) = (remaining.clone(), seen.clone(), finished.clone());
                    registrar.register(move |more: &[String]| {
                        if *f.lock().unwrap() {
                            return false;
                        }
                        for item in more {
                            if s.lock().unwrap().insert(item.clone()) {
                                r.lock().unwrap().push(item.clone());
                            }
                        }
                        true
                    });
                    while !remaining.lock().unwrap().is_empty() {
                        if let Some(h) = &hold {
                            h.wait().await;
                        }
                        let next = remaining.lock().unwrap().remove(0);
                        processed.borrow_mut().push(next);
                        yield_now().await;
                    }
                    *finished.lock().unwrap() = true;
                })
                .await
            })
        }

        #[test]
        fn adds_the_new_items_of_a_mid_pass_request_to_the_running_pass_instead_of_queueing_a_second_full_pass() {
            let (q, pool, processed, hold) = (queue(), Pool::new(), Rc::new(RefCell::new(Vec::new())), Gate::new());
            let first = start_pass(&q, &pool, &["a", "b"], &processed, "", Some(hold.clone()));
            pool.run_until_stalled();
            assert_eq!(state_of(&q, "pass"), Some(TaskState::Running));

            let second = start_pass(&q, &pool, &["a", "b", "c"], &processed, "", None);
            pool.run_until_stalled();
            assert_eq!(q.tasks().iter().filter(|t| t.id == "pass").count(), 1, "joined - one pass");
            assert!(!second.is_done() && !first.is_done());

            hold.open();
            pool.run_until_stalled();
            assert!(first.is_done() && second.is_done(), "the joiner returns when the pass does");
            assert_eq!(*processed.borrow(), ["a", "b", "c"], "nothing processed twice");
        }

        #[test]
        fn queues_a_separate_pass_for_a_request_with_a_different_join_key() {
            let (q, pool, processed, hold) = (queue(), Pool::new(), Rc::new(RefCell::new(Vec::new())), Gate::new());
            start_pass(&q, &pool, &["a"], &processed, "folders-v1", Some(hold.clone()));
            pool.run_until_stalled();
            assert_eq!(state_of(&q, "pass"), Some(TaskState::Running));

            start_pass(&q, &pool, &["a", "b"], &processed, "folders-v2", None);
            hold.open();
            pool.run_until_stalled();
            assert_eq!(*processed.borrow(), ["a", "a", "b"]);
        }

        #[test]
        fn starts_a_new_pass_for_a_request_that_arrives_after_the_running_one_finished() {
            let (q, pool, processed) = (queue(), Pool::new(), Rc::new(RefCell::new(Vec::new())));
            start_pass(&q, &pool, &["a"], &processed, "", None);
            pool.run_until_stalled();
            start_pass(&q, &pool, &["a", "b"], &processed, "", None);
            pool.run_until_stalled();
            assert_eq!(*processed.borrow(), ["a", "a", "b"]);
        }

        #[test]
        fn joiners_are_released_if_the_future_driving_the_pass_is_dropped() {
            let (q, pool, processed, hold) = (queue(), Pool::new(), Rc::new(RefCell::new(Vec::new())), Gate::new());
            let first = start_pass(&q, &pool, &["a"], &processed, "", Some(hold));
            pool.run_until_stalled();
            let second = start_pass(&q, &pool, &["b"], &processed, "", None);
            pool.run_until_stalled();
            assert!(!second.is_done());
            first.cancel();
            pool.run_until_stalled();
            assert!(second.is_done());
            assert!(q.tasks().is_empty());
        }
    }
}
