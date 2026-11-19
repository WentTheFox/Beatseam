use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::error::LibraryError;
use super::scan::{ScanPhase, ScanProgress, ScanResult, scan_root};
use crate::file_access::FileAccess;
use crate::library_store::LibraryStore;
use crate::tasks::{AbortHandle, Gate, TaskOptions, TaskPriority, TaskProgress, TaskQueue};

type Listener = Arc<dyn Fn() + Send + Sync>;
type SharedResult = Arc<Mutex<Option<Result<ScanResult, LibraryError>>>>;

struct ActiveScan {
    abort: AbortHandle,
    done: Gate,
    result: SharedResult,
}

#[derive(Default)]
struct State {
    active: HashMap<String, ActiveScan>,
    listeners: Vec<(u64, Listener)>,
    next_listener: u64,
    /// Root ids currently scanning, in start order; a new `Arc` only when the set actually changes.
    snapshot: Arc<Vec<String>>,
    order: Vec<String>,
}

/// Runs root scans so that a root is never scanned twice at once. State is shared by every clone, deliberately - a root
/// can be scanned by two entirely separate call sites that don't know about each other: an automatic background refresh
/// (which runs on launch/focus with no user gesture) and a user clicking "Rescan". Confirmed live in the TypeScript:
/// without this, both could scan the same root at once, each independently walking the whole tree and upserting into the
/// store - wasted work at best, and a confusing double-cancel/interleaved-progress UI at worst, since neither side has
/// any way to know the other is already scanning.
///
/// A scan joins the future that first asks for it: a later caller for the same root waits for that scan's result (or its
/// cancellation, even if a different caller cancels it) rather than getting its own run. As with every `TaskQueue` task,
/// a scan is "in flight" once its future has been polled, not at the call.
#[derive(Clone)]
pub struct ScanCoordinator {
    queue: TaskQueue,
    state: Arc<Mutex<State>>,
}

impl ScanCoordinator {
    pub fn new(queue: TaskQueue) -> Self {
        Self { queue, state: Arc::new(Mutex::new(State::default())) }
    }

    /// True while any caller's scan of this root (background or user-triggered) is in flight.
    pub fn is_root_scanning(&self, root_id: &str) -> bool {
        self.state.lock().unwrap().active.contains_key(root_id)
    }

    /// Every currently-scanning root id, as a stable reference that only changes when the active set actually does.
    pub fn active_scan_root_ids(&self) -> Arc<Vec<String>> {
        self.state.lock().unwrap().snapshot.clone()
    }

    /// Fires on every scan start/finish (own or another caller's). Dropping the returned [`ScanSubscription`] unsubscribes.
    pub fn subscribe(&self, listener: impl Fn() + Send + Sync + 'static) -> ScanSubscription {
        let mut state = self.state.lock().unwrap();
        state.next_listener += 1;
        let id = state.next_listener;
        state.listeners.push((id, Arc::new(listener)));
        ScanSubscription { coordinator: self.clone(), id }
    }

    /// No-op if the root isn't currently scanning (already finished, or never started) - callers don't need to check
    /// [`is_root_scanning`](Self::is_root_scanning) first.
    pub fn cancel_root_scan(&self, root_id: &str) {
        let abort = self.state.lock().unwrap().active.get(root_id).map(|a| a.abort.clone());
        if let Some(abort) = abort {
            abort.abort(); // outside the lock: aborting runs listeners that call back into the queue
        }
    }

    fn refresh(state: &mut State) -> Vec<Listener> {
        state.snapshot = Arc::new(state.order.clone());
        state.listeners.iter().map(|(_, l)| l.clone()).collect()
    }

