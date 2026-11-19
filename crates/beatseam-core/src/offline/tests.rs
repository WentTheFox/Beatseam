use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use super::*;
use crate::library_store::{LibraryStore, MemoryLibraryStore};
use crate::metadata::TrackMetadata;
use crate::tasks::Gate;
use crate::tasks::test_executor::Pool;

fn sha(c: char) -> String {
    c.to_string().repeat(64)
}

// ---- harness ----

#[derive(Clone)]
struct TestRuntime {
    pool: Rc<Pool>,
    clock: Rc<Cell<u64>>,
    sleeps: Rc<RefCell<Vec<(u64, Gate)>>>,
}

impl OfflineRuntime for TestRuntime {
    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()>>>) {
        self.pool.spawn(future);
    }
    fn sleep(&self, millis: u64) -> Pin<Box<dyn Future<Output = ()>>> {
        let gate = Gate::new();
        self.sleeps.borrow_mut().push((millis, gate.clone()));
        Box::pin(async move { gate.wait().await })
    }
    fn now_ms(&self) -> u64 {
        self.clock.get()
    }
}

#[derive(Default)]
struct BackendState {
    files: RefCell<HashMap<String, Vec<u8>>>,
    /// Scripted outcomes per file id, consumed front first; an empty script means success (100 bytes, sha `a`).
    plan: RefCell<HashMap<String, VecDeque<Result<Downloaded, DownloadError>>>>,
    hold: RefCell<Option<Gate>>,
    started: RefCell<Vec<String>>,
    in_flight: Cell<usize>,
    max_in_flight: Cell<usize>,
    removed: RefCell<Vec<String>>,
}

#[derive(Clone, Default)]
struct FakeBackend(Rc<BackendState>);

impl OfflineBackend for FakeBackend {
    async fn download(&self, request: DownloadRequest<'_>) -> Result<Downloaded, DownloadError> {
        let s = &self.0;
        s.started.borrow_mut().push(request.file_id.to_owned());
        s.in_flight.set(s.in_flight.get() + 1);
        s.max_in_flight.set(s.max_in_flight.get().max(s.in_flight.get()));
        let hold = s.hold.borrow().clone();
        if let Some(gate) = hold {
            gate.wait().await;
        }
        (request.on_progress)(10);
        s.in_flight.set(s.in_flight.get() - 1);
        if request.signal.is_aborted() {
            return Err(DownloadError::Other("aborted".into()));
        }
        let scripted = s.plan.borrow_mut().get_mut(request.file_id).and_then(VecDeque::pop_front);
        let result = scripted.unwrap_or_else(|| Ok(Downloaded { size_bytes: 100, sha256: sha('a') }));
        if result.is_ok() {
            s.files.borrow_mut().insert(request.file_id.to_owned(), b"audio".to_vec());
        }
        result
    }
    async fn read(&self, file_id: &str) -> Result<Option<Vec<u8>>, BackendError> {
        Ok(self.0.files.borrow().get(file_id).cloned())
    }
    async fn remove(&self, file_id: &str) -> Result<(), BackendError> {
        self.0.removed.borrow_mut().push(file_id.to_owned());
        self.0.files.borrow_mut().remove(file_id);
        Ok(())
    }
    async fn list(&self) -> Result<Vec<(String, u64)>, BackendError> {
        Ok(self.0.files.borrow().keys().map(|k| (k.clone(), 100)).collect())
    }
}

struct Signed(bool);

impl OfflineSource for Signed {
    fn download_target(&self, file_id: &str) -> Option<DownloadTarget> {
        self.0.then(|| DownloadTarget { url: format!("https://server/stream/{file_id}"), headers: vec![("Authorization".into(), "Bearer t".into())] })
    }
}

type Library = OfflineLibrary<FakeBackend, MemoryLibraryStore, Signed, TestRuntime>;

struct Fixture {
    pool: Rc<Pool>,
    runtime: TestRuntime,
    backend: FakeBackend,
    store: Arc<MemoryLibraryStore>,
    library: Library,
}

fn fast_options() -> OfflineOptions {
    OfflineOptions { retry_delay_ms: Box::new(|attempt| 10 * u64::from(attempt)), ..Default::default() }
}

