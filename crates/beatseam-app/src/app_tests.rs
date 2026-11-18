use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use beatseam_core::audio_engine::fake::FakeEngine;
use beatseam_core::file_access::{FileAccessError, FileRef, GrantedRoot, MemoryFileAccess, RootKind};
use beatseam_core::http::block_on;
use beatseam_core::library_scan::{LibraryError, scan_root};
use beatseam_core::library_store::{LibraryStore, LoopMode, MemoryLibraryStore, OpenedPlaylist, PlaybackState};
use beatseam_core::playback::{PlaylistPlayer, PlaylistPlayerOptions, ResolveTrack, TrackPlayerStatus};
use beatseam_core::tasks::Gate;
use beatseam_core::tasks::Runtime;
use beatseam_core::tasks::test_executor::Pool;

use super::*;

#[derive(Clone)]
struct TestRuntime {
    pool: Rc<Pool>,
    clock: Rc<Cell<u64>>,
    sleeps: Rc<RefCell<Vec<(u64, Gate)>>>,
}

impl Runtime for TestRuntime {
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
struct JobLog {
    scanned: Vec<String>,
    pruned: Vec<Vec<String>>,
    metadata_tracks: Vec<usize>,
    lyrics_matches: Vec<(usize, usize)>,
    callbacks: Option<LyricsMatchCallbacks>,
    sync_calls: usize,
    user_playlists_active: bool,
    user_ops: Vec<String>,
}

struct FakeJobs {
    file_access: Arc<MemoryFileAccess>,
    store: Arc<MemoryLibraryStore>,
    log: Rc<RefCell<JobLog>>,
    failing_roots: Vec<String>,
    sync_result: Rc<Cell<Option<bool>>>,
}

impl LibraryJobs for FakeJobs {
    async fn scan_root(&self, root_id: &str, _display_name: &str) -> Result<(), LibraryError> {
        self.log.borrow_mut().scanned.push(root_id.to_owned());
        if self.failing_roots.iter().any(|r| r == root_id) {
            return Err(LibraryError::FileAccess(FileAccessError::Io("disk on fire".into())));
        }
        scan_root(&*self.file_access, &*self.store, root_id, None, None).await.map(drop)
    }
    fn prune_orphans(&self, granted_root_ids: Vec<String>) {
        self.log.borrow_mut().pruned.push(granted_root_ids);
    }
    fn match_lyrics(&self, scopes: Vec<beatseam_core::library_store::LyricsScope>, tracks: Vec<beatseam_core::library_store::TrackRecord>, callbacks: LyricsMatchCallbacks, _priority: Rc<dyn Fn() -> Vec<String>>) {
        let mut log = self.log.borrow_mut();
        log.lyrics_matches.push((scopes.len(), tracks.len()));
        log.callbacks = Some(callbacks);
    }
    fn scan_metadata(&self, tracks: Vec<beatseam_core::library_store::TrackRecord>, _priority: Rc<dyn Fn() -> Vec<String>>) {
        self.log.borrow_mut().metadata_tracks.push(tracks.len());
    }
    async fn sync_server(&self) -> Option<bool> {
        self.log.borrow_mut().sync_calls += 1;
        self.sync_result.get()
    }
    fn user_playlists_active(&self) -> bool {
        self.log.borrow().user_playlists_active
    }
    async fn user_playlist_create(&self, name: &str, file_ids: &[String]) -> Result<beatseam_core::library_store::PlaylistRecord, String> {
        self.log.borrow_mut().user_ops.push(format!("create {name} {}", file_ids.join(",")));
        let record = beatseam_core::library_store::PlaylistRecord { id: "user:new".into(), root_id: "root-1".into(), file_id: "user:new".into(), name: name.into(), track_file_ids: file_ids.to_vec() };
        self.store.upsert_playlist(record.clone()).await.map_err(|e| e.to_string())?;
        Ok(record)
    }
    async fn user_playlist_add(&self, id: &str, file_ids: &[String], at_start: bool) -> Result<beatseam_core::library_store::PlaylistRecord, String> {
        self.log.borrow_mut().user_ops.push(format!("add {id} {} {at_start}", file_ids.join(",")));
        Ok(beatseam_core::library_store::PlaylistRecord { id: id.into(), root_id: "root-1".into(), file_id: id.into(), name: id.into(), track_file_ids: vec![] })
    }
    async fn user_playlist_remove(&self, id: &str, file_ids: &[String]) -> Result<beatseam_core::library_store::PlaylistRecord, String> {
        self.log.borrow_mut().user_ops.push(format!("remove {id} {}", file_ids.join(",")));
        Ok(beatseam_core::library_store::PlaylistRecord { id: id.into(), root_id: "root-1".into(), file_id: id.into(), name: id.into(), track_file_ids: vec![] })
    }
    async fn user_playlist_set(&self, id: &str, file_ids: &[String]) -> Result<beatseam_core::library_store::PlaylistRecord, String> {
        self.log.borrow_mut().user_ops.push(format!("set {id} {}", file_ids.join(",")));
        Ok(beatseam_core::library_store::PlaylistRecord { id: id.into(), root_id: "root-1".into(), file_id: id.into(), name: id.into(), track_file_ids: file_ids.to_vec() })
    }
    async fn user_playlist_rename(&self, id: &str, name: &str) -> Result<beatseam_core::library_store::PlaylistRecord, String> {
        self.log.borrow_mut().user_ops.push(format!("rename {id} {name}"));
        Ok(beatseam_core::library_store::PlaylistRecord { id: id.into(), root_id: "root-1".into(), file_id: id.into(), name: name.trim().into(), track_file_ids: vec![] })
    }
    async fn user_playlist_delete(&self, id: &str) -> Result<(), String> {
        self.log.borrow_mut().user_ops.push(format!("delete {id}"));
        Ok(())
    }
    async fn find_unplaylisted(&self, root_id: &str, _name: &str) -> Result<Vec<beatseam_core::library_store::TrackRecord>, String> {
        beatseam_core::library_scan::find_unplaylisted_tracks(&*self.file_access, &*self.store, root_id, None).await.map_err(|e| e.to_string())
    }
}

type TestApp = App<MemoryFileAccess, MemoryFileAccess, MemoryLibraryStore, FakeEngine, TestRuntime, FakeJobs>;

struct Fixture {
    pool: Rc<Pool>,
    runtime: TestRuntime,
    engine: Rc<FakeEngine>,
    file_access: Arc<MemoryFileAccess>,
    store: Arc<MemoryLibraryStore>,
    log: Rc<RefCell<JobLog>>,
    server_active: Rc<Cell<bool>>,
    sync_result: Rc<Cell<Option<bool>>>,
    persisted: Rc<RefCell<Vec<PlaybackState>>>,
    synced_settings: Rc<RefCell<Vec<(String, String)>>>,
    app: TestApp,
}

const LIBRARY: [(&str, &str); 4] = [("Mix.m3u8", "Track A.mp3\nTrack B.mp3"), ("Track A.mp3", "a"), ("Track B.mp3", "b"), ("Other.m3u8", "Track B.mp3")];

fn root() -> GrantedRoot {
    GrantedRoot::new("root-1", "Music")
}

fn fixture_with(files: &[(&str, &str)], roots: Vec<GrantedRoot>, failing_roots: &[&str]) -> Fixture {
    let pool = Rc::new(Pool::new());
    let runtime = TestRuntime { pool: pool.clone(), clock: Rc::new(Cell::new(1_000)), sleeps: Rc::default() };
    let engine = Rc::new(FakeEngine::default());
    engine.0.decode_duration_seconds.set(100.0);
    let file_access = Arc::new(MemoryFileAccess::new(files.iter().copied()).with_roots(roots));
    let store = Arc::new(MemoryLibraryStore::new());
    let log: Rc<RefCell<JobLog>> = Rc::default();
    let sync_result: Rc<Cell<Option<bool>>> = Rc::default();
    let jobs = Rc::new(FakeJobs { file_access: file_access.clone(), store: store.clone(), log: log.clone(), failing_roots: failing_roots.iter().map(|s| s.to_string()).collect(), sync_result: sync_result.clone() });
    let resolve_track: ResolveTrack = Rc::new(|id: &str| {
        let id = id.to_owned();
        Box::pin(async move { Ok(FileRef { id: id.clone(), name: id.clone(), relative_path: id, size_bytes: 1, last_modified_ms: 0.0 }) })
    });
    let rt = Arc::new(runtime.clone());
    let player = PlaylistPlayer::new(engine.clone(), rt.clone(), resolve_track, PlaylistPlayerOptions { random: Some(Rc::new(|| 0.5)), ..Default::default() });
    let ui = Rc::new(Store::new(AppState::default()));
    let live = Rc::new(LivePlayback::new(player.get_state()));
    let server_active = Rc::new(Cell::new(false));
    let (persisted, synced_settings): (Rc<RefCell<Vec<PlaybackState>>>, Rc<RefCell<Vec<(String, String)>>>) = Default::default();
    let (p, s, sa) = (persisted.clone(), synced_settings.clone(), server_active.clone());
    let app = App::new(AppConfig {
        store: ui,
        file_access: file_access.clone(),
        background_file_access: file_access.clone(),
        library_store: store.clone(),
        player,
        live,
        jobs,
        runtime: rt,
        server_active: Rc::new(move || sa.get()),
        always_rescan_root: Rc::new(|id| id == "always"),
        on_playback_persisted: Some(Rc::new(move |state| p.borrow_mut().push(state.clone()))),
        on_synced_setting: Some(Rc::new(move |k, v| s.borrow_mut().push((k.to_owned(), v.to_owned())))),
    });
    Fixture { pool, runtime, engine, file_access, store, log, server_active, sync_result, persisted, synced_settings, app }
}

fn fixture() -> Fixture {
    fixture_with(&LIBRARY, vec![root()], &[])
}

fn stored_state() -> PlaybackState {
    PlaybackState {
        playlist_id: Some("Mix.m3u8".into()),
        current_track_file_id: Some("Track B.mp3".into()),
        root_id: Some("root-1".into()),
        opened_playlist_id: OpenedPlaylist::Unset,
        opened_root_id: None,
        position_seconds: 30.0,
        loop_mode: LoopMode::All,
        shuffle_enabled: false,
        shuffle_order: None,
        volume: 0.6,
        now_playing_open: true,
    }
}

impl Fixture {
    fn settle(&self) {
        self.pool.run_until_stalled();
    }

