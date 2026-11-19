use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use beatseam_api::client::DeviceSignInResult;
use beatseam_api::encode_track_key;
use beatseam_core::http::{HttpRequest, HttpResponse, HttpTransport, TransportError};
use beatseam_core::library_store::{LibraryStore, LoopMode, MemoryLibraryStore, OpenedPlaylist, PlaybackState};
use beatseam_core::offline::OfflineRuntime;
use beatseam_core::tasks::Gate;
use beatseam_core::tasks::test_executor::Pool;

use super::*;

// ---- harness ----

#[derive(Default)]
struct NetState {
    routes: RefCell<Vec<(String, VecDeque<(u16, String)>)>>,
    log: RefCell<Vec<(String, Option<String>)>>,
    offline: Cell<bool>,
    /// While set, a request for the snapshot waits here (so a sync stays in flight).
    hold: RefCell<Option<Gate>>,
}

#[derive(Clone, Default)]
struct Net(Rc<NetState>);

impl Net {
    /// Replies to a request whose URL contains `fragment`, in order; the last reply repeats.
    fn route(&self, fragment: &str, replies: &[(u16, &str)]) {
        let mut routes = self.0.routes.borrow_mut();
        routes.retain(|(f, _)| f != fragment);
        routes.push((fragment.to_owned(), replies.iter().map(|(s, b)| (*s, b.to_string())).collect()));
    }

    fn requests_to(&self, fragment: &str) -> Vec<Option<String>> {
        self.0.log.borrow().iter().filter(|(u, _)| u.contains(fragment)).map(|(_, b)| b.clone()).collect()
    }
}

