//! The thunks of `packages/core/src/store`: the async operations that read services and drive the slices - startup restore, library
//! refresh, settings and playback persistence.

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt::Display;
use std::rc::Rc;
use std::sync::Arc;

use beatseam_core::file_access::{FileAccess, FileAccessError, GrantedRoot, RootKind};
use beatseam_core::library_scan::{LibraryError, LoadRootLibraryOptions, load_root_library};
use beatseam_core::library_store::{LibraryStore, LyricsScope, PlaybackState, PlaylistRecord, TrackRecord};
use beatseam_core::lyrics::{LYRICS_MATCHED_COUNT_SETTING_KEY, LyricsLoader};
use beatseam_core::playback::{PlaylistOptions, PlaylistPlayer};
use beatseam_core::audio_engine::AudioEngine;
use beatseam_core::tasks::Runtime;
use futures_util::future::join_all;

use crate::library::{LibraryRootData, RootWithLibrary};
use crate::live_playback::LivePlayback;
use crate::navigation::PlaylistContext;
use crate::persistence::PlaybackPatch;
use crate::restore::RestoringStep;
use crate::settings::{SettingsPatch, ThemeMode, persist_patch, read_stored_settings};
use crate::store::Store;
use crate::{AppState, CoarsePlayback};

fn message(error: impl Display) -> String {
    error.to_string()
}

/// How often to persist the playing position while a track is playing - frequent enough that a crash loses little progress,
/// infrequent enough not to hammer the store on every tick.
pub const POSITION_PERSIST_INTERVAL_MS: u64 = 5000;
/// Debounce for a rapidly-changing value (currently just volume, dragged via a slider) - long enough to coalesce a drag's burst into
/// one write, short enough that a quick tap still saves within a beat.
pub const RAPID_PLAYBACK_PATCH_DEBOUNCE_MS: u64 = 400;

/// Callbacks of the background lyrics-match pass.
#[derive(Clone)]
pub struct LyricsMatchCallbacks {
    /// Matched-so-far count, after each track.
    pub on_progress: Rc<dyn Fn(usize)>,
    /// A processed track turned out to have no match.
    pub on_anomaly: Rc<dyn Fn()>,
    /// The final matched count, once the whole pass is done.
    pub on_settled: Rc<dyn Fn(usize)>,
}

/// The background passes a refresh starts. Composed once per app from the concrete services (the scan coordinator, the queued
/// metadata and lyrics passes, the server connection); a fake in tests records what was asked of it.
pub trait LibraryJobs {
    /// Scans one root's folders, sharing the work with any scan of it already running.
    fn scan_root(&self, root_id: &str, display_name: &str) -> impl std::future::Future<Output = Result<(), LibraryError>>;
    /// Fire-and-forget: self-heals installs from before removing a root cleaned up after itself, never awaited, so a large
    /// orphaned-data scan can't delay the rest of a refresh or the restoring screen.
    fn prune_orphans(&self, granted_root_ids: Vec<String>);
    /// Fire-and-forget, idle-chunked: auto-assigns any track without a lyrics match (a manual override is never overwritten).
    fn match_lyrics(&self, scopes: Vec<LyricsScope>, tracks: Vec<TrackRecord>, callbacks: LyricsMatchCallbacks, priority_file_ids: Rc<dyn Fn() -> Vec<String>>);
    /// Fire-and-forget: fills in real titles/artists as it goes instead of blocking the library screen on reading every file's tag
    /// bytes. Cheap to repeat - fresh tracks are skipped.
    fn scan_metadata(&self, tracks: Vec<TrackRecord>, priority_file_ids: Rc<dyn Fn() -> Vec<String>>);
    /// Checks in with the server. `Some(changed)` once it did, `None` when there is no server connection at all.
    fn sync_server(&self) -> impl std::future::Future<Output = Option<bool>>;
}