fn fixture_with(backend: FakeBackend, store: Arc<MemoryLibraryStore>, options: OfflineOptions) -> Fixture {
    let pool = Rc::new(Pool::new());
    let runtime = TestRuntime { pool: pool.clone(), clock: Rc::new(Cell::new(1_000)), sleeps: Rc::default() };
    let opened = pool.spawn({
        let (b, s, r) = (backend.clone(), store.clone(), runtime.clone());
        async move { OfflineLibrary::open(b, s, Signed(true), r, options).await }
    });
    pool.run_until_stalled();
    let library = opened.take().expect("opened").expect("store readable");
    Fixture { pool, runtime, backend, store, library }
}

fn fixture() -> Fixture {
    fixture_with(FakeBackend::default(), Arc::new(MemoryLibraryStore::new()), fast_options())
}

impl Fixture {
    fn settle(&self) {
        self.pool.run_until_stalled();
    }

    fn run<T: 'static>(&self, future: impl Future<Output = T> + 'static) -> T {
        let handle = self.pool.spawn(future);
        self.settle();
        handle.take().expect("the future finished")
    }

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    /// Wakes the retry timer that has been waiting the longest.
    fn fire_next_timer(&self) -> u64 {
        let (millis, gate) = self.runtime.sleeps.borrow_mut().remove(0);
        gate.open();
        self.settle();
        millis
    }

    fn metadata_with_hash(&self, file_id: &str, hash: &str) {
        let metadata = TrackMetadata { file_id: file_id.into(), title: None, artists: vec![], album: None, duration_seconds: None, size_bytes: 1, last_modified_ms: 1.0, parser_version: 1, content_hash: Some(hash.into()) };
        self.run({
            let store = self.store.clone();
            async move { store.put_metadata(metadata).await.unwrap() }
        });
    }
}

// ---- tests ----

#[test]
fn downloads_a_couple_at_a_time_and_keeps_requested_tracks_pinned() {
    let f = fixture();
    *f.backend.0.hold.borrow_mut() = Some(Gate::new());
    f.library.request(&Fixture::ids(&["a", "b", "c"]), true);
    f.settle();
    let state = f.library.get_state();
    assert_eq!((state.active_count, state.queued_count, state.requested_count), (2, 1, 3));
    assert_eq!(f.backend.0.started.borrow().len(), 2, "only the concurrency limit starts");

    f.backend.0.hold.borrow().clone().unwrap().open();
    f.settle();
    let state = f.library.get_state();
    assert_eq!((state.downloaded_count, state.downloaded_bytes, state.active_count, state.queued_count), (3, 300, 0, 0));
    assert_eq!(f.backend.0.max_in_flight.get(), 2);
    assert!(["a", "b", "c"].iter().all(|id| f.library.is_available(id) && f.library.is_pinned(id)));
    assert!(state.progress.is_empty());
}

