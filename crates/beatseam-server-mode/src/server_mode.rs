use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};

use beatseam_api::auth::RoleDto;
use beatseam_api::lastfm::LastFmStatusDto;
use beatseam_api::mutations::MutationDto;
use beatseam_api::sync::SyncChangeDto;
use beatseam_api::client::{DeviceSignInProgress, DeviceSignInResult};
use beatseam_api::{ApiClient, ApiClientError, ApiClientOptions, LyricsDto, LyricsSaveLineDto, RootDto, RootKind as DtoRootKind, decode_track_key, encode_track_key};
use beatseam_core::file_access::{FileAccessError, FileRef, GrantedRoot, LyricsAnswer, RootKind};
use beatseam_core::http::HttpTransport;
use beatseam_core::library_store::{LibraryStore, PlaybackState, StoreError};
use beatseam_core::lyrics::{LyricLine, ParsedLyrics, Tags};
use beatseam_core::offline::{OfflineRuntime, ServerStatusSummary};
use beatseam_core::tasks::{AbortSignal, Gate};
use serde::{Deserialize, Serialize};

use crate::apply_sync::{ApplyContext, USER_PLAYLIST_ID_PREFIX, apply_change};
use crate::device_playback::{DEVICE_PLAYBACK_KEY, RemotePlayback, list_device_playback, playback_payload};
use crate::lyrics_editing::{EditedLine, LyricsEditError, LyricsSource, ServerLyricsEditing};
use crate::mutation_queue::MutationQueue;
use crate::server_covers::{CoverFetch, CoverFetchError};
use crate::server_file_access::{ServerLink, unsupported};
use crate::server_last_fm::SERVER_SCROBBLE_QUEUE_KEY;
use crate::server_url::normalize_server_url;

const CONFIG_KEY: &str = "serverMode.config";
const CURSOR_KEY: &str = "serverMode.cursor";
const ROOTS_KEY: &str = "serverMode.roots";
const SYNCED_AT_KEY: &str = "serverMode.syncedAtMs";
const DEVICE_ID_KEY: &str = "serverMode.deviceId";
/// This device's playback state goes to the server at most this often while it's just the position moving.
const PLAYBACK_PUSH_INTERVAL_MS: u64 = 20_000;
const LASTFM_STATUS_KEY: &str = "serverMode.lastFmStatus";
const LYRICS_CACHE_PREFIX: &str = "serverMode.lyrics:";
const LYRICS_INDEX_KEY: &str = "serverMode.lyricsIndex";
const MAX_CACHED_LYRICS: usize = 2000;
const PAGE_SIZE: u32 = 500;

/// The saved sign-in (JSON-compatible with the TypeScript app's record).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredConfig {
    /// Where the server is, e.g. https://music.example.com. Empty for the web app served by the server itself.
    url: String,
    /// A native app's device token. `None` on the web, which relies on its session cookie.
    token: Option<String>,
    user_name: Option<String>,
    /// "ADMIN" or "USER". Absent for a sign-in saved before this was recorded; refreshed at the next check-in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    /// The token lives in the platform's secret storage, not in this record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token_in_secret_store: Option<bool>,
}

fn role_name(role: RoleDto) -> String {
    if role == RoleDto::Admin { "ADMIN" } else { "USER" }.to_owned()
}

fn admin_flag(config: &Option<StoredConfig>) -> Option<bool> {
    config.as_ref().and_then(|c| c.role.as_ref()).map(|r| r == "ADMIN")
}

/// Hardware-backed storage for the device token (Android Keystore), so a copy of the app's data doesn't carry a working
/// sign-in.
pub trait SecretStore {
    /// `false` for a platform with no such storage: the token then stays with the rest of the settings.
    fn available(&self) -> bool {
        true
    }
    fn get(&self) -> impl Future<Output = Option<String>>;
    /// `None` removes it.
    fn set(&self, value: Option<&str>) -> impl Future<Output = ()>;
}

/// No secret storage.
pub struct NoSecretStore;

impl SecretStore for NoSecretStore {
    fn available(&self) -> bool {
        false
    }
    async fn get(&self) -> Option<String> {
        None
    }
    async fn set(&self, _value: Option<&str>) {}
}

#[derive(Clone, Debug, PartialEq)]
pub struct ServerStatus {
    /// There's a signed-in server this app takes its library from.
    pub active: bool,
    pub url: String,
    pub user_name: Option<String>,
    /// Whether the account is an admin; `None` until known.
    pub is_admin: Option<bool>,
    pub last_synced_at_ms: Option<f64>,
    pub syncing: bool,
    /// The last sync could not reach the server; the cached library is being shown.
    pub offline: bool,
    /// Why the last attempt failed, if it did for a reason other than being offline.
    pub error: Option<String>,
}