pub struct AppConfig<F, BF, S, E: AudioEngine + 'static, R: Runtime + 'static, J> {
    pub store: Rc<Store<AppState>>,
    /// File access for work with a user gesture behind it.
    pub file_access: Arc<F>,
    /// File access for work with no user gesture behind it (startup refresh, background scans), which must never trigger a
    /// permission prompt. The same object as `file_access` on platforms without that permission model.
    pub background_file_access: Arc<BF>,
    pub library_store: Arc<S>,
    pub player: PlaylistPlayer<E, R>,
    pub live: Rc<LivePlayback>,
    pub jobs: Rc<J>,
    pub runtime: Arc<R>,
    /// Whether the library currently comes from a server.
    pub server_active: Rc<dyn Fn() -> bool>,
    /// Roots that refresh always rescans, not just when their cache is empty (the self-hosted server's: its files change on disk
    /// with no app interaction).
    pub always_rescan_root: Rc<dyn Fn(&str) -> bool>,
    /// Tells the server what this device is playing (called after each persisted playback write; no-op when signed out).
    pub on_playback_persisted: Option<Rc<dyn Fn(&PlaybackState)>>,
    /// Queues an account-wide setting change for the server (`setting.set`), called for each synced key written while signed in.
    pub on_synced_setting: Option<Rc<dyn Fn(&str, &str)>>,
}

struct Inner<F, BF, S, E: AudioEngine + 'static, R: Runtime + 'static, J> {
    config: AppConfig<F, BF, S, E, R, J>,
    lyrics: LyricsLoader<BF, S>,
    /// Bumped by every playback patch: a delayed write that finds it moved on gives up (a newer patch's own write carries the full
    /// merged state, so nothing is lost).
    write_epoch: Cell<u64>,
}

/// The services and the store together: every async operation the screens trigger is a method on this. Cloning shares it.
pub struct App<F, BF, S, E: AudioEngine + 'static, R: Runtime + 'static, J> {
    inner: Rc<Inner<F, BF, S, E, R, J>>,
}

impl<F, BF, S, E: AudioEngine + 'static, R: Runtime + 'static, J> Clone for App<F, BF, S, E, R, J> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