impl HttpTransport for Net {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.0.log.borrow_mut().push((request.url.clone(), request.body.clone()));
        if self.0.offline.get() {
            return Err(TransportError("offline".into()));
        }
        let hold = self.0.hold.borrow().clone().filter(|_| request.url.contains("/sync/snapshot"));
        if let Some(gate) = hold {
            gate.wait().await;
        }
        let mut routes = self.0.routes.borrow_mut();
        let Some((_, replies)) = routes.iter_mut().find(|(f, _)| request.url.contains(f.as_str())) else { return Ok(HttpResponse::new(404, r#"{"message":"no route"}"#)) };
        let (status, body) = if replies.len() > 1 { replies.pop_front().unwrap() } else { replies.front().cloned().unwrap() };
        Ok(HttpResponse::new(status, body))
    }
}

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

#[derive(Clone, Default)]
struct Vault(Rc<RefCell<Option<String>>>);

impl SecretStore for Vault {
    async fn get(&self) -> Option<String> {
        self.0.borrow().clone()
    }
    async fn set(&self, value: Option<&str>) {
        *self.0.borrow_mut() = value.map(str::to_owned);
    }
}

fn me(name: &str, role: &str) -> String {
    format!(r#"{{"id":"u1","username":"u","displayName":"{name}","role":"{role}","enrolled":true,"hasPassword":true,"totpEnabled":false,"passkeys":[],"recoveryCodesRemaining":0,"authenticatedAtMs":null,"freshUntilMs":null,"serverTimeMs":1,"via":"device"}}"#)
}

fn track_json(path: &str, title: &str) -> String {
    format!(
        r#"{{"entity":"track","id":"{}","track":{{"id":"{}","rootId":"root","relativePath":"{path}","sizeBytes":10,"lastModifiedMs":1,"missing":false,"title":"{title}","artists":["A"],"album":null,"durationSeconds":60,"contentHash":null,"normalizationGain":null,"hasCover":false,"hasLyrics":false}}}}"#,
        encode_track_key(path),
        encode_track_key(path)
    )
}

fn roots_json() -> &'static str {
    r#"[{"id":"root","displayName":"Music","kind":"library"},{"id":"lyr","displayName":"Lyrics","kind":"lyrics"}]"#
}

fn snapshot_json(entities: &[String], cursor: &str, next: Option<&str>) -> String {
    format!(r#"{{"entities":[{}],"next":{},"cursor":"{cursor}","roots":{}}}"#, entities.join(","), next.map_or("null".into(), |n| format!("\"{n}\"")), roots_json())
}

fn changes_json(changes: &[String], cursor: &str, has_more: bool, tracks: u64, playlists: u64) -> String {
    format!(r#"{{"changes":[{}],"cursor":"{cursor}","hasMore":{has_more},"roots":{},"totals":{{"tracks":{tracks},"playlists":{playlists}}}}}"#, changes.join(","), roots_json())
}

type Mode = ServerMode<MemoryLibraryStore, Net, TestRuntime, Vault>;

struct Fixture {
    pool: Rc<Pool>,
    runtime: TestRuntime,
    net: Net,
    store: Arc<MemoryLibraryStore>,
    vault: Vault,
    mode: Mode,
}

fn open_with(pool: &Rc<Pool>, runtime: &TestRuntime, net: &Net, store: &Arc<MemoryLibraryStore>, vault: &Vault) -> Mode {
    let counter = Rc::new(Cell::new(0));
    let (r, n, s, v) = (runtime.clone(), net.clone(), store.clone(), vault.clone());
    let opened = pool.spawn(async move {
        ServerMode::open(ServerModeDeps {
            store: s,
            transport: n,
            runtime: r,
            secret_store: v,
            device_name: Box::new(|| "Test Phone".into()),
            new_id: Box::new(move || {
                counter.set(counter.get() + 1);
                format!("id{}", counter.get())
            }),
            same_origin: false,
        })
        .await
    });
    pool.run_until_stalled();
    opened.take().expect("opened").expect("store readable")
}

fn fixture() -> Fixture {
    let pool = Rc::new(Pool::new());
    let runtime = TestRuntime { pool: pool.clone(), clock: Rc::new(Cell::new(1_000_000)), sleeps: Rc::default() };
    let (net, store, vault) = (Net::default(), Arc::new(MemoryLibraryStore::new()), Vault::default());
    let mode = open_with(&pool, &runtime, &net, &store, &vault);
    Fixture { pool, runtime, net, store, vault, mode }
}

impl Fixture {
    fn run<T: 'static>(&self, f: impl Future<Output = T> + 'static) -> T {
        let h = self.pool.spawn(f);
        self.pool.run_until_stalled();
        h.take().expect("finished")
    }

    fn reopen(&self) -> Mode {
        open_with(&self.pool, &self.runtime, &self.net, &self.store, &self.vault)
    }

    fn setting(&self, key: &str) -> Option<String> {
        let s = self.store.clone();
        let key = key.to_owned();
        self.run(async move { s.get_setting(&key).await.unwrap() })
    }

    fn put(&self, key: &str, value: &str) {
        let s = self.store.clone();
        let (key, value) = (key.to_owned(), value.to_owned());
        self.run(async move { s.put_setting(&key, &value).await.unwrap() });
    }

    fn sync(&self) -> SyncOutcome {
        let m = self.mode.clone();
        self.run(async move { m.sync().await })
    }

    /// Signs in through the device-code flow against scripted answers.
    fn sign_in(&self) {
        self.net.route("/auth/device/code", &[(200, r#"{"deviceCode":"dc","userCode":"ABCD","verificationUri":"https://music.example.com/device","expiresInSeconds":600}"#)]);
        let wait = format!(r#"{{"status":"approved","token":"tok","me":{}}}"#, me("Ada", "ADMIN"));
        self.net.route("/auth/device/wait", &[(200, &wait)]);
        let m = self.mode.clone();
        let result = self.run(async move { m.sign_in_with_device_code("music.example.com", |_| {}, None).await });
        assert!(matches!(result, Ok(DeviceSignInResult::SignedIn { .. })), "{result:?}");
    }

    fn track_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self.store.snapshot_tracks().into_iter().map(|t| t.file_id).collect();
        paths.sort();
        paths
    }
}

fn playback(track: &str) -> PlaybackState {
    PlaybackState {
        playlist_id: Some("p".into()),
        current_track_file_id: Some(track.into()),
        root_id: Some("root".into()),
        opened_playlist_id: OpenedPlaylist::Unset,
        opened_root_id: None,
        position_seconds: 1.0,
        loop_mode: LoopMode::Off,
        shuffle_enabled: false,
        shuffle_order: None,
        volume: 1.0,
        now_playing_open: false,
    }
}

// ---- tests ----

#[test]
fn a_fresh_install_is_signed_out_with_a_stable_device_id() {
    let f = fixture();
    assert!(!f.mode.is_active());
    let first = f.setting("serverMode.deviceId").expect("made at first start");
    assert!(!first.is_empty());
    let again = f.reopen();
    assert_eq!(f.setting("serverMode.deviceId").unwrap(), first, "kept");
    assert!(!again.is_active());
    assert_eq!(f.run({ let m = f.mode.clone(); async move { m.sync().await } }), SyncOutcome { kind: SyncKind::Inactive, changed: false });
}

#[test]
fn signing_in_with_a_device_code_saves_the_server_and_the_token_and_activates() {
    let f = fixture();
    let progress = Rc::new(RefCell::new(Vec::new()));
    f.net.route("/auth/device/code", &[(200, r#"{"deviceCode":"dc","userCode":"ABCD","verificationUri":"https://x/device","expiresInSeconds":600}"#)]);
    let wait = format!(r#"{{"status":"approved","token":"tok","me":{}}}"#, me("Ada", "ADMIN"));
    f.net.route("/auth/device/wait", &[(200, r#"{"status":"pending"}"#), (200, &wait)]);
    let p = progress.clone();
    let m = f.mode.clone();
    let result = f.run(async move { m.sign_in_with_device_code("  Music.Example.com/  ", move |step| p.borrow_mut().push(step), None).await });
    assert!(matches!(result, Ok(DeviceSignInResult::SignedIn { .. })));
    assert!(matches!(progress.borrow()[0], beatseam_api::client::DeviceSignInProgress::Code { .. }));
    let status = f.mode.get_status();
    assert_eq!((status.active, status.url.as_str(), status.user_name.as_deref(), status.is_admin), (true, "https://music.example.com", Some("Ada"), Some(true)));
    assert!(f.net.requests_to("/auth/device/code")[0].as_deref().unwrap().contains("Test Phone"), "the device is named");
    let saved = f.setting("serverMode.config").unwrap();
    assert!(saved.contains(r#""role":"ADMIN""#) && !saved.contains("tok\""), "the token is not in the settings: {saved}");
    assert_eq!(f.vault.0.borrow().as_deref(), Some("tok"));
    assert!(f.reopen().is_active(), "survives a restart");
}

#[test]
fn a_bad_address_is_refused_before_any_request() {
    let f = fixture();
    let m = f.mode.clone();
    let err = f.run(async move { m.sign_in_with_device_code("http://example.com", |_| {}, None).await }).unwrap_err();
    assert!(err.to_string().starts_with("Plain http://"));
    assert!(f.net.0.log.borrow().is_empty());
}

#[test]
fn a_denied_sign_in_changes_nothing() {
    let f = fixture();
    f.net.route("/auth/device/code", &[(200, r#"{"deviceCode":"dc","userCode":"ABCD","verificationUri":"https://x/device","expiresInSeconds":600}"#)]);
    f.net.route("/auth/device/wait", &[(200, r#"{"status":"denied"}"#)]);
    let m = f.mode.clone();
    let result = f.run(async move { m.sign_in_with_device_code("music.example.com", |_| {}, None).await });
    assert!(matches!(result, Ok(DeviceSignInResult::Denied)));
    assert!(!f.mode.is_active());
}

#[test]
fn the_first_sync_takes_a_snapshot_page_by_page_keeping_only_library_roots() {
    let f = fixture();
    f.sign_in();
    f.net.route("/sync/snapshot", &[(200, &snapshot_json(&[track_json("a.mp3", "A")], "c1", Some("page2"))), (200, &snapshot_json(&[track_json("b.mp3", "B")], "ignored", None))]);
    f.net.route("/mutations", &[(200, r#"{"results":[]}"#)]);
    let outcome = f.sync();
    assert_eq!(outcome, SyncOutcome { kind: SyncKind::Snapshot, changed: true });
    assert_eq!(f.track_paths(), ["a.mp3", "b.mp3"]);
    assert_eq!(f.mode.roots().iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["root"], "the lyrics root is not a library");
    assert_eq!(f.setting("serverMode.cursor").as_deref(), Some("c1"), "the FIRST page's cursor");
    let status = f.mode.get_status();
    assert_eq!((status.last_synced_at_ms, status.syncing, status.offline, status.error), (Some(1_000_000.0), false, false, None));
    assert_eq!(f.net.requests_to("/sync/snapshot").len(), 2);
    assert!(f.reopen().roots().len() == 1, "roots are remembered");
}

#[test]
fn later_syncs_apply_only_what_changed_and_a_snapshot_drops_what_vanished() {
    let f = fixture();
    f.sign_in();
    f.net.route("/sync/snapshot", &[(200, &snapshot_json(&[track_json("a.mp3", "A"), track_json("b.mp3", "B")], "c1", None))]);
    f.sync();
    f.net.route("/sync/changes", &[(200, &changes_json(&[track_json("c.mp3", "C")], "c2", false, 3, 0))]);
    assert_eq!(f.sync(), SyncOutcome { kind: SyncKind::Delta, changed: true });
    assert_eq!(f.track_paths(), ["a.mp3", "b.mp3", "c.mp3"]);
    assert_eq!(f.setting("serverMode.cursor").as_deref(), Some("c2"));
    assert!(f.net.requests_to("/sync/changes")[0].is_none() && f.net.0.log.borrow().iter().any(|(u, _)| u.contains("since=c1")));

    // Nothing changed: still a delta, and nothing modified.
    f.net.route("/sync/changes", &[(200, &changes_json(&[], "c2", false, 3, 0))]);
    assert_eq!(f.sync(), SyncOutcome { kind: SyncKind::Delta, changed: false });

    // The server's history was trimmed (410): start over, and "b" is gone from the new snapshot.
    f.net.route("/sync/changes", &[(410, r#"{"code":"resync_required","message":"too old"}"#)]);
    f.net.route("/sync/snapshot", &[(200, &snapshot_json(&[track_json("a.mp3", "A"), track_json("c.mp3", "C")], "c9", None))]);
    assert_eq!(f.sync().kind, SyncKind::Snapshot);
    assert_eq!(f.track_paths(), ["a.mp3", "c.mp3"]);
    assert_eq!(f.setting("serverMode.cursor").as_deref(), Some("c9"));
}

#[test]
fn counts_that_disagree_after_a_delta_trigger_a_self_repairing_snapshot() {
    let f = fixture();
    f.sign_in();
    f.net.route("/sync/snapshot", &[(200, &snapshot_json(&[track_json("a.mp3", "A")], "c1", None))]);
    f.sync();
    // Lost a row some way a change feed can't repair: the server says 2 tracks, we hold 1 and the feed adds nothing.
    f.net.route("/sync/changes", &[(200, &changes_json(&[], "c2", false, 2, 0))]);
    f.net.route("/sync/snapshot", &[(200, &snapshot_json(&[track_json("a.mp3", "A"), track_json("b.mp3", "B")], "c3", None))]);
    assert_eq!(f.sync().kind, SyncKind::Snapshot);
    assert_eq!(f.track_paths(), ["a.mp3", "b.mp3"]);
}

#[test]
fn an_unreachable_server_is_offline_not_an_error_and_the_next_sync_recovers() {
    let f = fixture();
    f.sign_in();
    f.net.route("/sync/snapshot", &[(200, &snapshot_json(&[track_json("a.mp3", "A")], "c1", None))]);
    f.sync();
    f.net.0.offline.set(true);
    assert_eq!(f.sync(), SyncOutcome { kind: SyncKind::Offline, changed: false });
    let status = f.mode.get_status();
    assert_eq!((status.offline, status.error, status.syncing), (true, None, false));
    assert_eq!(f.track_paths(), ["a.mp3"], "the cached library stays");
    f.net.0.offline.set(false);
    f.net.route("/sync/changes", &[(200, &changes_json(&[], "c1", false, 1, 0))]);
    assert_eq!(f.sync().kind, SyncKind::Delta);
    assert!(!f.mode.get_status().offline);
}

#[test]
fn a_revoked_token_signs_out_but_leaves_the_library_visible() {
    let f = fixture();
    f.sign_in();
    f.net.route("/sync/snapshot", &[(200, &snapshot_json(&[track_json("a.mp3", "A")], "c1", None))]);
    f.sync();
    f.net.route("/sync/changes", &[(401, r#"{"message":"revoked"}"#)]);
    assert_eq!(f.sync(), SyncOutcome { kind: SyncKind::SignedOut, changed: false });
    let status = f.mode.get_status();
    assert!(!status.active);
    assert_eq!(status.error.as_deref(), Some("You were signed out by the server. Sign in again to keep syncing."));
    assert_eq!(f.track_paths(), ["a.mp3"]);
    assert_eq!(f.setting("serverMode.config").as_deref(), Some(""));
}

#[test]
fn another_failure_is_reported_in_the_status() {
    let f = fixture();
    f.sign_in();
    f.net.route("/sync/snapshot", &[(500, r#"{"message":"database on fire"}"#)]);
    assert_eq!(f.sync(), SyncOutcome { kind: SyncKind::Failed, changed: false });
    assert_eq!(f.mode.get_status().error.as_deref(), Some("database on fire"));
}

#[test]
fn overlapping_syncs_join_one_check_in() {
    let f = fixture();
    f.sign_in();
    f.net.route("/sync/snapshot", &[(200, &snapshot_json(&[], "c1", None))]);
    *f.net.0.hold.borrow_mut() = Some(Gate::new());
    let (m1, m2) = (f.mode.clone(), f.mode.clone());
    let (a, b) = (f.pool.spawn(async move { m1.sync().await }), f.pool.spawn(async move { m2.sync().await }));
    f.pool.run_until_stalled();
    assert!(f.mode.get_status().syncing);
    f.net.0.hold.borrow().clone().unwrap().open();
    f.pool.run_until_stalled();
    assert_eq!((a.take(), b.take()), (Some(outcome(SyncKind::Snapshot, true)), Some(outcome(SyncKind::Snapshot, true))));
    assert_eq!(f.net.requests_to("/sync/snapshot").len(), 1);
}

#[test]
fn pending_changes_are_flushed_at_the_start_of_a_check_in() {
    let f = fixture();
    f.sign_in();
    f.net.0.offline.set(true);
    let m = f.mode.clone();
    f.run(async move { m.mutations().enqueue(beatseam_api::mutations::MutationDto::PlaylistDelete { op_id: String::new(), id: "p".into() }).await });
    assert_eq!(f.mode.mutations().get_state().pending, 1);
    f.net.0.offline.set(false);
    f.net.route("/mutations", &[(200, &format!(r#"{{"results":[{{"opId":"{}","status":"applied"}}]}}"#, f.mode.mutations().pending_mutations()[0].op_id()))]);
    f.net.route("/sync/snapshot", &[(200, &snapshot_json(&[], "c1", None))]);
    f.sync();
    assert_eq!(f.mode.mutations().get_state().pending, 0);
    let order: Vec<String> = f.net.0.log.borrow().iter().map(|(u, _)| u.clone()).filter(|u| u.contains("/mutations") || u.contains("/sync/")).collect();
    assert!(order.iter().position(|u| u.contains("/mutations")).unwrap() < order.iter().position(|u| u.contains("/sync/snapshot")).unwrap());
}

#[test]
fn the_token_goes_to_secret_storage_and_a_lost_secret_means_signed_out() {
    let f = fixture();
    f.sign_in();
    // `Vault` always counts as available, so the sign-in above already used it.
    assert_eq!(f.vault.0.borrow().as_deref(), Some("tok"));
    let saved = f.setting("serverMode.config").unwrap();
    assert!(saved.contains(r#""token":null"#) && saved.contains(r#""tokenInSecretStore":true"#), "{saved}");
    assert!(f.reopen().is_active() && f.reopen().get_api().is_some());
    *f.vault.0.borrow_mut() = None;
    assert!(!f.reopen().is_active(), "the Keystore key was wiped");
}

#[test]
fn a_token_left_in_plain_settings_by_an_older_build_is_moved_on_first_start() {
    let f = fixture();
    f.put("serverMode.config", r#"{"url":"https://music.example.com","token":"old","userName":"Ada","role":"USER"}"#);
    let reopened = f.reopen();
    assert!(reopened.is_active());
    assert_eq!(f.vault.0.borrow().as_deref(), Some("old"));
    let saved = f.setting("serverMode.config").unwrap();
    assert!(saved.contains(r#""token":null"#) && saved.contains("tokenInSecretStore"), "{saved}");
}

#[test]
fn a_saved_sign_in_without_a_role_learns_it_at_the_next_check_in() {
    let f = fixture();
    f.put("serverMode.config", r#"{"url":"https://music.example.com","token":"t","userName":"Ada"}"#);
    let mode = f.reopen();
    assert_eq!(mode.get_status().is_admin, None);
    f.net.route("/auth/me", &[(200, &me("Ada", "ADMIN"))]);
    f.net.route("/sync/snapshot", &[(200, &snapshot_json(&[], "c1", None))]);
    f.run(async move { mode.sync().await });
    assert!(f.setting("serverMode.config").unwrap().contains(r#""role":"ADMIN""#));
}

#[test]
fn signing_out_revokes_the_token_and_forgets_everything_learned_from_the_server() {
    let f = fixture();
    f.sign_in();
    f.net.route("/sync/snapshot", &[(200, &snapshot_json(&[track_json("a.mp3", "A")], "c1", None))]);
    f.sync();
    f.put("serverMode.lastFmStatus", "{}");
    f.net.route("/auth/me/token", &[(204, "")]);
    let m = f.mode.clone();
    f.run(async move { m.sign_out().await.unwrap() });
    assert!(!f.mode.is_active() && f.track_paths().is_empty() && f.mode.roots().is_empty());
    assert_eq!(f.net.requests_to("/auth/me/token").len(), 1);
    assert_eq!(f.vault.0.borrow().as_deref(), None);
    for key in ["serverMode.cursor", "serverMode.roots", "serverMode.syncedAtMs", "serverMode.lastFmStatus", "serverMode.config"] {
        assert_eq!(f.setting(key).as_deref(), Some(""), "{key}");
    }
    assert_eq!(f.mode.get_status().last_synced_at_ms, None);
}

#[test]
fn signing_out_works_offline() {
    let f = fixture();
    f.sign_in();
    f.net.0.offline.set(true);
    let m = f.mode.clone();
    f.run(async move { m.sign_out().await.unwrap() });
    assert!(!f.mode.is_active());
}

#[test]
fn lyrics_are_kept_per_track_and_used_when_the_server_cannot_be_reached() {
    let f = fixture();
    f.sign_in();
    let lyrics = r#"{"synced":true,"tags":{"ar":"X"},"lines":[{"timeSeconds":1,"text":"Hello"}]}"#;
    f.net.route("/lyrics", &[(200, lyrics)]);
    let m = f.mode.clone();
    let fetched = f.run(async move { m.lyrics_for("a.mp3").await }).expect("lyrics");
    assert_eq!(fetched.lines[0].text, "Hello");
    f.net.0.offline.set(true);
    let m = f.mode.clone();
    assert_eq!(f.run(async move { m.lyrics_for("a.mp3").await }).map(|l| l.lines.len()), Some(1), "the kept copy");
    let m = f.mode.clone();
    assert_eq!(f.run(async move { m.lyrics_for("never-seen.mp3").await }), None);
    assert_eq!(f.setting("serverMode.lyricsIndex").as_deref(), Some(r#"["a.mp3"]"#));
}

#[test]
fn no_lyrics_is_remembered_too_and_lyrics_become_the_apps_parsed_shape() {
    let f = fixture();
    f.sign_in();
    f.net.route("/lyrics", &[(404, r#"{"message":"none"}"#)]);
    let m = f.mode.clone();
    assert_eq!(f.run(async move { m.lyrics_for("a.mp3").await }), None);
    let dto: beatseam_api::LyricsDto = serde_json::from_str(r#"{"synced":true,"tags":{"ar":"X"},"lines":[{"timeSeconds":1,"text":"Hi","translation":"Salut"}]}"#).unwrap();
    let parsed = lyrics_from_dto(&dto);
    assert_eq!((parsed.synced, parsed.lines[0].translation.as_deref(), parsed.lines[0].time_seconds), (true, Some("Salut"), Some(1.0)));
}

#[test]
fn a_new_track_is_pushed_at_once_but_the_same_track_only_every_twenty_seconds_with_one_trailing_send() {
    let f = fixture();
    f.sign_in();
    f.net.route("/mutations", &[(200, r#"{"results":[]}"#)]);
    f.mode.push_playback_state(&playback("t1"), true);
    f.pool.run_until_stalled();
    assert_eq!(f.net.requests_to("/mutations").len(), 1, "a new track goes out at once");
    assert!(f.net.requests_to("/mutations")[0].as_deref().unwrap().contains("playbackState.set"));

    // Same track, still playing, 5 s later: wait for the trailing send (once, however many updates arrive).
    f.runtime.clock.set(1_005_000);
    f.mode.push_playback_state(&playback("t1"), true);
    f.mode.push_playback_state(&playback("t1"), true);
    f.pool.run_until_stalled();
    assert_eq!(f.net.requests_to("/mutations").len(), 1);
    assert_eq!(f.runtime.sleeps.borrow().len(), 1);
    assert_eq!(f.runtime.sleeps.borrow()[0].0, 15_000, "20 s after the last send");
    f.runtime.clock.set(1_020_000);
    f.runtime.sleeps.borrow_mut().remove(0).1.open();
    f.pool.run_until_stalled();
    assert_eq!(f.net.requests_to("/mutations").len(), 2);

    // Pausing is news: it doesn't wait either.
    f.runtime.clock.set(1_021_000);
    f.mode.push_playback_state(&playback("t1"), false);
    f.pool.run_until_stalled();
    assert_eq!(f.net.requests_to("/mutations").len(), 3);
}

#[test]
fn nothing_is_pushed_when_signed_out_or_without_a_playing_track() {
    let f = fixture();
    f.mode.push_playback_state(&playback("t1"), true);
    f.sign_in();
    let mut idle = playback("t1");
    idle.current_track_file_id = None;
    f.mode.push_playback_state(&idle, true);
    f.pool.run_until_stalled();
    assert!(f.net.requests_to("/mutations").is_empty());
}

#[test]
fn the_last_fm_status_is_kept_for_when_the_server_cannot_be_reached() {
    let f = fixture();
    f.sign_in();
    f.net.route("/api/v1/lastfm", &[(200, r#"{"connected":true,"username":"ada"}"#)]);
    let m = f.mode.clone();
    assert_eq!(f.run(async move { m.last_fm_status().await }).map(|s| s.connected), Some(true));
    f.net.0.offline.set(true);
    let m = f.mode.clone();
    assert_eq!(f.run(async move { m.last_fm_status().await }).and_then(|s| s.username), Some("ada".into()));
}

#[test]
fn reporting_lyrics_needs_a_sign_in() {
    let f = fixture();
    let m = f.mode.clone();
    assert_eq!(f.run(async move { m.report_lyrics("a.mp3", None).await }).unwrap_err(), ServerModeError::NotSignedIn);
}

#[test]
fn the_server_serves_streamed_bytes_and_lyrics_through_the_link_traits() {
    use crate::{CoverFetch, ServerLink};
    let f = fixture();
    f.sign_in();
    f.net.route("/stream", &[(200, "audio-bytes")]);
    f.net.route("/lyrics", &[(404, r#"{"message":"none"}"#)]);
    let m = f.mode.clone();
    assert_eq!(f.run(async move { m.stream_bytes("a.mp3").await }).unwrap(), b"audio-bytes");
    let m = f.mode.clone();
    assert_eq!(f.run(async move { ServerLink::lyrics_for(&m, "a.mp3").await }).unwrap(), beatseam_core::file_access::LyricsAnswer::NoLyrics);
    f.net.route("/cover", &[(404, r#"{"message":"none"}"#)]);
    let m = f.mode.clone();
    assert!(matches!(f.run(async move { m.fetch_cover("a.mp3").await }), Some(Err(crate::CoverFetchError::NotFound))));

    let signed_out = fixture();
    let m = signed_out.mode.clone();
    assert!(signed_out.run(async move { m.stream_bytes("a.mp3").await }).unwrap_err().to_string().starts_with("Reading files without a server connection"));
    let m = signed_out.mode.clone();
    assert!(signed_out.run(async move { m.fetch_cover("a.mp3").await }).is_none());
}

#[test]
fn the_status_listener_hears_a_check_in_start_and_finish() {
    let f = fixture();
    f.sign_in();
    f.net.route("/sync/snapshot", &[(200, &snapshot_json(&[], "c1", None))]);
    let heard = Rc::new(RefCell::new(Vec::new()));
    let h = heard.clone();
    let id = f.mode.subscribe(move |s| h.borrow_mut().push(s.syncing));
    f.sync();
    assert_eq!(heard.borrow().first(), Some(&true));
    assert_eq!(heard.borrow().last(), Some(&false));
    f.mode.unsubscribe(id);
}