    /// Runs `scan_root` for `root_id`, joining an already-in-flight scan of the same root instead of starting a duplicate
    /// one - see the type docs. A caller that joins gets that scan's result (or its [`LibraryError::Cancelled`]).
    ///
    /// The scan itself runs as a foreground task on the shared queue, so it waits its turn behind other folder scans and
    /// pauses background passes (metadata, lyrics) instead of racing them; the queue is also what shows its progress and
    /// a Cancel button. `display_name` labels it there - a freshly added folder's first scan runs before the root shows up
    /// anywhere else its name could be looked up.
    // The lock is released (`drop(state)`) before the await; clippy can't see that through the early return.
    #[allow(clippy::await_holding_lock)]
    pub async fn scan_root_coordinated<F: FileAccess, S: LibraryStore>(&self, file_access: &F, store: &S, root_id: &str, display_name: Option<&str>) -> Result<ScanResult, LibraryError> {
        let (abort, done, result, listeners) = {
            let mut state = self.state.lock().unwrap();
            if let Some(existing) = state.active.get(root_id) {
                let (done, result) = (existing.done.clone(), existing.result.clone());
                drop(state);
                done.wait().await;
                let joined = result.lock().unwrap().clone();
                return joined.unwrap_or(Err(LibraryError::Cancelled));
            }
            let active = ActiveScan { abort: AbortHandle::new(), done: Gate::new(), result: Arc::new(Mutex::new(None)) };
            let handles = (active.abort.clone(), active.done.clone(), active.result.clone());
            state.active.insert(root_id.to_owned(), active);
            state.order.push(root_id.to_owned());
            let listeners = Self::refresh(&mut state);
            (handles.0, handles.1, handles.2, listeners)
        };
        for listener in listeners {
            listener();
        }
        // Whether the scan finishes or this future is dropped, the root stops being "scanning" and joiners are released.
        let _end = ActiveEnd { coordinator: self.clone(), root_id: root_id.to_owned(), done };

        let on_cancel = abort.clone();
        let options = TaskOptions::new(format!("scanning-{root_id}"), format!("Scanning \"{}\"", display_name.unwrap_or(root_id)), TaskPriority::Foreground).cancel(move || on_cancel.abort()).signal(abort.signal());
        let signal = abort.signal();
        let outcome = self
            .queue
            .run_task(options, |context| async move {
                let mut report = |p: ScanProgress| context.report_progress(describe_scan_progress(&p));
                scan_root(file_access, store, root_id, Some(&signal), Some(&mut report)).await
            })
            .await;
        *result.lock().unwrap() = Some(outcome.clone());
        outcome
    }
}

struct ActiveEnd {
    coordinator: ScanCoordinator,
    root_id: String,
    done: Gate,
}

impl Drop for ActiveEnd {
    fn drop(&mut self) {
        let listeners = {
            let mut state = self.coordinator.state.lock().unwrap();
            state.active.remove(&self.root_id);
            state.order.retain(|r| *r != self.root_id);
            ScanCoordinator::refresh(&mut state)
        };
        self.done.open();
        for listener in listeners {
            listener();
        }
    }
}

pub struct ScanSubscription {
    coordinator: ScanCoordinator,
    id: u64,
}

impl Drop for ScanSubscription {
    fn drop(&mut self) {
        self.coordinator.state.lock().unwrap().listeners.retain(|(id, _)| *id != self.id);
    }
}