#[test]
fn the_index_is_saved_in_the_typescripts_json_shape_and_survives_a_reopen() {
    let f = fixture();
    f.library.request(&Fixture::ids(&["a"]), true);
    f.settle();
    let saved = f.run({
        let s = f.store.clone();
        async move { s.get_setting("offline.index").await.unwrap() }
    });
    let expected = format!(r#"[{{"fileId":"a","sizeBytes":100,"sha256":"{}","downloadedAtMs":1000,"lastUsedAtMs":1000,"pinned":true}}]"#, sha('a'));
    assert_eq!(saved.as_deref(), Some(expected.as_str()));

    let reopened = fixture_with(f.backend.clone(), f.store.clone(), fast_options());
    assert!(reopened.library.is_available("a") && reopened.library.is_pinned("a"));
    assert_eq!(decode_offline_index("not json"), vec![]);
}

#[test]
fn a_hash_mismatch_is_retried_with_backoff_then_reported_without_keeping_the_file() {
    let f = fixture();
    f.metadata_with_hash("a", &sha('b')); // the server says b; the backend delivers a
    f.library.request(&Fixture::ids(&["a"]), true);
    f.settle();
    assert_eq!(f.fire_next_timer(), 10, "first retry delay");
    assert_eq!(f.fire_next_timer(), 20, "second retry delay");
    let state = f.library.get_state();
    assert_eq!(state.failures, [OfflineFailure { file_id: "a".into(), message: "The downloaded file does not match what the server has".into() }]);
    assert_eq!(f.backend.0.started.borrow().len(), 3, "max_attempts tries");
    assert!(!f.library.is_available("a") && f.backend.0.files.borrow().is_empty());
}

#[test]
fn a_matching_hash_is_accepted() {
    let f = fixture();
    f.metadata_with_hash("a", &sha('a'));
    f.library.request(&Fixture::ids(&["a"]), true);
    f.settle();
    assert!(f.library.is_available("a"));
}

#[test]
fn losing_the_network_pauses_and_keeps_its_place_until_the_server_is_back() {
    let f = fixture();
    f.backend.0.plan.borrow_mut().insert("a".into(), VecDeque::from([Err(DownloadError::Network)]));
    f.library.request(&Fixture::ids(&["a", "b"]), true);
    f.settle();
    let state = f.library.get_state();
    assert!(state.paused);
    assert_eq!(state.failures, vec![], "not the track's fault, so no failure is recorded");
    assert!(f.library.is_available("b"), "the other slot finished before the pause took effect");

    // Offline -> still offline: nothing. Back online: resumes with `a` first in line.
    f.library.on_server_status(ServerStatusSummary { offline: true, active: true, last_synced_at_ms: Some(1) });
    assert!(f.library.get_state().paused);
    f.library.on_server_status(ServerStatusSummary { offline: false, active: true, last_synced_at_ms: Some(2) });
    f.settle();
    assert!(!f.library.get_state().paused);
    assert!(f.library.is_available("a"));
}

#[test]
fn a_lapsed_sign_in_pauses_and_says_so_while_a_missing_track_is_just_reported() {
    let f = fixture();
    f.backend.0.plan.borrow_mut().insert("a".into(), VecDeque::from([Err(DownloadError::Unauthorized)]));
    f.library.request(&Fixture::ids(&["a"]), true);
    f.settle();
    let state = f.library.get_state();
    assert!(state.paused);
    assert_eq!(state.failures[0].message, "Signed out by the server");

    let g = fixture();
    g.backend.0.plan.borrow_mut().insert("gone".into(), VecDeque::from([Err(DownloadError::NotFound)]));
    g.library.request(&Fixture::ids(&["gone", "ok"]), true);
    g.settle();
    let state = g.library.get_state();
    assert!(!state.paused);
    assert_eq!(state.failures, [OfflineFailure { file_id: "gone".into(), message: "No longer on the server".into() }]);
    assert!(g.library.is_available("ok"));
}

#[test]
fn not_being_signed_in_counts_as_the_network_being_away() {
    let pool = Rc::new(Pool::new());
    let runtime = TestRuntime { pool: pool.clone(), clock: Rc::new(Cell::new(0)), sleeps: Rc::default() };
    let opened = pool.spawn({
        let (r, s) = (runtime, Arc::new(MemoryLibraryStore::new()));
        async move { OfflineLibrary::open(FakeBackend::default(), s, Signed(false), r, fast_options()).await }
    });
    pool.run_until_stalled();
    let library = opened.take().unwrap().unwrap();
    library.request(&Fixture::ids(&["a"]), true);
    pool.run_until_stalled();
    assert!(library.get_state().paused);
    assert_eq!(library.get_state().queued_count, 1);
}

#[test]
fn the_budget_evicts_the_least_recently_played_unpinned_copies_but_never_pinned_ones() {
    let f = fixture();
    f.library.request(&Fixture::ids(&["old", "mid"]), false);
    f.settle();
    f.runtime.clock.set(2_000);
    f.library.request(&Fixture::ids(&["pinned"]), true);
    f.settle();
    f.runtime.clock.set(3_000);
    f.library.request(&Fixture::ids(&["new"]), false);
    f.settle();
    // Play "old" so "mid" becomes the least recently used.
    f.runtime.clock.set(4_000);
    assert!(f.run({ let l = f.library.clone(); async move { l.read("old").await.unwrap() } }).is_some());

    f.run({ let l = f.library.clone(); async move { l.set_budget_bytes(250).await.unwrap() } });
    assert!(!f.library.is_available("mid"), "least recently played goes first");
    assert!(!f.library.is_available("new"), "still over budget: the next least recently used");
    assert!(f.library.is_available("old") && f.library.is_available("pinned"));
    assert_eq!(f.library.get_state().budget_bytes, 250);
    let stored = f.run({ let s = f.store.clone(); async move { s.get_setting("offline.budgetBytes").await.unwrap() } });
    assert_eq!(stored.as_deref(), Some("250"));
}

#[test]
fn a_fresh_download_is_protected_from_its_own_eviction() {
    let f = fixture();
    f.run({ let l = f.library.clone(); async move { l.set_budget_bytes(50).await.unwrap() } });
    f.library.request(&Fixture::ids(&["a"]), false);
    f.settle();
    assert!(f.library.is_available("a"), "100 bytes is over a 50-byte budget, but it was just fetched");
    f.library.request(&Fixture::ids(&["b"]), false);
    f.settle();
    assert!(f.library.is_available("b") && !f.library.is_available("a"));
}

#[test]
fn cache_on_play_keeps_the_track_unpinned_and_concurrent_callers_share_one_transfer() {
    let f = fixture();
    assert!(!f.run({ let l = f.library.clone(); async move { l.fetch_for_playback("a").await } }), "off by default");
    f.run({ let l = f.library.clone(); async move { l.set_cache_on_play(true).await.unwrap() } });
    assert!(f.library.get_state().cache_on_play);

    *f.backend.0.hold.borrow_mut() = Some(Gate::new());
    let first = f.pool.spawn({ let l = f.library.clone(); async move { l.fetch_for_playback("a").await } });
    let second = f.pool.spawn({ let l = f.library.clone(); async move { l.fetch_for_playback("a").await } });
    f.settle();
    assert_eq!(f.backend.0.started.borrow().len(), 1, "one transfer for both readers");
    f.backend.0.hold.borrow().clone().unwrap().open();
    f.settle();
    assert_eq!((first.take(), second.take()), (Some(true), Some(true)));
    assert!(f.library.is_available("a") && !f.library.is_pinned("a"));
    assert!(f.run({ let l = f.library.clone(); async move { l.fetch_for_playback("a").await } }), "already stored");
}

#[test]
fn a_failed_play_fetch_leaves_no_trace() {
    let f = fixture();
    f.run({ let l = f.library.clone(); async move { l.set_cache_on_play(true).await.unwrap() } });
    f.backend.0.plan.borrow_mut().insert("a".into(), VecDeque::from([Err(DownloadError::Network)]));
    assert!(!f.run({ let l = f.library.clone(); async move { l.fetch_for_playback("a").await } }));
    let state = f.library.get_state();
    assert_eq!((state.paused, state.failures.len(), state.active_count, state.queued_count), (false, 0, 0, 0));
}

#[test]
fn a_play_fetch_leaves_a_track_that_is_already_being_downloaded_alone() {
    let f = fixture();
    f.run({ let l = f.library.clone(); async move { l.set_cache_on_play(true).await.unwrap() } });
    *f.backend.0.hold.borrow_mut() = Some(Gate::new());
    f.library.request(&Fixture::ids(&["a"]), true);
    f.settle();
    assert!(!f.run({ let l = f.library.clone(); async move { l.fetch_for_playback("a").await } }));
    assert_eq!(f.backend.0.started.borrow().len(), 1);
}

#[test]
fn a_copy_the_server_has_since_replaced_is_dropped_and_fetched_again_when_it_was_pinned() {
    let f = fixture();
    f.library.request(&Fixture::ids(&["keep"]), true);
    f.library.request(&Fixture::ids(&["cache"]), false);
    f.settle();
    // The server's hash for both changes.
    f.metadata_with_hash("keep", &sha('c'));
    f.metadata_with_hash("cache", &sha('c'));
    assert_eq!(f.run({ let l = f.library.clone(); async move { l.read("keep").await.unwrap() } }), None);
    assert_eq!(f.run({ let l = f.library.clone(); async move { l.read("cache").await.unwrap() } }), None);
    // Both were forgotten; only the pinned one is requested again (and it fails verification the same way, which is fine).
    let started = f.backend.0.started.borrow().clone();
    assert_eq!(started.iter().filter(|id| *id == "keep").count(), 2);
    assert_eq!(started.iter().filter(|id| *id == "cache").count(), 1);
    assert!(!f.library.is_available("cache"));
}

#[test]
fn reading_returns_the_bytes_and_a_file_that_vanished_from_disk_drops_out_of_the_index() {
    let f = fixture();
    f.library.request(&Fixture::ids(&["a"]), true);
    f.settle();
    assert_eq!(f.run({ let l = f.library.clone(); async move { l.read("a").await.unwrap() } }), Some(b"audio".to_vec()));
    assert_eq!(f.run({ let l = f.library.clone(); async move { l.read("never").await.unwrap() } }), None);
    f.backend.0.files.borrow_mut().clear();
    assert_eq!(f.run({ let l = f.library.clone(); async move { l.read("a").await.unwrap() } }), None);
    assert!(!f.library.is_available("a"));
}

#[test]
fn opening_reconciles_the_index_with_the_disk() {
    let f = fixture();
    f.library.request(&Fixture::ids(&["a", "b"]), true);
    f.settle();
    // "a" vanishes from disk, and an interrupted run left "orphan" behind.
    f.backend.0.files.borrow_mut().remove("a");
    f.backend.0.files.borrow_mut().insert("orphan".into(), vec![1]);
    let reopened = fixture_with(f.backend.clone(), f.store.clone(), fast_options());
    assert!(!reopened.library.is_available("a") && reopened.library.is_available("b"));
    assert!(f.backend.0.removed.borrow().contains(&"orphan".to_string()));
    assert!(!f.backend.0.files.borrow().contains_key("orphan"));
}

#[test]
fn cancelling_aborts_a_transfer_in_flight_and_removing_frees_the_space() {
    let f = fixture();
    *f.backend.0.hold.borrow_mut() = Some(Gate::new());
    f.library.request(&Fixture::ids(&["a", "b", "c"]), true);
    f.settle();
    f.library.cancel(&Fixture::ids(&["a", "c"]));
    assert_eq!(f.library.get_state().queued_count, 0, "c left the queue");
    f.backend.0.hold.borrow().clone().unwrap().open();
    f.settle();
    assert!(!f.library.is_available("a") && f.library.get_state().failures.is_empty(), "an aborted transfer is neither stored nor a failure");
    assert!(f.library.is_available("b"));

    f.run({ let l = f.library.clone(); async move { l.remove(&Fixture::ids(&["b"])).await } });
    assert!(!f.library.is_available("b") && f.backend.0.files.borrow().is_empty());
    assert_eq!(f.library.get_state().removing, None);
}

#[test]
fn clear_forgets_everything_and_cancel_all_stops_pending_retries() {
    let f = fixture();
    f.backend.0.plan.borrow_mut().insert("flaky".into(), VecDeque::from([Err(DownloadError::Other("boom".into()))]));
    f.library.request(&Fixture::ids(&["a", "b", "flaky"]), true);
    f.settle();
    assert_eq!(f.library.get_state().downloaded_count, 2);
    assert_eq!(f.runtime.sleeps.borrow().len(), 1, "a retry is waiting");

    f.run({ let l = f.library.clone(); async move { l.clear().await } });
    assert_eq!(f.library.get_state().downloaded_count, 0);
    f.fire_next_timer();
    assert_eq!(f.library.get_state().queued_count, 0, "the retry timer found itself cancelled");
    assert_eq!(f.backend.0.started.borrow().iter().filter(|id| *id == "flaky").count(), 1);
}

#[test]
fn requesting_a_stored_track_only_upgrades_it_to_pinned() {
    let f = fixture();
    f.library.request(&Fixture::ids(&["a"]), false);
    f.settle();
    assert!(!f.library.is_pinned("a"));
    f.library.request(&Fixture::ids(&["a"]), true);
    f.settle();
    assert!(f.library.is_pinned("a"));
    assert_eq!(f.backend.0.started.borrow().len(), 1, "no second download");
}

#[test]
fn subscribers_hear_changes_until_they_unsubscribe() {
    let f = fixture();
    let heard = Rc::new(Cell::new(0));
    let h = heard.clone();
    let id = f.library.subscribe(move |_| h.set(h.get() + 1));
    f.library.request(&Fixture::ids(&["a"]), true);
    f.settle();
    assert!(heard.get() > 0);
    f.library.unsubscribe(id);
    let before = heard.get();
    f.library.pause();
    assert_eq!(heard.get(), before);
}

#[test]
fn index_encoding_round_trips_and_drops_incomplete_entries() {
    let records = vec![OfflineRecord { file_id: "é\"x".into(), size_bytes: 5, sha256: sha('f'), downloaded_at_ms: 7, last_used_at_ms: 9, pinned: false }];
    assert_eq!(decode_offline_index(&encode_offline_index(&records)), records);
    assert_eq!(decode_offline_index(r#"[{"fileId":"x"},{"fileId":"y","sizeBytes":1,"sha256":"s","downloadedAtMs":1,"lastUsedAtMs":1,"pinned":true}]"#).len(), 1);
}