impl<F, BF, S, E, R, J> App<F, BF, S, E, R, J>
where
    F: FileAccess + 'static,
    BF: FileAccess + 'static,
    S: LibraryStore + 'static,
    E: AudioEngine + 'static,
    R: Runtime + 'static,
    J: LibraryJobs + 'static,
{
    pub fn new(config: AppConfig<F, BF, S, E, R, J>) -> Self {
        let clock = config.runtime.clone();
        let lyrics = LyricsLoader::new(config.background_file_access.clone(), config.library_store.clone(), move || clock.now_ms());
        // Mirror the player's coarse state into the store - only when it actually changed, so the ~5 Hz position ticks never touch it.
        let store = config.store.clone();
        config.live.subscribe(move |state| {
            let changed = Rc::new(Cell::new(false));
            let c = changed.clone();
            let incoming = CoarsePlayback::from(state);
            if store.read(|s| s.playback.coarse != incoming) {
                store.dispatch(move |s| {
                    c.set(s.playback.sync_if_changed_to(incoming));
                });
            }
        });
        Self { inner: Rc::new(Inner { config, lyrics, write_epoch: Cell::new(0) }) }
    }

    pub fn store(&self) -> &Rc<Store<AppState>> {
        &self.inner.config.store
    }

    fn cfg(&self) -> &AppConfig<F, BF, S, E, R, J> {
        &self.inner.config
    }

    fn spawn(&self, future: impl std::future::Future<Output = ()> + 'static) {
        self.cfg().runtime.spawn(Box::pin(future));
    }

    /// Re-reads the player and notifies the live-state subscribers (which mirror the coarse part into the store).
    pub fn publish_live(&self) -> beatseam_core::playback::PlaylistPlayerState {
        self.cfg().live.publish(self.cfg().player.get_state())
    }

    // ---------------------------------------------------------------- settings

    /// Reads every persisted setting and hydrates the slice. `system_theme_mode` is the first-run theme default (nothing stored yet).
    pub async fn load_settings(&self, system_theme_mode: ThemeMode) -> Result<(), String> {
        let stored = read_stored_settings(&*self.cfg().library_store).await.map_err(message)?;
        self.cfg().store.dispatch(move |s| s.settings.loaded(&stored, system_theme_mode));
        // The player is a long-lived service built before the store, so a loaded setting has to be pushed into it.
        self.cfg().player.set_crossfade_seconds(self.cfg().store.read(|s| s.settings.values.crossfade_seconds));
        Ok(())
    }

    /// A user/auth-flow change: applies it, writes the patched keys, queues the account-wide ones for the server, and pushes a changed
    /// crossfade time into the player.
    pub fn update_settings(&self, patch: SettingsPatch) {
        let crossfade = patch.crossfade_seconds;
        let applied = patch.clone();
        self.cfg().store.dispatch(move |s| s.settings.updated(&applied));
        if let Some(seconds) = crossfade {
            self.cfg().player.set_crossfade_seconds(seconds);
        }
        let this = self.clone();
        self.spawn(async move {
            if let Ok(synced) = persist_patch(&*this.cfg().library_store, &patch).await {
                if (this.cfg().server_active)() {
                    if let Some(hook) = &this.cfg().on_synced_setting {
                        for (key, value) in synced {
                            hook(key, &value);
                        }
                    }
                }
            }
        });
    }

    pub fn reset_settings(&self, system_theme_mode: ThemeMode) {
        self.update_settings(crate::settings::reset_patch(system_theme_mode));
    }

    // ---------------------------------------------------------------- playback persistence

    /// Merges a change into the persisted playback state and writes it. `debounce_ms` delays only the write (coalescing e.g. a volume
    /// drag's burst); any newer patch cancels a pending delayed write first, and its own write carries the full merged state, so
    /// nothing is lost. A no-op before the startup restore has read what is actually persisted.
    pub fn patch_playback(&self, patch: PlaybackPatch, debounce_ms: u64) {
        let applied = Rc::new(Cell::new(false));
        let a = applied.clone();
        self.cfg().store.dispatch(move |s| a.set(s.persistence.playback_patched(&patch)));
        if !applied.get() {
            return;
        }
        let epoch = self.inner.write_epoch.get() + 1;
        self.inner.write_epoch.set(epoch);
        let this = self.clone();
        self.spawn(async move {
            if debounce_ms > 0 {
                this.cfg().runtime.sleep(debounce_ms).await;
                if this.inner.write_epoch.get() != epoch {
                    return;
                }
            }
            let state = this.cfg().store.read(|s| s.persistence.state.clone());
            if this.cfg().library_store.put_playback_state(state.clone()).await.is_ok() {
                // Signed in to a server: the person's other devices can offer to continue this.
                if let Some(hook) = &this.cfg().on_playback_persisted {
                    hook(&state);
                }
            }
        });
    }

    /// Persists the playing position, at most once per [`POSITION_PERSIST_INTERVAL_MS`], and only while a track is playing or paused.
    /// Called on every ticker tick.
    pub fn persist_position_if_due(&self, player: &beatseam_core::playback::PlaylistPlayerState) {
        use beatseam_core::playback::TrackPlayerStatus::{Paused, Playing};
        let at = self.cfg().runtime.now_ms();
        if player.current_file_id.is_none() || !matches!(player.track.status, Playing | Paused) {
            return;
        }
        if at.saturating_sub(self.cfg().store.read(|s| s.persistence.last_position_persist_at)) < POSITION_PERSIST_INTERVAL_MS {
            return;
        }
        self.cfg().store.dispatch(move |s| s.persistence.position_persist_stamped(at));
        self.patch_playback(PlaybackPatch { position_seconds: Some(player.track.position_seconds), ..Default::default() }, 0);
    }

    /// The player moved on without a manual action (a crossfade completed, or a track ended naturally) - refresh the live state right
    /// away rather than waiting up to one tick for the "now playing" display to notice, and persist where it landed. Also re-persists
    /// the shuffle order, which can be silently regenerated mid-playback (loop-all wrapping past the last track).
    pub fn player_advanced(&self) {
        let state = self.publish_live();
        let Some(current) = state.current_file_id else { return };
        self.patch_playback(
            PlaybackPatch { current_track_file_id: Some(Some(current)), position_seconds: Some(state.track.position_seconds), shuffle_order: Some(self.cfg().player.get_shuffle_order()), ..Default::default() },
            0,
        );
    }

    // ---------------------------------------------------------------- the startup restore

    /// Restores the last playback state on launch: loads the matching playlist into the player, seeks to the saved position, and
    /// leaves it paused (never autoplays on a fresh launch). Resolves once the screen-critical part is done; the rest of the
    /// library loads in the background ([`refresh_library`](Self::refresh_library)). `stored` is the persisted playback state,
    /// fetched at launch (the same fetch the player's initial-volume gate waits on - one fetch, not two racing each other).
    ///
    /// Startup-latency rule: this may only block on the last-played root/playlist and the current track's lyrics assignment. Every
    /// other root, library-wide lyrics matching and metadata scanning stays background work that live-merges in afterward - don't
    /// add to what this awaits.
    ///
    /// Races it guards: `user_took_over` (see [`RestoreState`](crate::RestoreState)) and `is_cancelled`, re-checked after each
    /// await and again right before touching the player. The decode itself is deliberately NOT awaited here - only the position
    /// seek and the live-state publish wait on it.
    pub async fn restore_playback(&self, stored: Option<PlaybackState>, is_cancelled: Rc<dyn Fn() -> bool>) {
        let outcome = self.run_restore(stored, &is_cancelled).await;
        if let Err(error) = outcome {
            self.cfg().store.dispatch(move |s| s.library.library_error_set(Some(error)));
        }
        if !is_cancelled() {
            self.cfg().store.dispatch(|s| s.restore.finished());
        }
    }

    fn took_over(&self) -> bool {
        self.cfg().store.read(|s| s.restore.user_took_over)
    }

    fn step(&self, step: RestoringStep) {
        self.cfg().store.dispatch(move |s| s.restore.step_advanced(step));
    }

    async fn run_restore(&self, stored: Option<PlaybackState>, is_cancelled: &Rc<dyn Fn() -> bool>) -> Result<(), String> {
        let cfg = self.cfg();
        self.step(RestoringStep::ListingFolders);
        // In server mode the cached library is all this needs - the sign-in was read from local storage only, never the network, so a
        // server being down or slow cannot hold up startup.
        let load_options = LoadRootLibraryOptions { scan_if_empty: !(cfg.server_active)() };
        self.step(RestoringStep::RestoringPlayback);
        if is_cancelled() {
            return Ok(());
        }
        // From here on the persisted state is real (or a legitimate "nothing was ever saved" default), so it's safe for patches to
        // merge onto it and write it back - see `PersistenceState::ready`.
        let read = stored.clone();
        cfg.store.dispatch(move |s| s.persistence.playback_state_read(read));
        // Idempotent with the player's own initial-volume gate on the same value - this one makes the displayed volume (gated on
        // restoring) pick up the real value too, not just the player's internal gain.
        if let Some(stored) = &stored {
            cfg.player.set_volume(stored.volume);
        }

        // Restore only the one root/playlist/track needed to reach a playable screen - everything else is deferred to refresh.
        if let Some(stored) = stored.as_ref().filter(|s| s.playlist_id.is_some() && s.current_track_file_id.is_some()) {
            let playlist_id = stored.playlist_id.clone().expect("filtered");
            let track_id = stored.current_track_file_id.clone().expect("filtered");
            let roots = cfg.file_access.list_granted_roots().await.map_err(message)?;
            if is_cancelled() {
                return Ok(());
            }
            let target_root = match stored.root_id.as_ref().and_then(|id| roots.iter().find(|r| &r.id == id)) {
                Some(root) => Some(root.clone()),
                // Older persisted state (no root id yet) or a stale/removed root - fall back to a linear search over each granted
                // root's already-cached playlists. Still cheap: a plain store read, never a scan.
                None => self.find_root_with_playlist(&roots, &playlist_id, is_cancelled).await?,
            };
            if is_cancelled() {
                return Ok(());
            }
            if let Some(target_root) = target_root {
                self.step(RestoringStep::ScanningLibrary);
                // The background file access, not the interactive one: no user gesture backs this automatic startup path, so a lapsed
                // browser grant can't be re-requested. A permission-pending root just fails to resume this session (same as a
                // not-found playlist/track) rather than aborting the rest of the restore.
                let (playlists, tracks_by_id) = self.load_root_tolerating_pending(&target_root, load_options).await?;
                if is_cancelled() {
                    return Ok(());
                }
                let playlist = playlists.iter().find(|p| p.id == playlist_id).cloned();
                let track = playlist.as_ref().and_then(|_| tracks_by_id.get(&track_id)).cloned();

                // The person already took a manual playback action while everything above was in flight - applying this now would
                // silently stomp their choice. Bails out of resuming a playable screen only; the refresh below (every other root,
                // lyrics auto-match, metadata) is unaffected and still runs.
                if let (Some(playlist), Some(track), false) = (playlist, track, self.took_over()) {
                    let scopes = cfg.library_store.get_lyrics_scopes().await.map_err(message)?;
                    if is_cancelled() {
                        return Ok(());
                    }
                    let (known, loaded_scopes) = (!scopes.is_empty(), scopes.clone());
                    cfg.store.dispatch(move |s| {
                        s.library.lyrics_scopes_loaded(loaded_scopes);
                        s.restore.lyrics_scopes_known(known);
                    });
                    if known {
                        self.step(RestoringStep::ScanningLyrics);
                        let track_name = track.relative_path.rsplit('/').next().unwrap_or(&track.relative_path).to_owned();
                        // No user gesture backs this call either: a lapsed grant skips resolving this track's lyrics assignment this
                        // run (the loader reports that as success) rather than aborting the rest of the restore.
                        self.inner.lyrics.ensure_lyrics_assignment(&scopes, &track.file_id, &track_name).await.map_err(message)?;
                        if is_cancelled() {
                            return Ok(());
                        }
                    }

                    // Re-checked here, not just above - the lyrics await is a real gap where the person's own action could land.
                    if !self.took_over() {
                        let playing = PlaylistContext { root: target_root.clone(), playlist: playlist.clone(), tracks: tracks_by_id.values().cloned().collect() };
                        // Before the player loads it: the player resolves tracks through the playing context, possibly synchronously from
                        // its first decode.
                        let ctx = playing.clone();
                        cfg.store.dispatch(move |s| s.navigation.playing_context_set(Some(ctx)));
                        cfg.player.set_shuffle(stored.shuffle_enabled);
                        cfg.player.set_loop_mode(stored.loop_mode);
                        self.step(RestoringStep::LoadingPlaylist);
                        // Loading (unlike a play request) decodes without starting playback. Not awaited: decoding (real file I/O +
                        // codec work) isn't needed to show the restored screen, only to play/seek it - the ticker picks up the loading
                        // to paused transition as it completes, like any other track load.
                        let load = {
                            let this = self.clone();
                            let (ids, start, options) = (playlist.track_file_ids.clone(), track_id.clone(), PlaylistOptions { playlist_id: Some(playlist.id.clone()), shuffle_order: stored.shuffle_order.clone() });
                            let (cancelled, position) = (is_cancelled.clone(), stored.position_seconds);
                            async move {
                                this.cfg().player.load_playlist(ids, Some(&start), options).await;
                                // One more recheck: the decode can take real time - if the person acted during it, the player's own play
                                // token already kept the audio correct, but this must not overwrite the UI back to this stale restore's state.
                                if cancelled() || this.took_over() {
                                    return;
                                }
                                if position > 0.0 {
                                    this.cfg().player.seek(position);
                                }
                                this.publish_live();
                            }
                        };
                        self.spawn(load);

                        let opened = self.resolve_opened_playlist(stored, &roots, &target_root, &playlist, &tracks_by_id, load_options, is_cancelled).await?;
                        if is_cancelled() {
                            return Ok(());
                        }
                        let now_playing_open = stored.now_playing_open;
                        cfg.store.dispatch(move |s| s.navigation.playback_restored(opened, now_playing_open, playing));
                    }
                }
            }
        }

        // Fire-and-forget: every other root, the full-library lyrics auto-match and metadata, now that they're off the critical path.
        let this = self.clone();
        self.spawn(async move {
            if let Err(error) = this.refresh_library(true).await {
                this.cfg().store.dispatch(move |s| s.library.library_error_set(Some(error)));
            }
        });
        Ok(())
    }

    async fn find_root_with_playlist(&self, roots: &[GrantedRoot], playlist_id: &str, is_cancelled: &Rc<dyn Fn() -> bool>) -> Result<Option<GrantedRoot>, String> {
        for root in roots {
            let playlists = self.cfg().library_store.list_playlists(&root.id).await.map_err(message)?;
            if is_cancelled() {
                return Ok(None);
            }
            if playlists.iter().any(|p| p.id == playlist_id) {
                return Ok(Some(root.clone()));
            }
        }
        Ok(None)
    }

    async fn load_root_tolerating_pending(&self, root: &GrantedRoot, options: LoadRootLibraryOptions) -> Result<(Vec<PlaylistRecord>, HashMap<String, TrackRecord>), String> {
        match load_root_library(&*self.cfg().background_file_access, &*self.cfg().library_store, root.clone(), options).await {
            Ok(loaded) => Ok((loaded.playlists, loaded.tracks_by_id)),
            Err(LibraryError::FileAccess(FileAccessError::PermissionPending { .. })) => Ok((Vec::new(), HashMap::new())),
            Err(other) => Err(message(other)),
        }
    }

    /// Which playlist screen to show as "open" - see `PlaybackState::opened_playlist_id`. State persisted before the field existed
    /// falls back to the playing playlist, matching the pre-field behaviour.
    #[allow(clippy::too_many_arguments)]
    async fn resolve_opened_playlist(
        &self,
        stored: &PlaybackState,
        roots: &[GrantedRoot],
        target_root: &GrantedRoot,
        playlist: &PlaylistRecord,
        tracks_by_id: &HashMap<String, TrackRecord>,
        load_options: LoadRootLibraryOptions,
        is_cancelled: &Rc<dyn Fn() -> bool>,
    ) -> Result<Option<PlaylistContext>, String> {
        use beatseam_core::library_store::OpenedPlaylist;
        let playing = || PlaylistContext { root: target_root.clone(), playlist: playlist.clone(), tracks: tracks_by_id.values().cloned().collect() };
        let opened_id = match &stored.opened_playlist_id {
            OpenedPlaylist::Unset => return Ok(Some(playing())),
            OpenedPlaylist::Library => return Ok(None),
            OpenedPlaylist::Playlist(id) if Some(id) == stored.playlist_id.as_ref() => return Ok(Some(playing())),
            OpenedPlaylist::Playlist(id) => id.clone(),
        };
        // A different playlist was open than the one playing - same root-then-linear-search resolution as above. Left as the library
        // if it can't be found - a removed/renamed playlist or a lapsed grant is no worse than "not found" for the playing playlist.
        let opened_root = match stored.opened_root_id.as_ref().and_then(|id| roots.iter().find(|r| &r.id == id)) {
            Some(root) => Some(root.clone()),
            None => self.find_root_with_playlist(roots, &opened_id, is_cancelled).await?,
        };
        let Some(opened_root) = opened_root else { return Ok(None) };
        let (playlists, tracks) = self.load_root_tolerating_pending(&opened_root, load_options).await?;
        Ok(playlists.into_iter().find(|p| p.id == opened_id).map(|playlist| PlaylistContext { root: opened_root, playlist, tracks: tracks.into_values().collect() }))
    }

    // ---------------------------------------------------------------- refreshing the library

    /// (Re)loads everything derived from the granted roots: lists the roots, scans/loads each library root, loads lyrics scopes, then
    /// kicks off the background lyrics-match and metadata passes and reconciles the playing playlist. Resolves with the loaded roots as
    /// soon as they're in the store; the lyrics/metadata passes are fire-and-forget. Fails only if the roots can't be listed at all -
    /// one bad root just reports its own error and drops out, leaving the rest of the library visible.
    ///
    /// Uses the background file access throughout: this runs automatically (on launch, after restore) with no user gesture behind it,
    /// so it must never trigger a permission prompt. `sync_server`: in server mode, check in with the server afterwards (the sync
    /// itself passes `false` so it doesn't loop).
    pub async fn refresh_library(&self, sync_server: bool) -> Result<Vec<RootWithLibrary>, String> {
        let cfg = self.cfg();
        let server_active = (cfg.server_active)();
        self.step(RestoringStep::ListingFolders);
        let roots = match cfg.file_access.list_granted_roots().await {
            Ok(roots) => roots,
            Err(error) => {
                cfg.store.dispatch(|s| s.library.roots_load_failed());
                return Err(message(error));
            }
        };
        let granted = roots.clone();
        cfg.store.dispatch(move |s| s.library.granted_roots_loaded(granted));

        // Once per launch.
        if !cfg.store.read(|s| s.library.orphan_prune_started) {
            cfg.store.dispatch(|s| s.library.orphan_prune_started());
            cfg.jobs.prune_orphans(roots.iter().map(|r| r.id.clone()).collect());
        }

        self.step(RestoringStep::ScanningLibrary);
        // Each root is isolated - one bad root (a moved/deleted folder, a native file-access error) used to reject the whole load and
        // blank the ENTIRE library on every refresh until the bad root was removed.
        let loaded = join_all(roots.iter().filter(|r| r.kind == RootKind::Library).map(|root| self.load_one_root(root.clone(), server_active))).await;
        let root_data: Vec<LibraryRootData> = loaded.into_iter().flatten().collect();
        let for_state = root_data.clone();
        cfg.store.dispatch(move |s| s.library.library_loaded(for_state));
        let with_library: Vec<RootWithLibrary> = root_data.iter().map(RootWithLibrary::from).collect();

        // A lyrics-kind root can reach the root listing without a gesture of its own (e.g. the self-hosted server exposing one just by
        // having the volume mounted). Adding a scope is an upsert, so repeating it per refresh is a no-op.
        for root in roots.iter().filter(|r| r.kind == RootKind::Lyrics) {
            cfg.library_store.add_lyrics_scope(LyricsScope { root_id: root.id.clone(), relative_path: String::new() }).await.map_err(message)?;
        }

        // Lyrics scopes are subfolders of an already-granted root, so just walk each configured scope for .lrc files. (Server mode:
        // the server matches lyrics, and scopes left over from local folders would only point at roots that no longer exist.)
        let scopes = if server_active { Vec::new() } else { cfg.library_store.get_lyrics_scopes().await.map_err(message)? };
        let (loaded_scopes, known) = (scopes.clone(), !scopes.is_empty());
        cfg.store.dispatch(move |s| {
            s.library.lyrics_scopes_loaded(loaded_scopes);
            s.restore.lyrics_scopes_known(known);
        });

        let player = cfg.player.clone();
        let priority: Rc<dyn Fn() -> Vec<String>> = Rc::new(move || [player.get_state().current_file_id, player.get_next_file_id()].into_iter().flatten().collect());
        // A missing-track placeholder has no real file to match against - excluded rather than let the scan fail on it.
        let real_tracks: Vec<TrackRecord> = with_library.iter().flat_map(|r| r.tracks_by_id.values().filter(|t| !t.missing).cloned()).collect();

        // Auto-assign any track without a lyrics match (a manual override is never overwritten). Recomputed on every refresh rather
        // than cached, since a scope's contents can change between scans.
        if known {
            self.step(RestoringStep::ScanningLyrics);
            // Optimistic starting display: last launch's exact final count, so a library that hasn't changed since (the common case)
            // never visibly moves; the pass only ticks it down on an anomaly, then snaps to the true count and persists it as next
            // launch's baseline.
            let last_known: Option<u32> = cfg.library_store.get_setting(LYRICS_MATCHED_COUNT_SETTING_KEY).await.map_err(message)?.and_then(|v| v.trim().parse::<f64>().ok()).map(|v| v as u32);
            cfg.store.dispatch(move |s| s.library.matched_lyrics_count_set(last_known));
            let (progress_store, anomaly_store, settled_store) = (cfg.store.clone(), cfg.store.clone(), cfg.store.clone());
            let (persist_store, runtime) = (cfg.library_store.clone(), cfg.runtime.clone());
            let callbacks = LyricsMatchCallbacks {
                // Only drives the display for a first-ever run (no baseline to start optimistic from) - otherwise the anomaly and settled
                // callbacks own it so it doesn't jump down to a small live count and re-climb every launch.
                on_progress: Rc::new(move |matched| {
                    if last_known.is_none() {
                        progress_store.dispatch(move |s| s.library.matched_lyrics_count_set(Some(matched as u32)));
                    }
                }),
                on_anomaly: Rc::new(move || {
                    if last_known.is_some() {
                        anomaly_store.dispatch(|s| s.library.matched_lyrics_count_decremented());
                    }
                }),
                on_settled: Rc::new(move |matched| {
                    settled_store.dispatch(move |s| s.library.matched_lyrics_count_set(Some(matched as u32)));
                    let store = persist_store.clone();
                    runtime.spawn(Box::pin(async move {
                        let _ = store.put_setting(LYRICS_MATCHED_COUNT_SETTING_KEY, &matched.to_string()).await;
                    }));
                }),
            };
            cfg.jobs.match_lyrics(scopes, real_tracks.clone(), callbacks, priority.clone());
        } else {
            cfg.store.dispatch(|s| s.library.matched_lyrics_count_set(None));
        }

        // Skipped in server mode: the sync already delivered every track's metadata, and this would download each file just to read it.
        if !server_active {
            cfg.jobs.scan_metadata(real_tracks, priority);
        }

        // The now-playing playlist's .m3u8 may have been rescanned (this refresh, a manual rescan, or a sync tool dropping in a file)
        // since playback started - reconcile the player's running order against the fresh scan (a plain reload would restart
        // shuffle/position bookkeeping).
        if let Some(now_playing) = cfg.player.get_current_playlist_id() {
            for entry in &with_library {
                if let Some(playlist) = entry.playlists.iter().find(|p| p.id == now_playing) {
                    cfg.player.reconcile_playlist(playlist.track_file_ids.clone());
                    let (refreshed, tracks) = (playlist.clone(), entry.tracks_by_id.values().cloned().collect::<Vec<_>>());
                    cfg.store.dispatch(move |s| s.navigation.playing_context_refreshed(refreshed, tracks));
                    self.publish_live();
                    self.patch_playback(PlaybackPatch { shuffle_order: Some(cfg.player.get_shuffle_order()), ..Default::default() }, 0);
                    break;
                }
            }
        }

        // Checked in last and in the background: the cached library above is already on screen.
        if server_active && sync_server {
            let this = self.clone();
            self.spawn(async move {
                this.sync_server_library().await;
            });
        }
        Ok(with_library)
    }

    async fn load_one_root(&self, root: GrantedRoot, server_active: bool) -> Option<LibraryRootData> {
        let cfg = self.cfg();
        let result: Result<LibraryRootData, LibraryError> = async {
            let mut playlists = cfg.library_store.list_playlists(&root.id).await?;
            let mut tracks = cfg.library_store.list_tracks(&root.id).await?;
            // A root can reach the root listing without ever going through the explicit request-then-scan flow - e.g. a self-hosted
            // server root just by having a volume mounted. Scan it now instead of silently showing an empty library until the person
            // clicks Rescan. Some roots (the self-hosted server's) are always rescanned, since their files can change on disk with zero
            // app interaction and there's no OS permission prompt to worry about triggering outside a gesture. (Server mode never
            // scans: there are no folders, and the sync fills the store.)
            if !server_active && ((playlists.is_empty() && tracks.is_empty()) || (cfg.always_rescan_root)(&root.id)) {
                cfg.jobs.scan_root(&root.id, &root.display_name).await?;
                playlists = cfg.library_store.list_playlists(&root.id).await?;
                tracks = cfg.library_store.list_tracks(&root.id).await?;
            }
            Ok(LibraryRootData { root: root.clone(), playlists, tracks })
        }
        .await;
        match result {
            Ok(data) => Some(data),
            // A lapsed browser grant can't be re-requested from this non-gesture scan - the root just stays empty until a real click
            // re-grants it, rather than surfacing the browser's raw permission-error text as if it were a real bug.
            Err(LibraryError::FileAccess(FileAccessError::PermissionPending { .. })) => None,
            Err(error) => {
                let text = message(error);
                cfg.store.dispatch(move |s| s.library.library_error_set(Some(text)));
                None
            }
        }
    }

    /// Checks in with the server and, if that changed anything, reloads the library from the store so the screens pick it up. Offline
    /// and other failures are quiet here (the server connection keeps them in its status for the settings screen); the cached library
    /// simply stays. `None` when there is no server connection.
    pub async fn sync_server_library(&self) -> Option<bool> {
        let changed = self.cfg().jobs.sync_server().await?;
        if changed {
            // The sync may have brought preferences changed on another device; re-read them into the store.
            let theme = self.cfg().store.read(|s| s.settings.values.theme_mode);
            let _ = self.load_settings(theme).await;
            let _ = self.refresh_library(false).await;
        }
        Some(changed)
    }
}