/// Turns a scan's raw progress into the task queue's display shape.
pub fn describe_scan_progress(progress: &ScanProgress) -> TaskProgress {
    match progress.phase {
        ScanPhase::Listing => {
            let folders = format!("{} folder{}", progress.folders_listed, if progress.folders_listed == 1 { "" } else { "s" });
            TaskProgress { detail: format!("{} files in {folders}", progress.files_found), current: 0, total: 0 }
        }
        ScanPhase::Playlists => TaskProgress { detail: "reading playlists".into(), current: progress.current as u64, total: progress.total as u64 },
        ScanPhase::Saving => TaskProgress { detail: "saving library".into(), current: progress.current as u64, total: progress.total as u64 },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::file_access::{CallOptions, DirectoryEntry, FileAccessError, FileRef, GrantedRoot, MemoryFileAccess, RootKind};
    use crate::library_store::MemoryLibraryStore;
    use crate::tasks::TaskState;
    use crate::tasks::test_executor::Pool;

    /// A file access whose directory listings hang until released, counting how many were started.
    struct Hanging {
        inner: MemoryFileAccess,
        gate: Gate,
        calls: AtomicUsize,
    }

    impl Hanging {
        fn new() -> Arc<Self> {
            Arc::new(Self { inner: MemoryFileAccess::new([("Mix.m3u8", "Track.mp3"), ("Track.mp3", "x")]), gate: Gate::new(), calls: AtomicUsize::new(0) })
        }
    }

    impl FileAccess for Hanging {
        async fn request_root(&self, k: RootKind) -> Result<Option<GrantedRoot>, FileAccessError> {
            self.inner.request_root(k).await
        }
        async fn list_granted_roots(&self) -> Result<Vec<GrantedRoot>, FileAccessError> {
            self.inner.list_granted_roots().await
        }
        async fn revoke_root(&self, r: &str) -> Result<(), FileAccessError> {
            self.inner.revoke_root(r).await
        }
        async fn list_directory(&self, r: &str, p: Option<&str>, o: CallOptions) -> Result<Vec<DirectoryEntry>, FileAccessError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.gate.wait().await;
            self.inner.list_directory(r, p, o).await
        }
        async fn read_file_bytes(&self, f: &FileRef, o: CallOptions) -> Result<Vec<u8>, FileAccessError> {
            self.inner.read_file_bytes(f, o).await
        }
        async fn read_file_text(&self, f: &FileRef, o: CallOptions) -> Result<String, FileAccessError> {
            self.inner.read_file_text(f, o).await
        }
        async fn write_file_text(&self, r: &str, p: &str, c: &str) -> Result<(), FileAccessError> {
            self.inner.write_file_text(r, p, c).await
        }
    }

    fn coordinator() -> ScanCoordinator {
        ScanCoordinator::new(TaskQueue::new(|| 0))
    }

    type Handle = crate::tasks::test_executor::Handle<Result<ScanResult, LibraryError>>;

    fn start(pool: &Pool, c: &ScanCoordinator, fa: &Arc<Hanging>, store: &Arc<MemoryLibraryStore>, root: &'static str) -> Handle {
        let (c, fa, store) = (c.clone(), fa.clone(), store.clone());
        pool.spawn(async move { c.scan_root_coordinated(&*fa, &*store, root, None).await })
    }

    #[test]
    fn joins_an_already_in_flight_scan_of_the_same_root_instead_of_starting_a_second_one() {
        let (pool, c, fa, store) = (Pool::new(), coordinator(), Hanging::new(), Arc::new(MemoryLibraryStore::new()));
        let first = start(&pool, &c, &fa, &store, "root-1");
        let second = start(&pool, &c, &fa, &store, "root-1");
        pool.run_until_stalled();
        assert_eq!(fa.calls.load(Ordering::SeqCst), 1, "only one real walk started, not two");
        assert!(c.is_root_scanning("root-1"));

        fa.gate.open();
        pool.run_until_stalled();
        let (first, second) = (first.take().unwrap().unwrap(), second.take().unwrap().unwrap());
        assert_eq!(first, second, "both callers got the same result");
        assert!(!c.is_root_scanning("root-1"));
        assert_eq!(first.playlists.len(), 1);
    }

    #[test]
    fn queues_a_different_root_scan_behind_the_running_one_instead_of_joining_or_racing_it() {
        let (pool, c, fa, store) = (Pool::new(), coordinator(), Hanging::new(), Arc::new(MemoryLibraryStore::new()));
        let a = start(&pool, &c, &fa, &store, "root-a");
        pool.run_until_stalled();
        let b = start(&pool, &c, &fa, &store, "root-b");
        pool.run_until_stalled();
        assert_eq!(fa.calls.load(Ordering::SeqCst), 1, "root-b waits for root-a's turn");
        assert!(c.is_root_scanning("root-a") && c.is_root_scanning("root-b"));

        fa.gate.open();
        pool.run_until_stalled();
        assert!(a.take().unwrap().is_ok() && b.take().unwrap().is_ok());
        assert_eq!(fa.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn cancelling_a_still_queued_scan_settles_it_right_away_without_waiting_for_the_one_ahead() {
        let (pool, c, fa, store) = (Pool::new(), coordinator(), Hanging::new(), Arc::new(MemoryLibraryStore::new()));
        let a = start(&pool, &c, &fa, &store, "root-a");
        pool.run_until_stalled();
        let b = start(&pool, &c, &fa, &store, "root-b");
        pool.run_until_stalled();
        c.cancel_root_scan("root-b");
        pool.run_until_stalled();
        assert_eq!(b.take(), Some(Err(LibraryError::Cancelled)));
        assert!(!c.is_root_scanning("root-b"));
        assert!(a.take().is_none(), "root-a is still running");
        fa.gate.open();
        pool.run_until_stalled();
    }

    #[test]
    fn cancel_root_scan_aborts_the_in_flight_scan_and_rejects_every_joined_caller() {
        let (pool, c, fa, store) = (Pool::new(), coordinator(), Hanging::new(), Arc::new(MemoryLibraryStore::new()));
        let first = start(&pool, &c, &fa, &store, "root-1");
        let second = start(&pool, &c, &fa, &store, "root-1");
        pool.run_until_stalled();
        c.cancel_root_scan("root-1");
        fa.gate.open(); // the walk notices the abort once its in-flight listing returns
        pool.run_until_stalled();
        assert_eq!(first.take(), Some(Err(LibraryError::Cancelled)));
        assert_eq!(second.take(), Some(Err(LibraryError::Cancelled)));
        assert!(store.snapshot_tracks().is_empty(), "a cancelled scan writes nothing");
    }

    #[test]
    fn cancel_root_scan_is_a_no_op_when_the_root_is_not_scanning() {
        coordinator().cancel_root_scan("nothing");
    }

    #[test]
    fn notifies_subscribers_when_a_scan_starts_and_finishes_and_keeps_a_stable_snapshot() {
        let (pool, c, fa, store) = (Pool::new(), coordinator(), Hanging::new(), Arc::new(MemoryLibraryStore::new()));
        let heard = Arc::new(AtomicUsize::new(0));
        let h = heard.clone();
        let sub = c.subscribe(move || {
            h.fetch_add(1, Ordering::SeqCst);
        });
        let before = c.active_scan_root_ids();
        let run = start(&pool, &c, &fa, &store, "root-1");
        pool.run_until_stalled();
        assert_eq!(heard.load(Ordering::SeqCst), 1);
        let during = c.active_scan_root_ids();
        assert_eq!(*during, ["root-1"]);
        assert!(Arc::ptr_eq(&during, &c.active_scan_root_ids()), "same Arc until the set changes");
        assert!(!Arc::ptr_eq(&before, &during));

        fa.gate.open();
        pool.run_until_stalled();
        assert!(run.take().unwrap().is_ok());
        assert_eq!(heard.load(Ordering::SeqCst), 2);
        assert!(c.active_scan_root_ids().is_empty());
        drop(sub);
        let again = start(&pool, &c, &Hanging::new(), &store, "root-2");
        pool.run_until_stalled();
        assert_eq!(heard.load(Ordering::SeqCst), 2, "unsubscribed");
        drop(again);
    }

    #[test]
    fn runs_as_a_labelled_cancellable_foreground_task_gone_from_the_queue_once_finished() {
        let queue = TaskQueue::new(|| 0);
        let c = ScanCoordinator::new(queue.clone());
        let (pool, fa, store) = (Pool::new(), Hanging::new(), Arc::new(MemoryLibraryStore::new()));
        let (c2, fa2, store2) = (c.clone(), fa.clone(), store.clone());
        let run = pool.spawn(async move { c2.scan_root_coordinated(&*fa2, &*store2, "root-1", Some("My Music")).await });
        pool.run_until_stalled();
        let tasks = queue.tasks();
        assert_eq!(tasks.len(), 1);
        assert_eq!((tasks[0].id.as_str(), tasks[0].label.as_str(), tasks[0].priority, tasks[0].state), ("scanning-root-1", "Scanning \"My Music\"", TaskPriority::Foreground, TaskState::Running));
        assert!(tasks[0].cancel.is_some());

        (tasks[0].cancel.as_ref().unwrap())(); // the Cancel button
        fa.gate.open();
        pool.run_until_stalled();
        assert_eq!(run.take(), Some(Err(LibraryError::Cancelled)));
        assert!(queue.tasks().is_empty());
    }

    #[test]
    fn dropping_the_scanning_future_frees_the_root_and_releases_joiners() {
        let (pool, c, fa, store) = (Pool::new(), coordinator(), Hanging::new(), Arc::new(MemoryLibraryStore::new()));
        let leader = start(&pool, &c, &fa, &store, "root-1");
        let joiner = start(&pool, &c, &fa, &store, "root-1");
        pool.run_until_stalled();
        leader.cancel();
        pool.run_until_stalled();
        assert_eq!(joiner.take(), Some(Err(LibraryError::Cancelled)));
        assert!(!c.is_root_scanning("root-1"));
    }

    #[test]
    fn describes_progress_for_the_task_queue() {
        let p = |phase, folders, files, current, total| describe_scan_progress(&ScanProgress { phase, folders_listed: folders, files_found: files, current, total });
        assert_eq!(p(ScanPhase::Listing, 1, 12, 0, 0), TaskProgress { detail: "12 files in 1 folder".into(), current: 0, total: 0 });
        assert_eq!(p(ScanPhase::Listing, 3, 12, 0, 0).detail, "12 files in 3 folders");
        assert_eq!(p(ScanPhase::Playlists, 3, 12, 2, 5), TaskProgress { detail: "reading playlists".into(), current: 2, total: 5 });
        assert_eq!(p(ScanPhase::Saving, 3, 12, 4, 9), TaskProgress { detail: "saving library".into(), current: 4, total: 9 });
    }
}