impl ServerStatus {
    /// What the offline library needs of this (see `OfflineLibrary::on_server_status`).
    pub fn summary(&self) -> ServerStatusSummary {
        ServerStatusSummary { offline: self.offline, active: self.active, last_synced_at_ms: self.last_synced_at_ms.map(|t| t as u64) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncKind {
    Snapshot,
    Delta,
    Offline,
    SignedOut,
    Failed,
    Inactive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncOutcome {
    pub kind: SyncKind,
    /// The local library was modified.
    pub changed: bool,
}

const fn outcome(kind: SyncKind, changed: bool) -> SyncOutcome {
    SyncOutcome { kind, changed }
}

/// Why a sign-in, report or similar operation failed.
#[derive(Clone, Debug, PartialEq)]
pub enum ServerModeError {
    /// A message meant for the person (a bad address, ...).
    Message(String),
    NotSignedIn,
    Api(ApiClientError),
    Store(StoreError),
}

impl fmt::Display for ServerModeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(m) => f.write_str(m),
            Self::NotSignedIn => f.write_str("Not signed in to a server"),
            Self::Api(e) => e.fmt(f),
            Self::Store(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ServerModeError {}

impl From<ApiClientError> for ServerModeError {
    fn from(e: ApiClientError) -> Self {
        Self::Api(e)
    }
}

impl From<StoreError> for ServerModeError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

/// What can go wrong in a check-in, before it is turned into an outcome.
enum SyncError {
    Api(ApiClientError),
    Store(StoreError),
}

impl From<ApiClientError> for SyncError {
    fn from(e: ApiClientError) -> Self {
        Self::Api(e)
    }
}

impl From<StoreError> for SyncError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

pub struct ServerModeDeps<S, T, R, K> {
    pub store: Arc<S>,
    pub transport: T,
    pub runtime: R,
    pub secret_store: K,
    /// Used to name this device when it asks to be signed in ("Pixel 8", "Windows PC").
    pub device_name: Box<dyn Fn() -> String>,
    /// A fresh random id (see [`random_id`](crate::random_id)): this device's id, and queued operations'.
    pub new_id: Box<dyn Fn() -> String>,
    /// The web app served by the server itself: same address, cookie instead of a token.
    pub same_origin: bool,
}

struct LastPush {
    track_file_id: Option<String>,
    playing: bool,
    at_ms: u64,
}

struct SyncFlight {
    done: Gate,
    result: Mutex<Option<SyncOutcome>>,
}

struct Inner {
    config: Option<StoredConfig>,
    /// Names this device to the server and to the other devices' "continue" lists; made once and kept.
    device_id: String,
    last_push: LastPush,
    pending_push: Option<(PlaybackState, bool)>,
    /// Bumped whenever the trailing-send timer is cancelled or fires, so a sleeping timer knows it is stale.
    push_epoch: u64,
    push_timer_armed: bool,
    cached_roots: Vec<GrantedRoot>,
    status: ServerStatus,
    current: Option<Arc<SyncFlight>>,
    listeners: Vec<(u64, Arc<dyn Fn(&ServerStatus)>)>,
    next_listener: u64,
}

struct Shared<S, T: HttpTransport, R, K> {
    store: Arc<S>,
    transport: T,
    runtime: Arc<R>,
    secret_store: K,
    device_name: Box<dyn Fn() -> String>,
    same_origin: bool,
    /// The current device token as the API client reads it on every request (separate so the client's closure is `Send`).
    token: Arc<Mutex<Option<String>>>,
    /// The client while signed in (what `getApi()` returns), read by the mutation queue too.
    api: Arc<Mutex<Option<Arc<ApiClient<T>>>>>,
    mutations: MutationQueue<S, ApiClient<T>>,
    inner: Mutex<Inner>,
}

/// Takes the app's library from a Beatseam server instead of scanning folders: holds the sign-in, checks in with the server
/// (a snapshot the first time, then what changed) and writes what it learns into the same library store the local scan
/// fills, so everything that reads the library keeps working. Offline is a normal state: the last synced library stays, and
/// the next check-in catches up. Cloning shares it.
pub struct ServerMode<S: LibraryStore + 'static, T: HttpTransport + Clone + 'static, R: OfflineRuntime + 'static, K: SecretStore + 'static> {
    shared: Arc<Shared<S, T, R, K>>,
}

impl<S: LibraryStore + 'static, T: HttpTransport + Clone + 'static, R: OfflineRuntime + 'static, K: SecretStore + 'static> Clone for ServerMode<S, T, R, K> {
    fn clone(&self) -> Self {
        Self { shared: self.shared.clone() }
    }
}

struct SyncEnd<'a, S: LibraryStore + 'static, T: HttpTransport + Clone + 'static, R: OfflineRuntime + 'static, K: SecretStore + 'static> {
    mode: &'a ServerMode<S, T, R, K>,
    flight: Arc<SyncFlight>,
}

impl<S: LibraryStore + 'static, T: HttpTransport + Clone + 'static, R: OfflineRuntime + 'static, K: SecretStore + 'static> Drop for SyncEnd<'_, S, T, R, K> {
    fn drop(&mut self) {
        self.mode.shared.inner.lock().unwrap().current = None;
        self.mode.set_status(|s| s.syncing = false);
        self.flight.done.open();
    }
}

fn encode_roots(roots: &[GrantedRoot]) -> String {
    serde_json::Value::Array(roots.iter().map(|r| serde_json::json!({"id": r.id, "displayName": r.display_name, "kind": "library", "removable": r.removable})).collect()).to_string()
}

fn decode_roots(raw: &str) -> Vec<GrantedRoot> {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(raw) else { return Vec::new() };
    items
        .iter()
        .filter_map(|v| {
            Some(GrantedRoot {
                id: v.get("id")?.as_str()?.to_owned(),
                display_name: v.get("displayName")?.as_str()?.to_owned(),
                kind: if v.get("kind")?.as_str()? == "lyrics" { RootKind::Lyrics } else { RootKind::Library },
                removable: v.get("removable").and_then(serde_json::Value::as_bool).unwrap_or(false),
            })
        })
        .collect()
}

/// The server's lyrics, in the shape the rest of the app reads.
pub fn lyrics_from_dto(dto: &LyricsDto) -> ParsedLyrics {
    let mut tags = Tags::new();
    for (key, value) in &dto.tags {
        tags.set(key.clone(), value.clone());
    }
    ParsedLyrics {
        synced: dto.synced,
        tags,
        lines: dto.lines.iter().map(|l| LyricLine { time_seconds: l.time_seconds, text: l.text.clone(), translation: l.translation.clone() }).collect(),
    }
}

fn note_seen(change: &SyncChangeDto, tracks: &mut HashSet<String>, playlists: &mut HashSet<String>) {
    match change {
        SyncChangeDto::Track { id, track: Some(_) } => tracks.extend(decode_track_key(id).ok()),
        SyncChangeDto::Playlist { id, playlist: Some(_) } => playlists.extend(decode_track_key(id).ok()),
        SyncChangeDto::UserPlaylist { id, user_playlist: Some(_) } => {
            playlists.insert(format!("{USER_PLAYLIST_ID_PREFIX}{id}"));
        }
        _ => {}
    }
}

impl<S: LibraryStore + 'static, T: HttpTransport + Clone + 'static, R: OfflineRuntime + 'static, K: SecretStore + 'static> ServerMode<S, T, R, K> {
    /// Reads the saved sign-in from local storage (fast, never the network).
    pub async fn open(deps: ServerModeDeps<S, T, R, K>) -> Result<Self, StoreError> {
        let ServerModeDeps { store, transport, runtime, secret_store, device_name, new_id, same_origin } = deps;
        let runtime = Arc::new(runtime);
        let api: Arc<Mutex<Option<Arc<ApiClient<T>>>>> = Arc::new(Mutex::new(None));
        let new_id: Arc<dyn Fn() -> String> = Arc::from(new_id);
        let (reader, spawner, queue_ids) = (api.clone(), runtime.clone(), new_id.clone());
        let mutations = MutationQueue::open(store.clone(), move || reader.lock().unwrap().clone(), move || queue_ids(), Box::new(move |future| spawner.spawn(future))).await?;
        let inner = Inner {
            config: None,
            device_id: String::new(),
            last_push: LastPush { track_file_id: None, playing: false, at_ms: 0 },
            pending_push: None,
            push_epoch: 0,
            push_timer_armed: false,
            cached_roots: Vec::new(),
            status: ServerStatus { active: false, url: String::new(), user_name: None, is_admin: None, last_synced_at_ms: None, syncing: false, offline: false, error: None },
            current: None,
            listeners: Vec::new(),
            next_listener: 0,
        };
        let mode = Self { shared: Arc::new(Shared { store, transport, runtime, secret_store, device_name, same_origin, token: Arc::new(Mutex::new(None)), api, mutations, inner: Mutex::new(inner) }) };
        mode.load_saved(&*new_id).await?;
        Ok(mode)
    }

    // ---------------------------------------------------------------- state

    pub fn is_active(&self) -> bool {
        self.shared.inner.lock().unwrap().status.active
    }

    pub fn get_status(&self) -> ServerStatus {
        self.shared.inner.lock().unwrap().status.clone()
    }

    /// Changes made here that the server hasn't seen yet; flushed at the start of every check-in.
    pub fn mutations(&self) -> &MutationQueue<S, ApiClient<T>> {
        &self.shared.mutations
    }

    /// Calls `listener` on every status change. Returns an id for [`unsubscribe`](Self::unsubscribe).
    pub fn subscribe(&self, listener: impl Fn(&ServerStatus) + 'static) -> u64 {
        let mut inner = self.shared.inner.lock().unwrap();
        inner.next_listener += 1;
        let id = inner.next_listener;
        inner.listeners.push((id, Arc::new(listener)));
        id
    }

    pub fn unsubscribe(&self, id: u64) {
        self.shared.inner.lock().unwrap().listeners.retain(|(i, _)| *i != id);
    }

    /// `true` for the web app served by the server itself: sign-in is the browser session (the account pages), not a
    /// device code.
    pub fn uses_browser_session(&self) -> bool {
        self.shared.same_origin
    }

    /// The server's library roots as last synced - no network.
    pub fn roots(&self) -> Vec<GrantedRoot> {
        self.shared.inner.lock().unwrap().cached_roots.clone()
    }

    /// The client, while signed in.
    pub fn get_api(&self) -> Option<Arc<ApiClient<T>>> {
        if self.is_active() { self.shared.api.lock().unwrap().clone() } else { None }
    }

    fn now_ms(&self) -> u64 {
        self.shared.runtime.now_ms()
    }

    fn set_status(&self, patch: impl FnOnce(&mut ServerStatus)) {
        let (status, listeners) = {
            let mut inner = self.shared.inner.lock().unwrap();
            patch(&mut inner.status);
            (inner.status.clone(), inner.listeners.iter().map(|(_, l)| l.clone()).collect::<Vec<_>>())
        };
        for listener in listeners {
            listener(&status);
        }
    }

    fn make_api(&self, url: &str) -> ApiClient<T> {
        let token = self.shared.token.clone();
        ApiClient::new(ApiClientOptions::new(url).with_token(move || token.lock().unwrap().clone()), self.shared.transport.clone())
    }

    /// Switches the in-memory sign-in (no storage, no status).
    fn apply_config(&self, config: Option<StoredConfig>) {
        *self.shared.token.lock().unwrap() = config.as_ref().and_then(|c| c.token.clone());
        *self.shared.api.lock().unwrap() = config.as_ref().map(|c| Arc::new(self.make_api(&c.url)));
        self.shared.inner.lock().unwrap().config = config;
    }

    async fn load_saved(&self, new_id: &dyn Fn() -> String) -> Result<(), StoreError> {
        let store = &self.shared.store;
        let raw_config = store.get_setting(CONFIG_KEY).await?;
        let raw_roots = store.get_setting(ROOTS_KEY).await?;
        let raw_synced = store.get_setting(SYNCED_AT_KEY).await?;
        let raw_device_id = store.get_setting(DEVICE_ID_KEY).await?.filter(|d| !d.is_empty());
        let device_id = raw_device_id.clone().unwrap_or_else(new_id);
        if raw_device_id.is_none() {
            store.put_setting(DEVICE_ID_KEY, &device_id).await?;
        }
        let mut config = None;
        // Unreadable: treat as signed out rather than crash on startup.
        if let Some(stored) = raw_config.as_deref().filter(|r| !r.is_empty()).and_then(|r| serde_json::from_str::<StoredConfig>(r).ok()) {
            config = self.reveal_token(stored).await;
        }
        self.apply_config(config.clone());
        let roots = raw_roots.as_deref().map(decode_roots).unwrap_or_default();
        let synced = raw_synced.as_deref().and_then(|s| s.parse::<f64>().ok());
        self.shared.inner.lock().unwrap().device_id = device_id;
        self.shared.inner.lock().unwrap().cached_roots = roots;
        self.set_status(|s| {
            s.active = config.is_some();
            s.url = config.as_ref().map(|c| c.url.clone()).unwrap_or_default();
            s.user_name = config.as_ref().and_then(|c| c.user_name.clone());
            s.is_admin = admin_flag(&config);
            s.last_synced_at_ms = synced;
        });
        Ok(())
    }

    /// Fills in the token from secret storage. A token still sitting in the settings from before there was a secret store
    /// is moved there. `None` (signed out) if the secret is gone, e.g. the Keystore key was wiped: a stored sign-in without
    /// its token can't do anything.
    async fn reveal_token(&self, stored: StoredConfig) -> Option<StoredConfig> {
        let secrets = &self.shared.secret_store;
        if !secrets.available() {
            return Some(stored);
        }
        if stored.token_in_secret_store == Some(true) {
            let token = secrets.get().await?;
            return Some(StoredConfig { token: Some(token), ..stored });
        }
        if let Some(token) = &stored.token {
            secrets.set(Some(token)).await;
            let moved = StoredConfig { token: None, token_in_secret_store: Some(true), ..stored.clone() };
            let _ = self.shared.store.put_setting(CONFIG_KEY, &serde_json::to_string(&moved).expect("config serializes")).await;
        }
        Some(stored)
    }

    async fn save_config(&self, config: Option<StoredConfig>) -> Result<(), StoreError> {
        self.apply_config(config.clone());
        let secrets = &self.shared.secret_store;
        let mut persisted = config.clone();
        if secrets.available() {
            // The secret goes in first, so a record never points at a token that was not written.
            secrets.set(config.as_ref().and_then(|c| c.token.as_deref())).await;
            if let Some(c) = config.as_ref().filter(|c| c.token.is_some()) {
                persisted = Some(StoredConfig { token: None, token_in_secret_store: Some(true), ..c.clone() });
            }
        }
        // `put_setting` has no delete; an empty value is read back as "none".
        self.shared.store.put_setting(CONFIG_KEY, &persisted.map(|p| serde_json::to_string(&p).expect("config serializes")).unwrap_or_default()).await?;
        self.set_status(|s| {
            s.active = config.is_some();
            s.url = config.as_ref().map(|c| c.url.clone()).unwrap_or_default();
            s.user_name = config.as_ref().and_then(|c| c.user_name.clone());
            s.is_admin = admin_flag(&config);
        });
        Ok(())
    }

    // ---------------------------------------------------------------- signing in and out

    /// Native sign-in: ask the server for a code, let the caller show it, and wait for the person to approve it in a
    /// browser. On approval the token is saved and the app switches to server mode (the caller then calls
    /// [`sync`](Self::sync)).
    pub async fn sign_in_with_device_code(&self, address: &str, on_progress: impl FnMut(DeviceSignInProgress), signal: Option<&AbortSignal>) -> Result<DeviceSignInResult, ServerModeError> {
        let url = normalize_server_url(address).map_err(ServerModeError::Message)?;
        let api = self.make_unauthenticated(&url);
        let result = api.sign_in_with_device_code(&(self.shared.device_name)(), on_progress, signal).await?;
        if let DeviceSignInResult::SignedIn { token, me } = &result {
            self.reset_local_copy().await?;
            self.save_config(Some(StoredConfig { url, token: Some(token.clone()), user_name: Some(me.display_name.clone()), role: Some(role_name(me.role)), token_in_secret_store: None })).await?;
            self.set_status(|s| {
                s.offline = false;
                s.error = None;
            });
        }
        Ok(result)
    }

    /// A client that sends no token (for talking to a server this device isn't signed in to yet).
    fn make_unauthenticated(&self, url: &str) -> ApiClient<T> {
        ApiClient::new(ApiClientOptions::new(url), self.shared.transport.clone())
    }

    /// Whether the page itself was served by a Beatseam server (the web app on a self-hosted server, as opposed to a static
    /// host): its public status answers. Says whether the first account still has to be created, too.
    pub async fn probe_same_origin(&self) -> Option<bool> {
        self.make_unauthenticated("").auth_status().await.ok().map(|s| s.setup_required)
    }

    /// The web app, served by the server itself: nothing to type, just whether the session cookie is signed in.
    pub async fn connect_same_origin(&self) -> Result<bool, StoreError> {
        let Ok(me) = self.make_unauthenticated("").me().await else { return Ok(false) };
        if !me.enrolled {
            return Ok(false);
        }
        let unchanged = self.shared.inner.lock().unwrap().config.as_ref().is_some_and(|c| c.url.is_empty() && c.user_name.as_deref() == Some(me.display_name.as_str()));
        if !unchanged {
            self.reset_local_copy().await?;
            self.save_config(Some(StoredConfig { url: String::new(), token: None, user_name: Some(me.display_name.clone()), role: Some(role_name(me.role)), token_in_secret_store: None })).await?;
        }
        Ok(true)
    }

    // ---------------------------------------------------------------- playback state

    /// Tells the server what this device is playing, so the person's other devices can offer to continue it. A new track
    /// goes out at once; while the same track just advances, at most every few seconds' worth, with a single trailing send
    /// (no polling: it fires once, after the last change).
    pub fn push_playback_state(&self, state: &PlaybackState, playing: bool) {
        if !self.is_active() || state.playlist_id.is_none() || state.current_track_file_id.is_none() {
            return;
        }
        let (send_now, arm_for) = {
            let mut inner = self.shared.inner.lock().unwrap();
            inner.pending_push = Some((state.clone(), playing));
            let since = self.now_ms().saturating_sub(inner.last_push.at_ms);
            // A new track, or starting/stopping, is what the other devices care about most: those don't wait.
            if state.current_track_file_id != inner.last_push.track_file_id || playing != inner.last_push.playing || since >= PLAYBACK_PUSH_INTERVAL_MS {
                (true, None)
            } else if !inner.push_timer_armed {
                inner.push_timer_armed = true;
                (false, Some((PLAYBACK_PUSH_INTERVAL_MS - since, inner.push_epoch)))
            } else {
                (false, None)
            }
        };
        if send_now {
            self.send_pending_playback();
        } else if let Some((delay, epoch)) = arm_for {
            let this = self.clone();
            self.shared.runtime.spawn(Box::pin(async move {
                this.shared.runtime.sleep(delay).await;
                if this.shared.inner.lock().unwrap().push_epoch == epoch {
                    this.send_pending_playback();
                }
            }));
        }
    }

    fn send_pending_playback(&self) {
        let mutation = {
            let mut inner = self.shared.inner.lock().unwrap();
            inner.push_epoch += 1; // a timer still sleeping is now stale
            inner.push_timer_armed = false;
            let Some((state, playing)) = inner.pending_push.take() else { return };
            if !inner.status.active {
                return;
            }
            let now = self.now_ms();
            inner.last_push = LastPush { track_file_id: state.current_track_file_id.clone(), playing, at_ms: now };
            MutationDto::PlaybackStateSet { op_id: String::new(), device_id: inner.device_id.clone(), state: playback_payload(&state, &(self.shared.device_name)(), now as f64, playing) }
        };
        let this = self.clone();
        self.shared.runtime.spawn(Box::pin(async move { this.shared.mutations.enqueue(mutation).await }));
    }

    /// What the person's other devices were playing, as of the last sync - most recent first. No network.
    pub async fn other_devices_playback(&self) -> Result<Vec<RemotePlayback>, StoreError> {
        list_device_playback(&*self.shared.store, self.now_ms() as f64).await
    }

    // ---------------------------------------------------------------- lyrics

    /// A track's lyrics from the server. Each answer (including "none") is kept locally, so lyrics keep working offline for
    /// everything played before; when the server can't be reached the kept copy is used.
    pub async fn lyrics_for(&self, file_id: &str) -> Option<LyricsDto> {
        let key = format!("{LYRICS_CACHE_PREFIX}{file_id}");
        if let Some(api) = self.get_api() {
            let fetched: Result<Option<LyricsDto>, ()> = async {
                let lyrics = api.lyrics(&encode_track_key(file_id), None).await.map_err(drop)?;
                let value = lyrics.as_ref().map_or("-".to_owned(), |l| serde_json::to_string(l).expect("lyrics serialize"));
                self.shared.store.put_setting(&key, &value).await.map_err(drop)?;
                self.remember_cached_lyrics(file_id).await.map_err(drop)?;
                Ok(lyrics)
            }
            .await;
            // Offline or unwell: fall back to what was kept.
            if let Ok(lyrics) = fetched {
                return lyrics;
            }
        }
        let kept = self.shared.store.get_setting(&key).await.ok().flatten()?;
        if kept.is_empty() || kept == "-" {
            return None;
        }
        serde_json::from_str(&kept).ok()
    }

    /// Flags a track's lyrics as wrong for an admin to review (any signed-in user).
    pub async fn report_lyrics(&self, track_file_id: &str, reason: Option<&str>) -> Result<(), ServerModeError> {
        let api = self.get_api().ok_or(ServerModeError::NotSignedIn)?;
        Ok(api.report_lyrics(&encode_track_key(track_file_id), reason).await?)
    }

    /// Where the user's Last.fm link stands, from the server. The last answer is kept, so scrobbling keeps going (and
    /// queueing) while the server can't be reached; `None` when it has never been fetched.
    pub async fn last_fm_status(&self) -> Option<LastFmStatusDto> {
        if let Some(api) = self.get_api() {
            if let Ok(status) = api.last_fm_status().await {
                let _ = self.cache_last_fm_status(&status).await;
                return Some(status);
            }
        }
        let kept = self.shared.store.get_setting(LASTFM_STATUS_KEY).await.ok().flatten()?;
        serde_json::from_str(&kept).ok()
    }

    pub async fn cache_last_fm_status(&self, status: &LastFmStatusDto) -> Result<(), StoreError> {
        self.shared.store.put_setting(LASTFM_STATUS_KEY, &serde_json::to_string(status).expect("status serializes")).await
    }

    /// Remembers which tracks have kept lyrics (the store can't list settings by prefix), newest last, capped.
    async fn remember_cached_lyrics(&self, file_id: &str) -> Result<(), StoreError> {
        let mut next: Vec<String> = self.cached_lyrics_index().await.into_iter().filter(|id| id != file_id).collect();
        next.push(file_id.to_owned());
        let excess = next.len().saturating_sub(MAX_CACHED_LYRICS);
        for evicted in next.drain(..excess) {
            self.shared.store.put_setting(&format!("{LYRICS_CACHE_PREFIX}{evicted}"), "").await?;
        }
        self.shared.store.put_setting(LYRICS_INDEX_KEY, &serde_json::to_string(&next).expect("ids serialize")).await
    }

    async fn cached_lyrics_index(&self) -> Vec<String> {
        let raw = self.shared.store.get_setting(LYRICS_INDEX_KEY).await.ok().flatten().filter(|r| !r.is_empty());
        match raw.map(|r| serde_json::from_str::<serde_json::Value>(&r)) {
            Some(Ok(serde_json::Value::Array(items))) => items.into_iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
            _ => Vec::new(),
        }
    }

    // ---------------------------------------------------------------- signing out

    /// Signs this device out (revoking its token on the server when reachable) and drops the synced library.
    pub async fn sign_out(&self) -> Result<(), StoreError> {
        // Cut the sign-in off server-side too, if the server can be reached (the session for a browser, the token for an
        // app); signing out must work offline regardless.
        let (browser_session, api) = {
            let inner = self.shared.inner.lock().unwrap();
            (inner.config.as_ref().is_some_and(|c| c.token.is_none()), self.shared.api.lock().unwrap().clone())
        };
        if let Some(api) = api {
            let _ = if browser_session { api.logout().await } else { api.sign_out_this_device().await };
        }
        self.save_config(None).await?;
        self.reset_local_copy().await?;
        self.set_status(|s| {
            s.last_synced_at_ms = None;
            s.offline = false;
            s.error = None;
        });
        Ok(())
    }

    /// Clears everything learned from a server, so a different server or user can't see the last one's library.
    async fn reset_local_copy(&self) -> Result<(), StoreError> {
        let store = &self.shared.store;
        let roots = std::mem::take(&mut self.shared.inner.lock().unwrap().cached_roots);
        for root in &roots {
            for track in store.list_tracks(&root.id).await? {
                store.delete_track(&track.file_id).await?;
            }
            for playlist in store.list_playlists(&root.id).await? {
                store.delete_playlist(&playlist.id).await?;
            }
        }
        // One account's lyrics aren't shown to the next.
        for file_id in self.cached_lyrics_index().await {
            store.put_setting(&format!("{LYRICS_CACHE_PREFIX}{file_id}"), "").await?;
        }
        store.put_setting(LYRICS_INDEX_KEY, "").await?;
        store.put_setting(SERVER_SCROBBLE_QUEUE_KEY, "").await?;
        store.put_setting(DEVICE_PLAYBACK_KEY, "").await?;
        {
            let mut inner = self.shared.inner.lock().unwrap();
            inner.pending_push = None;
            inner.push_epoch += 1;
            inner.push_timer_armed = false;
            inner.last_push = LastPush { track_file_id: None, playing: false, at_ms: 0 };
        }
        store.put_setting(LASTFM_STATUS_KEY, "").await?;
        // Changes queued under one account must never be sent to another.
        self.shared.mutations.clear().await;
        store.put_setting(CURSOR_KEY, "").await?;
        store.put_setting(ROOTS_KEY, "").await?;
        store.put_setting(SYNCED_AT_KEY, "").await?;
        Ok(())
    }

    // ---------------------------------------------------------------- the check-in

    /// Checks in with the server. Joins a check-in already running. Failures are in the outcome and the status.
    pub async fn sync(&self) -> SyncOutcome {
        let running = self.shared.inner.lock().unwrap().current.clone();
        if let Some(flight) = running {
            flight.done.wait().await;
            return flight.result.lock().unwrap().unwrap_or(outcome(SyncKind::Failed, false));
        }
        let flight = Arc::new(SyncFlight { done: Gate::new(), result: Mutex::new(None) });
        self.shared.inner.lock().unwrap().current = Some(flight.clone());
        let _end = SyncEnd { mode: self, flight: flight.clone() };
        let result = self.run_sync().await;
        *flight.result.lock().unwrap() = Some(result);
        result
    }

    /// A sign-in saved before the role was recorded learns it here; a failure just leaves it unknown for now.
    async fn refresh_role(&self, api: &ApiClient<T>) {
        let Some(config) = self.shared.inner.lock().unwrap().config.clone().filter(|c| c.role.is_none()) else { return };
        // Offline or unwell: the sync itself will say so.
        let Ok(me) = api.me().await else { return };
        let still_signed_in = self.shared.inner.lock().unwrap().config.is_some();
        if still_signed_in {
            let _ = self.save_config(Some(StoredConfig { role: Some(role_name(me.role)), ..config })).await;
        }
    }

    async fn run_sync(&self) -> SyncOutcome {
        let Some(api) = self.get_api() else { return outcome(SyncKind::Inactive, false) };
        self.set_status(|s| {
            s.syncing = true;
            s.error = None;
        });
        match self.check_in(&api).await {
            Ok(result) => result,
            Err(SyncError::Api(ApiClientError::Network(_))) => {
                self.set_status(|s| s.offline = true);
                outcome(SyncKind::Offline, false)
            }
            Err(SyncError::Api(ApiClientError::Api(error))) if error.status == 401 => {
                // Revoked or expired: stop pretending, but keep the library visible until the person signs in again.
                let _ = self.save_config(None).await;
                self.set_status(|s| s.error = Some("You were signed out by the server. Sign in again to keep syncing.".into()));
                outcome(SyncKind::SignedOut, false)
            }
            Err(SyncError::Api(error)) => {
                self.set_status(|s| s.error = Some(error.to_string()));
                outcome(SyncKind::Failed, false)
            }
            Err(SyncError::Store(error)) => {
                self.set_status(|s| s.error = Some(error.to_string()));
                outcome(SyncKind::Failed, false)
            }
        }
    }

    async fn check_in(&self, api: &ApiClient<T>) -> Result<SyncOutcome, SyncError> {
        self.refresh_role(api).await;
        // Our own changes go first, so the version of them the server hands back is what we then pull.
        self.shared.mutations.flush().await;
        let cursor = self.shared.store.get_setting(CURSOR_KEY).await?.filter(|c| !c.is_empty());
        let result = match cursor {
            None => self.snapshot(api).await?,
            Some(cursor) => match self.deltas(api, cursor).await {
                Err(SyncError::Api(ApiClientError::ResyncRequired(_))) => self.snapshot(api).await?,
                other => other?,
            },
        };
        let synced_at = self.now_ms() as f64;
        self.shared.store.put_setting(SYNCED_AT_KEY, &beatseam_core::js_compat::js_number(synced_at)).await?;
        self.set_status(|s| {
            s.last_synced_at_ms = Some(synced_at);
            s.offline = false;
        });
        Ok(result)
    }

    async fn apply(&self, change: &SyncChangeDto) -> Result<bool, StoreError> {
        let (root, device_id) = {
            let inner = self.shared.inner.lock().unwrap();
            (inner.cached_roots.first().map(|r| r.id.clone()), inner.device_id.clone())
        };
        let runtime = self.shared.runtime.clone();
        let ctx = ApplyContext { store: &*self.shared.store, default_root_id: &move || root.clone(), now: &move || runtime.now_ms() as f64, device_id: &device_id };
        apply_change(&ctx, change).await
    }

    async fn set_roots(&self, roots: &[RootDto]) -> Result<(), StoreError> {
        // Lyrics-only roots are not music libraries (and lyrics aren't served to the app yet).
        let cached: Vec<GrantedRoot> = roots.iter().filter(|r| r.kind == DtoRootKind::Library).map(|r| GrantedRoot { id: r.id.clone(), display_name: r.display_name.clone(), kind: RootKind::Library, removable: false }).collect();
        self.shared.store.put_setting(ROOTS_KEY, &encode_roots(&cached)).await?;
        self.shared.inner.lock().unwrap().cached_roots = cached;
        Ok(())
    }

    /// The first sync, or after the server said our place in its history is gone: take everything, then drop what no longer
    /// exists.
    async fn snapshot(&self, api: &ApiClient<T>) -> Result<SyncOutcome, SyncError> {
        let store = &self.shared.store;
        let (mut seen_tracks, mut seen_playlists) = (HashSet::new(), HashSet::new());
        let mut after: Option<String> = None;
        let mut start_cursor: Option<String> = None;
        loop {
            let page = api.sync_snapshot(after.as_deref(), Some(PAGE_SIZE)).await?;
            if start_cursor.is_none() {
                start_cursor = Some(page.cursor.clone());
                self.set_roots(&page.roots).await?;
            }
            for entity in &page.entities {
                self.apply(entity).await?;
                note_seen(entity, &mut seen_tracks, &mut seen_playlists);
            }
            after = page.next.filter(|n| !n.is_empty());
            if after.is_none() {
                break;
            }
        }

        let root_ids: HashSet<String> = self.shared.inner.lock().unwrap().cached_roots.iter().map(|r| r.id.clone()).collect();
        for track in store.list_all_tracks().await? {
            if root_ids.contains(&track.root_id) && !seen_tracks.contains(&track.file_id) {
                store.delete_track(&track.file_id).await?;
            }
        }
        for playlist in store.list_all_playlists().await? {
            if root_ids.contains(&playlist.root_id) && !seen_playlists.contains(&playlist.id) {
                store.delete_playlist(&playlist.id).await?;
            }
        }
        store.put_setting(CURSOR_KEY, &start_cursor.unwrap_or_else(|| "0".into())).await?;
        Ok(outcome(SyncKind::Snapshot, true))
    }

    async fn counts_agree(&self, tracks: u64, playlists: u64) -> Result<bool, StoreError> {
        let root_ids: HashSet<String> = self.shared.inner.lock().unwrap().cached_roots.iter().map(|r| r.id.clone()).collect();
        let held_tracks = self.shared.store.list_all_tracks().await?.iter().filter(|t| root_ids.contains(&t.root_id)).count() as u64;
        // The user's own playlists live in the same store but are not part of the server's shared-playlist total.
        let held_playlists = self.shared.store.list_all_playlists().await?.iter().filter(|p| root_ids.contains(&p.root_id) && !p.id.starts_with(USER_PLAYLIST_ID_PREFIX)).count() as u64;
        Ok(held_tracks == tracks && held_playlists == playlists)
    }

    async fn deltas(&self, api: &ApiClient<T>, since: String) -> Result<SyncOutcome, SyncError> {
        let mut cursor = since;
        let mut changed = false;
        loop {
            let page = api.sync_changes(&cursor, Some(PAGE_SIZE)).await?;
            self.set_roots(&page.roots).await?;
            for change in &page.changes {
                if self.apply(change).await? {
                    changed = true;
                }
            }
            cursor = page.cursor.clone();
            // Saved page by page, so an interrupted check-in resumes instead of starting over.
            self.shared.store.put_setting(CURSOR_KEY, &cursor).await?;
            if !page.has_more {
                // A change feed can't put back what this device lost (a bug, cleared storage, a crash mid-write). If the
                // counts no longer agree with the server's, start over from a snapshot.
                if !self.counts_agree(page.totals.tracks, page.totals.playlists).await? {
                    return self.snapshot(api).await;
                }
                return Ok(outcome(SyncKind::Delta, changed));
            }
        }
    }
}

// ---------------------------------------------------------------- what other modules read this through

impl<S: LibraryStore + 'static, T: HttpTransport + Clone + 'static, R: OfflineRuntime + 'static, K: SecretStore + 'static> ServerLink for ServerMode<S, T, R, K> {
    fn is_active(&self) -> bool {
        ServerMode::is_active(self)
    }

    fn roots(&self) -> Vec<GrantedRoot> {
        ServerMode::roots(self)
    }

    async fn stream_bytes(&self, file_id: &str) -> Result<Vec<u8>, FileAccessError> {
        let api = self.get_api().ok_or_else(|| unsupported("Reading files without a server connection"))?;
        api.fetch_bytes(&api.stream_path_for_file_id(file_id), None, None).await.map_err(|e| FileAccessError::Io(e.to_string()))
    }

    async fn lyrics_for(&self, track_file_id: &str) -> Result<LyricsAnswer, FileAccessError> {
        Ok(match ServerMode::lyrics_for(self, track_file_id).await {
            Some(dto) => LyricsAnswer::Lyrics(lyrics_from_dto(&dto)),
            None => LyricsAnswer::NoLyrics,
        })
    }
}

impl<S: LibraryStore + 'static, T: HttpTransport + Clone + 'static, R: OfflineRuntime + 'static, K: SecretStore + 'static> CoverFetch for ServerMode<S, T, R, K> {
    async fn fetch_cover(&self, file_id: &str) -> Option<Result<(Vec<u8>, String), CoverFetchError>> {
        self.get_api()?.fetch_cover(file_id).await
    }
}

impl<S: LibraryStore + 'static, T: HttpTransport + Clone + 'static, R: OfflineRuntime + 'static, K: SecretStore + 'static> ServerLyricsEditing for ServerMode<S, T, R, K> {
    async fn source(&self, track_file_id: &str) -> Result<LyricsSource, LyricsEditError> {
        let api = self.get_api().ok_or(LyricsEditError::NotSignedIn)?;
        let source = api.lyrics_source(&encode_track_key(track_file_id)).await?;
        Ok(LyricsSource {
            assigned: source.assigned,
            candidates: source.candidates.into_iter().map(|c| FileRef { id: c.id, name: c.name, relative_path: c.relative_path, size_bytes: 0, last_modified_ms: 0.0 }).collect(),
        })
    }

    async fn assign(&self, track_file_id: &str, lrc_file_id: Option<&str>) -> Result<(), LyricsEditError> {
        let api = self.get_api().ok_or(LyricsEditError::NotSignedIn)?;
        api.assign_lyrics(&encode_track_key(track_file_id), lrc_file_id).await?;
        // What this device shows (the lyrics indicator, the next load) follows at once; the sync will confirm it.
        self.shared.store.put_lyrics_assignment(track_file_id, lrc_file_id.map(str::to_owned)).await?;
        Ok(())
    }

    async fn save(&self, track_file_id: &str, lines: &[EditedLine]) -> Result<(), LyricsEditError> {
        let api = self.get_api().ok_or(LyricsEditError::NotSignedIn)?;
        let lines = lines.iter().map(|l| LyricsSaveLineDto { time_seconds: l.time_seconds, text: l.text.clone() }).collect();
        api.save_lyrics(&encode_track_key(track_file_id), lines).await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "server_mode_tests.rs"]
mod server_mode_tests;