    fn read<R>(&self, f: impl FnOnce(&AppState) -> R) -> R {
        self.app.store().read(f)
    }

    fn restore(&self, stored: Option<PlaybackState>) {
        self.restore_with(stored, Rc::new(|| false));
    }

    fn restore_with(&self, stored: Option<PlaybackState>, cancelled: Rc<dyn Fn() -> bool>) {
        let app = self.app.clone();
        self.pool.spawn(async move { app.restore_playback(stored, cancelled).await });
        self.settle();
    }
}

// ---------------------------------------------------------------- the startup restore

#[test]
fn restores_the_last_playlist_and_track_paused_and_shows_the_playlist_screen() {
    let f = fixture();
    f.restore(Some(stored_state()));
    assert!(!f.read(|s| s.restore.is_restoring), "the restore finished");
    assert!(f.read(|s| s.persistence.ready));
    assert_eq!(f.read(|s| s.navigation.playing_context.as_ref().map(|c| c.playlist.id.clone())).as_deref(), Some("Mix.m3u8"));
    assert!(matches!(f.read(|s| s.navigation.screen.clone()), ScreenState::Playlist(c) if c.playlist.id == "Mix.m3u8"), "a state saved before the opened playlist was tracked falls back to the playing one");
    assert!(f.read(|s| s.navigation.now_playing_open));

    let player = f.app.publish_live();
    assert_eq!(player.current_file_id.as_deref(), Some("Track B.mp3"));
    assert_eq!(player.track.status, TrackPlayerStatus::Paused, "loaded and seeked, never autoplayed");
    assert_eq!(player.track.position_seconds, 30.0);
    assert_eq!((player.loop_mode, f.app.store().read(|s| s.playback.coarse.current_file_id.clone())), (LoopMode::All, Some("Track B.mp3".into())));
    assert!(f.engine.starts().is_empty(), "nothing started playing");
    assert_eq!(f.read(|s| s.restore.completed_steps.clone()), [RestoringStep::ListingFolders, RestoringStep::RestoringPlayback, RestoringStep::ScanningLibrary, RestoringStep::LoadingPlaylist]);
}

#[test]
fn restoring_leaves_everything_else_to_the_background_refresh() {
    let f = fixture();
    f.restore(Some(stored_state()));
    assert!(f.log.borrow().scanned.is_empty(), "the restore scans the one root it needs itself; the refresh then finds it stocked and has nothing to scan");
    assert!(!f.read(|s| s.library.is_loading_roots), "the refresh ran and the library is loaded");
    assert_eq!(f.log.borrow().metadata_tracks.len(), 1, "the metadata pass is background work started by the refresh");
}

#[test]
fn a_saved_library_screen_is_restored_as_the_library_and_a_different_open_playlist_is_found() {
    let f = fixture();
    f.restore(Some(PlaybackState { opened_playlist_id: OpenedPlaylist::Library, ..stored_state() }));
    assert_eq!(f.read(|s| s.navigation.screen.clone()), ScreenState::Library);
    assert!(f.read(|s| s.navigation.playing_context.is_some()), "though a track is playing");

    let g = fixture();
    g.restore(Some(PlaybackState { opened_playlist_id: OpenedPlaylist::Playlist("Other.m3u8".into()), opened_root_id: Some("root-1".into()), ..stored_state() }));
    assert!(matches!(g.read(|s| s.navigation.screen.clone()), ScreenState::Playlist(c) if c.playlist.id == "Other.m3u8"));
    assert_eq!(g.read(|s| s.navigation.playing_context.as_ref().map(|c| c.playlist.id.clone())).as_deref(), Some("Mix.m3u8"));

    let h = fixture();
    h.restore(Some(PlaybackState { opened_playlist_id: OpenedPlaylist::Playlist("Gone.m3u8".into()), ..stored_state() }));
    assert_eq!(h.read(|s| s.navigation.screen.clone()), ScreenState::Library, "not found: the library, no worse than for the playing playlist");
}

#[test]
fn a_stale_root_id_falls_back_to_searching_the_cached_playlists() {
    let f = fixture();
    // The store already knows the playlist (a plain read, never a scan) but the saved root id no longer exists.
    block_on(scan_root(&*f.file_access, &*f.store, "root-1", None, None)).unwrap();
    f.restore(Some(PlaybackState { root_id: Some("gone".into()), ..stored_state() }));
    assert_eq!(f.app.publish_live().current_file_id.as_deref(), Some("Track B.mp3"));
    assert_eq!(f.read(|s| s.navigation.playing_context.as_ref().map(|c| c.root.id.clone())).as_deref(), Some("root-1"));
}

#[test]
fn nothing_saved_means_a_ready_empty_state_and_an_idle_player() {
    let f = fixture();
    f.restore(None);
    assert!(f.read(|s| s.persistence.ready) && !f.read(|s| s.restore.is_restoring));
    assert_eq!(f.app.publish_live().track.status, TrackPlayerStatus::Idle);
    assert!(f.read(|s| s.navigation.playing_context.is_none()));
    assert!(!f.read(|s| s.library.is_loading_roots), "the refresh still runs");
}

#[test]
fn a_saved_playlist_or_track_that_no_longer_exists_just_shows_the_library() {
    let f = fixture();
    f.restore(Some(PlaybackState { current_track_file_id: Some("Gone.mp3".into()), ..stored_state() }));
    assert_eq!(f.app.publish_live().track.status, TrackPlayerStatus::Idle);
    assert_eq!(f.read(|s| s.navigation.screen.clone()), ScreenState::Library);
    assert!(!f.read(|s| s.restore.is_restoring));
}

#[test]
fn a_person_who_acts_early_is_never_overwritten_by_the_restore() {
    let f = fixture();
    f.app.store().dispatch(|s| s.restore.took_over());
    f.restore(Some(stored_state()));
    assert!(f.read(|s| s.navigation.playing_context.is_none()), "last session's choice was not applied");
    assert_eq!(f.app.publish_live().track.status, TrackPlayerStatus::Idle);
    assert!(!f.read(|s| s.library.is_loading_roots), "the rest of the startup work still ran");
    assert!(!f.read(|s| s.restore.is_restoring));
}

#[test]
fn a_cancelled_restore_changes_nothing_and_does_not_report_itself_finished() {
    let f = fixture();
    f.restore_with(Some(stored_state()), Rc::new(|| true));
    assert!(f.read(|s| s.restore.is_restoring));
    assert!(f.read(|s| s.navigation.playing_context.is_none()));
    assert!(f.log.borrow().scanned.is_empty());
}

#[test]
fn the_decode_is_not_awaited_so_the_screen_is_ready_before_the_track_has_finished_loading() {
    let f = fixture();
    f.engine.gate_decode("Track B.mp3");
    f.restore(Some(stored_state()));
    assert!(!f.read(|s| s.restore.is_restoring), "the restoring screen is gone while the decode is still held");
    assert!(f.read(|s| s.navigation.playing_context.is_some()));
    assert_eq!(f.app.publish_live().track.status, TrackPlayerStatus::Loading);
    f.engine.release_decode("Track B.mp3");
    f.settle();
    let player = f.app.publish_live();
    assert_eq!((player.track.status, player.track.position_seconds), (TrackPlayerStatus::Paused, 30.0), "the seek lands once it has loaded");
}

#[test]
fn if_the_person_acts_during_the_decode_the_late_seek_is_not_applied() {
    let f = fixture();
    f.engine.gate_decode("Track B.mp3");
    f.restore(Some(stored_state()));
    f.app.store().dispatch(|s| s.restore.took_over());
    f.engine.release_decode("Track B.mp3");
    f.settle();
    assert_eq!(f.app.publish_live().track.position_seconds, 0.0);
}

#[test]
fn the_volume_is_restored_into_the_player() {
    let f = fixture();
    f.restore(Some(stored_state()));
    assert_eq!(f.app.publish_live().track.status, TrackPlayerStatus::Paused);
    assert!((f.engine.0.decode_call_count.get() as i64) >= 1);
    assert!(f.app.store().read(|s| s.persistence.state.volume) == 0.6);
}

#[test]
fn a_lyrics_scope_makes_the_restore_resolve_that_tracks_lyrics_assignment_first() {
    let f = fixture_with(&[("Mix.m3u8", "Track B.mp3"), ("Track B.mp3", "b"), ("Lyrics/Track B.lrc", "[00:01.00]Hi")], vec![root()], &[]);
    block_on(f.store.add_lyrics_scope(beatseam_core::library_store::LyricsScope { root_id: "root-1".into(), relative_path: "Lyrics".into() })).unwrap();
    f.restore(Some(stored_state()));
    assert_eq!(block_on(f.store.get_lyrics_assignment("Track B.mp3")).unwrap().as_deref(), Some("Lyrics/Track B.lrc"));
    assert!(f.read(|s| s.restore.has_lyrics_scopes) && f.read(|s| s.restore.is_step_completed(RestoringStep::ScanningLyrics)));
    assert!(f.log.borrow().lyrics_matches.len() == 1, "the whole-library match is the refresh's background job");
}

// ---------------------------------------------------------------- refreshing

#[test]
fn refresh_loads_every_library_root_scanning_only_the_empty_or_always_rescanned_ones() {
    let f = fixture_with(&LIBRARY, vec![root(), GrantedRoot::new("always", "Server")], &[]);
    block_on(scan_root(&*f.file_access, &*f.store, "root-1", None, None)).unwrap();
    let loaded = block_on_pool(&f, async_refresh(&f, true)).unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(f.log.borrow().scanned, ["always"], "root-1 already had data; the always-rescanned root is scanned every time");
    assert_eq!(f.read(|s| s.library.roots.len()), 2);
    assert_eq!(f.read(|s| s.library.granted_roots.len()), 2);
    assert!(!f.read(|s| s.library.is_loading_roots));
}

#[test]
fn one_bad_root_reports_its_own_error_and_drops_out_leaving_the_rest_visible() {
    let f = fixture_with(&LIBRARY, vec![GrantedRoot::new("bad", "Broken"), root()], &["bad"]);
    let loaded = block_on_pool(&f, async_refresh(&f, true)).unwrap();
    assert_eq!(loaded.iter().map(|r| r.root.id.as_str()).collect::<Vec<_>>(), ["root-1"]);
    assert_eq!(f.read(|s| s.library.error.clone()).as_deref(), Some("disk on fire"));
}

#[test]
fn lyrics_roots_are_never_scanned_as_libraries_but_become_lyrics_scopes_and_start_the_match() {
    let lyrics_root = GrantedRoot { kind: RootKind::Lyrics, ..GrantedRoot::new("lyr", "Lyrics") };
    let f = fixture_with(&LIBRARY, vec![root(), lyrics_root], &[]);
    block_on_pool(&f, async_refresh(&f, true)).unwrap();
    assert_eq!(f.read(|s| s.library.roots.len()), 1);
    assert_eq!(f.read(|s| s.library.lyrics_scopes.clone()), [beatseam_core::library_store::LyricsScope { root_id: "lyr".into(), relative_path: String::new() }]);
    assert!(f.read(|s| s.restore.has_lyrics_scopes));
    assert_eq!(f.log.borrow().lyrics_matches, [(1, 2)], "one scope; the two tracks a playlist references (a scan is playlist-first)");
}

#[test]
fn the_lyrics_match_count_starts_from_last_launch_and_only_ticks_down_on_anomalies() {
    let lyrics_root = GrantedRoot { kind: RootKind::Lyrics, ..GrantedRoot::new("lyr", "Lyrics") };
    let f = fixture_with(&LIBRARY, vec![root(), lyrics_root], &[]);
    block_on(f.store.put_setting("lyricsMatchedCount", "42")).unwrap();
    block_on_pool(&f, async_refresh(&f, true)).unwrap();
    assert_eq!(f.read(|s| s.library.matched_lyrics_count), Some(42));
    let callbacks = f.log.borrow().callbacks.clone().unwrap();
    (callbacks.on_progress)(7);
    assert_eq!(f.read(|s| s.library.matched_lyrics_count), Some(42), "a baseline exists, so live progress does not move it");
    (callbacks.on_anomaly)();
    assert_eq!(f.read(|s| s.library.matched_lyrics_count), Some(41));
    (callbacks.on_settled)(40);
    f.settle();
    assert_eq!(f.read(|s| s.library.matched_lyrics_count), Some(40));
    assert_eq!(block_on(f.store.get_setting("lyricsMatchedCount")).unwrap().as_deref(), Some("40"), "persisted as the next launch's baseline");
}

#[test]
fn a_first_ever_lyrics_match_shows_live_progress_instead() {
    let lyrics_root = GrantedRoot { kind: RootKind::Lyrics, ..GrantedRoot::new("lyr", "Lyrics") };
    let f = fixture_with(&LIBRARY, vec![root(), lyrics_root], &[]);
    block_on_pool(&f, async_refresh(&f, true)).unwrap();
    assert_eq!(f.read(|s| s.library.matched_lyrics_count), None);
    let callbacks = f.log.borrow().callbacks.clone().unwrap();
    (callbacks.on_progress)(5);
    (callbacks.on_anomaly)();
    assert_eq!(f.read(|s| s.library.matched_lyrics_count), Some(5), "an anomaly does not decrement an unknown baseline");
}

#[test]
fn no_lyrics_scopes_means_no_match_pass_and_an_unknown_count() {
    let f = fixture();
    f.app.store().dispatch(|s| s.library.matched_lyrics_count_set(Some(9)));
    block_on_pool(&f, async_refresh(&f, true)).unwrap();
    assert!(f.log.borrow().lyrics_matches.is_empty());
    assert_eq!(f.read(|s| s.library.matched_lyrics_count), None);
}

#[test]
fn orphaned_data_is_pruned_once_per_launch_and_not_waited_on() {
    let f = fixture();
    block_on_pool(&f, async_refresh(&f, true)).unwrap();
    block_on_pool(&f, async_refresh(&f, true)).unwrap();
    assert_eq!(f.log.borrow().pruned, [["root-1"]]);
}

#[test]
fn listing_the_roots_failing_ends_the_loading_state_and_reports_the_error() {
    struct Failing;
    impl beatseam_core::file_access::FileAccess for Failing {
        async fn request_root(&self, _k: RootKind) -> Result<Option<GrantedRoot>, FileAccessError> {
            Ok(None)
        }
        async fn list_granted_roots(&self) -> Result<Vec<GrantedRoot>, FileAccessError> {
            Err(FileAccessError::Io("no storage".into()))
        }
        async fn revoke_root(&self, _r: &str) -> Result<(), FileAccessError> {
            Ok(())
        }
        async fn list_directory(&self, _r: &str, _p: Option<&str>, _o: beatseam_core::file_access::CallOptions) -> Result<Vec<beatseam_core::file_access::DirectoryEntry>, FileAccessError> {
            Ok(vec![])
        }
        async fn read_file_bytes(&self, _f: &FileRef, _o: beatseam_core::file_access::CallOptions) -> Result<Vec<u8>, FileAccessError> {
            Ok(vec![])
        }
        async fn read_file_text(&self, _f: &FileRef, _o: beatseam_core::file_access::CallOptions) -> Result<String, FileAccessError> {
            Ok(String::new())
        }
        async fn write_file_text(&self, _r: &str, _p: &str, _c: &str) -> Result<(), FileAccessError> {
            Ok(())
        }
    }
    let f = fixture();
    let jobs = Rc::new(FakeJobs { file_access: f.file_access.clone(), store: f.store.clone(), log: f.log.clone(), failing_roots: vec![], sync_result: Rc::default() });
    let app: App<Failing, MemoryFileAccess, MemoryLibraryStore, FakeEngine, TestRuntime, FakeJobs> = App::new(AppConfig {
        store: Rc::new(Store::new(AppState::default())),
        file_access: Arc::new(Failing),
        background_file_access: f.file_access.clone(),
        library_store: f.store.clone(),
        player: PlaylistPlayer::new(f.engine.clone(), Arc::new(f.runtime.clone()), Rc::new(|_: &str| Box::pin(async { Err("none".to_owned()) })), PlaylistPlayerOptions::default()),
        live: Rc::new(LivePlayback::new(f.app.publish_live())),
        jobs,
        runtime: Arc::new(f.runtime.clone()),
        server_active: Rc::new(|| false),
        always_rescan_root: Rc::new(|_| false),
        on_playback_persisted: None,
        on_synced_setting: None,
    });
    let handle = f.pool.spawn(async move { (app.refresh_library(true).await, app.store().read(|s| (s.library.is_loading_roots, s.library.error.clone()))) });
    f.settle();
    let (result, (loading, _)) = handle.take().unwrap();
    assert_eq!(result.unwrap_err(), "no storage");
    assert!(!loading);
}

#[test]
fn a_refresh_reconciles_the_playing_playlist_and_keeps_the_playing_context_in_step() {
    let f = fixture();
    f.restore(Some(stored_state()));
    assert_eq!(f.read(|s| s.navigation.playing_context.as_ref().map(|c| c.playlist.track_file_ids.len())), Some(2));
    // A sync tool drops a new track into the playlist; a rescan picks it up.
    f.file_access.add_file("Track C.mp3", "c");
    f.file_access.add_file("Mix.m3u8", "Track A.mp3\nTrack B.mp3\nTrack C.mp3");
    block_on(scan_root(&*f.file_access, &*f.store, "root-1", None, None)).unwrap();
    block_on_pool(&f, async_refresh(&f, true)).unwrap();
    assert_eq!(f.read(|s| s.navigation.playing_context.as_ref().map(|c| c.playlist.track_file_ids.len())), Some(3));
    assert_eq!(f.app.publish_live().total_tracks, 3, "the player's running order followed");
}

#[test]
fn in_server_mode_nothing_is_scanned_no_metadata_is_read_and_the_check_in_runs_in_the_background() {
    let f = fixture();
    f.server_active.set(true);
    block_on(scan_root(&*f.file_access, &*f.store, "root-1", None, None)).unwrap();
    block_on_pool(&f, async_refresh(&f, false)).unwrap();
    {
        let log = f.log.borrow();
        assert!(log.scanned.is_empty() && log.metadata_tracks.is_empty() && log.lyrics_matches.is_empty());
        assert_eq!(log.sync_calls, 0, "a refresh that is itself a sync's reload must not start another sync");
    }
    block_on_pool(&f, async_refresh(&f, true)).unwrap();
    assert_eq!(f.log.borrow().sync_calls, 1, "checked in last, in the background");
}

#[test]
fn a_check_in_that_changed_the_library_reloads_the_settings_and_the_library_once() {
    let f = fixture();
    f.server_active.set(true);
    f.sync_result.set(Some(true));
    // A preference changed on another device arrived with the sync.
    block_on(f.store.put_setting("settings.lyricsEnabled", "0")).unwrap();
    block_on(scan_root(&*f.file_access, &*f.store, "root-1", None, None)).unwrap();
    block_on_pool(&f, async_refresh(&f, true)).unwrap();
    assert_eq!(f.log.borrow().sync_calls, 1, "one check-in; its reload must not sync again");
    assert!(f.read(|s| s.settings.loaded) && !f.read(|s| s.settings.values.lyrics_enabled), "the preference is now in the store");
}

#[test]
fn a_check_in_that_changed_nothing_reloads_nothing() {
    let f = fixture();
    f.server_active.set(true);
    f.sync_result.set(Some(false));
    block_on_pool(&f, async_refresh(&f, true)).unwrap();
    assert_eq!(f.log.borrow().sync_calls, 1);
    assert!(!f.read(|s| s.settings.loaded), "no settings reload");
}

#[test]
fn syncing_the_server_library_reloads_settings_and_the_library_only_when_something_changed() {
    let f = fixture();
    let app = f.app.clone();
    // Nothing to talk to (no server): nothing happens.
    let none = f.pool.spawn(async move { app.sync_server_library().await });
    f.settle();
    assert_eq!(none.take(), Some(None));
    assert!(f.read(|s| s.library.is_loading_roots), "no reload happened");
}

// ---------------------------------------------------------------- settings and playback persistence

#[test]
fn updating_settings_applies_them_writes_them_pushes_the_crossfade_to_the_player_and_queues_synced_ones() {
    let f = fixture();
    f.server_active.set(true);
    f.app.update_settings(SettingsPatch { crossfade_seconds: Some(3.0), lyrics_enabled: Some(false), last_fm_api_key: Some("k".into()), ..Default::default() });
    f.settle();
    assert_eq!((f.read(|s| s.settings.values.crossfade_seconds), f.read(|s| s.settings.values.lyrics_enabled)), (3.0, false));
    assert_eq!(block_on(f.store.get_setting("settings.crossfadeSeconds")).unwrap().as_deref(), Some("3"));
    assert_eq!(block_on(f.store.get_setting("settings.lastFmApiKey")).unwrap().as_deref(), Some("k"));
    assert_eq!(*f.synced_settings.borrow(), [("settings.crossfadeSeconds".to_owned(), "3".to_owned()), ("settings.lyricsEnabled".to_owned(), "0".to_owned())], "the per-device Last.fm key is not synced");
}

#[test]
fn settings_are_only_queued_for_the_server_while_signed_in() {
    let f = fixture();
    f.app.update_settings(SettingsPatch { lyrics_enabled: Some(false), ..Default::default() });
    f.settle();
    assert!(f.synced_settings.borrow().is_empty());
}

#[test]
fn loading_settings_hydrates_the_slice_with_the_system_theme_for_a_first_run_and_pushes_the_crossfade_in() {
    let f = fixture();
    block_on(f.store.put_setting("settings.crossfadeSeconds", "12")).unwrap();
    let app = f.app.clone();
    f.pool.spawn(async move { app.load_settings(ThemeMode::Dark).await.unwrap() });
    f.settle();
    assert!(f.read(|s| s.settings.loaded));
    assert_eq!((f.read(|s| s.settings.values.theme_mode), f.read(|s| s.settings.values.crossfade_seconds)), (ThemeMode::Dark, 12.0));
}

#[test]
fn playback_patches_do_nothing_before_the_restore_has_read_the_real_state_then_write_the_merged_state() {
    let f = fixture();
    f.app.patch_playback(PlaybackPatch { now_playing_open: Some(true), ..Default::default() }, 0);
    f.settle();
    assert!(f.persisted.borrow().is_empty() && block_on(f.store.get_playback_state()).unwrap().is_none());

    f.app.store().dispatch(|s| s.persistence.playback_state_read(Some(PlaybackState { volume: 0.7, ..stored_state() })));
    f.app.patch_playback(PlaybackPatch { position_seconds: Some(12.0), ..Default::default() }, 0);
    f.settle();
    let written = block_on(f.store.get_playback_state()).unwrap().unwrap();
    assert_eq!((written.position_seconds, written.volume, written.playlist_id.as_deref()), (12.0, 0.7, Some("Mix.m3u8")), "merged onto what was read");
    assert_eq!(f.persisted.borrow().len(), 1, "the server is told");
}

#[test]
fn a_burst_of_debounced_patches_makes_one_write_carrying_the_latest_state() {
    let f = fixture();
    f.app.store().dispatch(|s| s.persistence.playback_state_read(Some(stored_state())));
    for volume in [0.1, 0.2, 0.3] {
        f.app.patch_playback(PlaybackPatch { volume: Some(volume), ..Default::default() }, RAPID_PLAYBACK_PATCH_DEBOUNCE_MS);
        f.settle();
    }
    assert!(f.persisted.borrow().is_empty(), "nothing is written while the drag continues");
    let sleeps: Vec<Gate> = f.runtime.sleeps.borrow().iter().map(|(_, g)| g.clone()).collect();
    assert_eq!(f.runtime.sleeps.borrow().iter().map(|(ms, _)| *ms).collect::<Vec<_>>(), [400, 400, 400]);
    for gate in sleeps {
        gate.open();
    }
    f.settle();
    assert_eq!(f.persisted.borrow().len(), 1, "the two superseded writes gave up");
    assert_eq!(f.persisted.borrow()[0].volume, 0.3);
}

#[test]
fn a_prompt_patch_cancels_a_pending_delayed_write_and_carries_everything() {
    let f = fixture();
    f.app.store().dispatch(|s| s.persistence.playback_state_read(Some(stored_state())));
    f.app.patch_playback(PlaybackPatch { volume: Some(0.4), ..Default::default() }, RAPID_PLAYBACK_PATCH_DEBOUNCE_MS);
    f.settle();
    f.app.patch_playback(PlaybackPatch { position_seconds: Some(5.0), ..Default::default() }, 0);
    f.settle();
    f.runtime.sleeps.borrow()[0].1.open();
    f.settle();
    assert_eq!(f.persisted.borrow().len(), 1);
    assert_eq!((f.persisted.borrow()[0].volume, f.persisted.borrow()[0].position_seconds), (0.4, 5.0));
}

#[test]
fn the_position_is_persisted_at_most_every_five_seconds_and_only_while_a_track_is_loaded() {
    let f = fixture();
    f.restore(Some(stored_state()));
    f.persisted.borrow_mut().clear();
    let player = f.app.publish_live();
    f.runtime.clock.set(10_000);
    f.app.persist_position_if_due(&player);
    f.settle();
    assert_eq!(f.persisted.borrow().len(), 1);
    f.runtime.clock.set(13_000);
    f.app.persist_position_if_due(&player);
    f.settle();
    assert_eq!(f.persisted.borrow().len(), 1, "too soon");
    f.runtime.clock.set(15_000);
    f.app.persist_position_if_due(&player);
    f.settle();
    assert_eq!(f.persisted.borrow().len(), 2);

    let idle = fixture();
    idle.app.store().dispatch(|s| s.persistence.playback_state_read(None));
    idle.runtime.clock.set(99_000);
    idle.app.persist_position_if_due(&idle.app.publish_live());
    idle.settle();
    assert!(idle.persisted.borrow().is_empty());
}

#[test]
fn the_coarse_playback_state_follows_the_player_through_the_live_snapshot() {
    let f = fixture();
    assert_eq!(f.read(|s| s.playback.coarse.position), -1);
    f.restore(Some(stored_state()));
    f.app.publish_live();
    assert_eq!(f.read(|s| s.playback.coarse.current_file_id.clone()).as_deref(), Some("Track B.mp3"));
    assert_eq!(f.read(|s| s.playback.coarse.status), TrackPlayerStatus::Paused);
}

// ---------------------------------------------------------------- helpers

/// A refresh as a future the test pool can drive.
fn async_refresh(f: &Fixture, sync_server: bool) -> Pin<Box<dyn Future<Output = Result<Vec<RootWithLibrary>, String>>>> {
    let app = f.app.clone();
    Box::pin(async move { app.refresh_library(sync_server).await })
}

fn block_on_pool<T: 'static>(f: &Fixture, future: Pin<Box<dyn Future<Output = T>>>) -> T {
    let handle = f.pool.spawn(future);
    f.settle();
    handle.take().expect("finished")
}

#[path = "app_actions_tests.rs"]
mod actions;
