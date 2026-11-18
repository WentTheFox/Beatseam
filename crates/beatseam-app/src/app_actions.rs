//! More thunks: notifications, continuing another device's playback, and the playlist screens' actions.

use std::collections::HashMap;
use std::rc::Rc;

use beatseam_core::audio_engine::AudioEngine;
use beatseam_core::file_access::FileAccess;
use beatseam_core::library_scan::{AddPosition, MovePosition, add_tracks_to_playlist, move_playlist_tracks, remove_playlist_tracks};
use beatseam_core::library_store::{LibraryStore, PlaylistRecord, TrackRecord, track_display_name};
use beatseam_core::playlist::Position;
use beatseam_core::tasks::Runtime;
use beatseam_server_mode::{RemotePlayback, is_user_playlist_id};

use crate::app::{App, LibraryJobs};
use crate::navigation::{PlaylistContext, ScreenState};
use crate::notifications::{NotificationEntry, NotificationKind, ProgressUpsert};
use crate::persistence::PlaybackPatch;

pub fn unplaylisted_playlist_id(root_id: &str) -> String {
    format!("virtual:{root_id}:unplaylisted")
}

pub fn now_playing_playlist_id(root_id: &str) -> String {
    format!("virtual:{root_id}:now-playing")
}

/// A progress entry as callers describe it - with the live action callback, which the registry (not state) holds.
pub struct ProgressNotification {
    pub entry: ProgressUpsert,
    pub action: Option<(String, Rc<dyn Fn()>)>,
}

/// Callbacks for the parts that still live outside the store.
#[derive(Clone, Default)]
pub struct PlaylistActionCallbacks {
    /// Rescans a root after its playlist files changed on disk.
    pub rescan: Option<Rc<dyn Fn(&str)>>,
    /// Called when a screen is opened, so the caller can persist "last opened playlist" for restore.
    pub on_opened: Option<Rc<dyn Fn(&PlaylistRecord, &str)>>,
}

impl PlaylistActionCallbacks {
    fn opened(&self, playlist: &PlaylistRecord, root_id: &str) {
        if let Some(on_opened) = &self.on_opened {
            on_opened(playlist, root_id);
        }
    }

    fn rescan(&self, root_id: &str) {
        if let Some(rescan) = &self.rescan {
            rescan(root_id);
        }
    }
}

fn to_position(position: Position) -> MovePosition {
    match position {
        Position::Before => MovePosition::Before,
        Position::After => MovePosition::After,
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
    // ---------------------------------------------------------------- notifications

    fn next_notification_id(&self) -> String {
        let n = self.inner.id_counter.get() + 1;
        self.inner.id_counter.set(n);
        format!("{}-{n}", self.cfg().runtime.now_ms())
    }

    /// Adds a new one-off error entry, newest first.
    pub fn add_error(&self, title: &str, detail: Option<&str>) {
        let (id, now) = (self.next_notification_id(), self.cfg().runtime.now_ms());
        let (title, detail) = (title.to_owned(), detail.map(str::to_owned));
        self.cfg().store.dispatch(move |s| {
            s.notifications.error_added(NotificationEntry { id, kind: NotificationKind::Error, title, detail, created_at: now, progress: None, action_label: None, status: None });
        });
    }

    /// Creates or replaces a progress entry by id. Passing `progress.done` leaves the finished entry in the list until dismissed, so a
    /// completed background pass doesn't just silently vanish.
    pub fn upsert_notification(&self, notification: ProgressNotification) {
        let ProgressNotification { mut entry, action } = notification;
        let id = entry.id.clone();
        entry.action_label = action.as_ref().map(|(label, _)| label.clone());
        self.notification_actions().set(&id, action.map(|(_, on_press)| on_press));
        let now = self.cfg().runtime.now_ms();
        self.cfg().store.dispatch(move |s| s.notifications.progress_upserted(entry, now));
    }

    pub fn dismiss_notification(&self, id: &str) {
        self.notification_actions().set(id, None);
        let id = id.to_owned();
        self.cfg().store.dispatch(move |s| s.notifications.dismissed(&id));
    }

    pub fn clear_all_notifications(&self) {
        self.notification_actions().clear();
        self.cfg().store.dispatch(|s| s.notifications.cleared());
    }

    /// Runs a notification row's secondary action (e.g. "Cancel").
    pub fn run_notification_action(&self, id: &str) {
        self.notification_actions().run(id);
    }

    // ---------------------------------------------------------------- continuing another device

    /// Starts what another of the person's devices was playing - the same playlist, track and position, with its loop and shuffle
    /// settings - on this one, and plays it. Everything is read from the synced library in local storage, so it works without waiting
    /// on the network (the audio itself streams, or is already downloaded). Resolves `false` if that playlist or track isn't in this
    /// device's library (not synced yet, or since removed), in which case nothing is touched.
    pub async fn continue_playback(&self, remote: &RemotePlayback) -> Result<bool, String> {
        let roots = self.cfg().file_access.list_granted_roots().await.map_err(|e| e.to_string())?;
        // The root the other device named first, then the rest (it may have been a different view of the same library).
        let mut ordered = roots.clone();
        ordered.sort_by_key(|r| if Some(&r.id) == remote.root_id.as_ref() { 0 } else { 1 });
        let mut found: Option<(beatseam_core::file_access::GrantedRoot, PlaylistRecord)> = None;
        for root in ordered {
            let playlists = self.cfg().library_store.list_playlists(&root.id).await.map_err(|e| e.to_string())?;
            if let Some(playlist) = playlists.into_iter().find(|p| p.id == remote.playlist_id && p.track_file_ids.contains(&remote.current_track_file_id)) {
                found = Some((root, playlist));
                break;
            }
        }
        let Some((root, playlist)) = found else { return Ok(false) };
        let tracks = self.cfg().library_store.list_tracks(&root.id).await.map_err(|e| e.to_string())?;
        if !tracks.iter().any(|t| t.file_id == remote.current_track_file_id) {
            return Ok(false);
        }

        // Before the player loads: it resolves tracks through the playing context, possibly synchronously.
        let context = PlaylistContext { root: root.clone(), playlist: playlist.clone(), tracks };
        self.cfg().store.dispatch(move |s| s.navigation.playing_context_set(Some(context)));
        let player = &self.cfg().player;
        player.set_shuffle(remote.shuffle_enabled);
        player.set_loop_mode(remote.loop_mode);
        player.set_playlist(playlist.track_file_ids.clone(), Some(&remote.current_track_file_id), beatseam_core::playback::PlaylistOptions { playlist_id: Some(playlist.id.clone()), shuffle_order: remote.shuffle_order.clone() }).await;
        if remote.position_seconds > 1.0 {
            player.seek(remote.position_seconds);
        }
        self.publish_live();
        self.patch_playback(
            PlaybackPatch {
                playlist_id: Some(Some(playlist.id.clone())),
                root_id: Some(Some(root.id.clone())),
                current_track_file_id: Some(Some(remote.current_track_file_id.clone())),
                position_seconds: Some(remote.position_seconds),
                loop_mode: Some(remote.loop_mode),
                shuffle_enabled: Some(remote.shuffle_enabled),
                shuffle_order: Some(player.get_shuffle_order()),
                ..Default::default()
            },
            0,
        );
        Ok(true)
    }

    // ---------------------------------------------------------------- the playlist screens

    /// Opens a real playlist from the library.
    pub fn open_playlist(&self, context: PlaylistContext, callbacks: &PlaylistActionCallbacks) {
        let (playlist, root_id) = (context.playlist.clone(), context.root.id.clone());
        self.cfg().store.dispatch(move |s| s.navigation.playlist_screen_opened(context));
        callbacks.opened(&playlist, &root_id);
    }

    /// Opens the virtual, never-persisted "Now Playing" playlist - mirrors what's actually playing right now, in file order, or the
    /// live shuffled order when shuffle is on (`None` when off, since sequential order needs no separate tracking). Everything is
    /// already in memory, so no walk. Reads the player directly rather than the mirrored shuffle flag, which matters when called
    /// synchronously right after toggling shuffle.
    pub fn show_now_playing(&self, callbacks: &PlaylistActionCallbacks) {
        let Some(playing) = self.cfg().store.read(|s| s.navigation.playing_context.clone()) else { return };
        let id = now_playing_playlist_id(&playing.root.id);
        let playlist = PlaylistRecord { id: id.clone(), root_id: playing.root.id.clone(), file_id: id, name: "Now Playing".into(), track_file_ids: self.cfg().player.get_shuffle_order().unwrap_or_else(|| playing.playlist.track_file_ids.clone()) };
        let (root_id, shown) = (playing.root.id.clone(), playlist.clone());
        self.cfg().store.dispatch(move |s| s.navigation.playlist_screen_opened(PlaylistContext { root: playing.root, playlist: shown, tracks: playing.tracks }));
        callbacks.opened(&playlist, &root_id);
    }

    /// Opens a virtual, never-persisted "songs not in a playlist" playlist for a root. The screen opens immediately, empty, with a
    /// "Finding..." notice while the real directory walk runs - awaiting it first could leave the person on the library screen for a
    /// long while on a big root. The tracks themselves DO get persisted by the walk (so metadata/art/playback work normally); only the
    /// playlist is recomputed each time, since membership changes whenever another playlist is edited. A result is applied only if
    /// it's still the latest request and the screen is still this playlist.
    pub async fn show_unplaylisted(&self, root_id: &str, callbacks: &PlaylistActionCallbacks) {
        let playlist_id = unplaylisted_playlist_id(root_id);
        // Already viewing this root's Unplaylisted - re-walking on every click just re-shows "Finding..." for no reason.
        if self.cfg().store.read(|s| matches!(&s.navigation.screen, ScreenState::Playlist(c) if c.playlist.id == playlist_id)) {
            return;
        }
        let Some(root) = self.cfg().store.read(|s| s.library.granted_roots.iter().find(|r| r.id == root_id).cloned()) else { return };
        let empty = PlaylistRecord { id: playlist_id.clone(), root_id: root_id.to_owned(), file_id: playlist_id, name: "Songs not in a playlist".into(), track_file_ids: Vec::new() };
        let (screen_root, screen_playlist) = (root.clone(), empty.clone());
        self.cfg().store.dispatch(move |s| s.navigation.playlist_screen_opened(PlaylistContext { root: screen_root, playlist: screen_playlist, tracks: Vec::new() }));
        callbacks.opened(&empty, &root.id);
        let request_id = {
            let id = Rc::new(std::cell::Cell::new(0));
            let c = id.clone();
            self.cfg().store.dispatch(move |s| c.set(s.navigation.unplaylisted_requested()));
            id.get()
        };
        let found = self.cfg().jobs.find_unplaylisted(root_id, &root.display_name).await;
        let latest = self.cfg().store.read(|s| s.navigation.unplaylisted_request_id) == request_id;
        if latest {
            match found {
                Ok(tracks) => {
                    let playlist = PlaylistRecord { track_file_ids: tracks.iter().map(|t| t.file_id.clone()).collect(), ..empty };
                    self.cfg().store.dispatch(move |s| s.navigation.unplaylisted_loaded(playlist, tracks));
                }
                Err(error) => self.add_error("Couldn't find songs not in a playlist", Some(&error)),
            }
        }
        self.cfg().store.dispatch(move |s| s.navigation.unplaylisted_settled(request_id));
    }

    /// The open playlist screen as the pieces the file-editing helpers take, or `None` if a playlist isn't open.
    fn open_playlist_of(&self) -> Option<(beatseam_core::file_access::GrantedRoot, PlaylistRecord, HashMap<String, TrackRecord>)> {
        self.cfg().store.read(|s| match &s.navigation.screen {
            ScreenState::Playlist(c) => Some((c.root.clone(), c.playlist.clone(), c.tracks_by_id())),
            ScreenState::Library => None,
        })
    }

    /// Whether `playlist` is one of the person's own (server mode) rather than a playlist file edited by rewriting it.
    fn is_own_playlist(&self, playlist: &PlaylistRecord) -> bool {
        self.cfg().jobs.user_playlists_active() && is_user_playlist_id(&playlist.id)
    }

    /// Moves tracks within the open real playlist (writing its `.m3u8`), patches the open screen's order immediately - nothing
    /// reconciles the open screen against a rescan's result, so without this the reorder would look like it did nothing until the
    /// playlist was reopened - then rescans.
    pub async fn move_open_playlist_tracks(&self, moving_file_ids: &[String], anchor_file_id: &str, position: Position, callbacks: &PlaylistActionCallbacks) {
        let Some((root, playlist, tracks_by_id)) = self.open_playlist_of() else { return };
        let outcome: Result<bool, String> = async {
            if self.is_own_playlist(&playlist) {
                if let Some(reordered) = beatseam_core::playlist::move_within(&playlist.track_file_ids, moving_file_ids, anchor_file_id, position) {
                    self.cfg().jobs.user_playlist_set(&playlist.id, &reordered).await?;
                }
                return Ok(true);
            }
            move_playlist_tracks(&*self.cfg().file_access, &root.id, &playlist, &tracks_by_id, moving_file_ids, anchor_file_id, to_position(position)).await.map_err(|e| e.to_string())?;
            Ok(false)
        }
        .await;
        match outcome {
            Ok(own) => {
                let (id, moving, anchor) = (playlist.id.clone(), moving_file_ids.to_vec(), anchor_file_id.to_owned());
                self.cfg().store.dispatch(move |s| s.navigation.open_playlist_tracks_moved(&id, &moving, &anchor, position));
                self.after_playlist_edit(own, &root.id, callbacks);
            }
            Err(error) => self.add_error(&format!("Couldn't reorder \"{}\"", playlist.name), Some(&error)),
        }
    }

    /// Removes tracks from the open real playlist (writing its `.m3u8`), patches the open screen immediately, then rescans.
    pub async fn remove_open_playlist_tracks(&self, file_ids: &[String], callbacks: &PlaylistActionCallbacks) {
        let Some((root, playlist, tracks_by_id)) = self.open_playlist_of() else { return };
        let outcome: Result<bool, String> = async {
            if self.is_own_playlist(&playlist) {
                self.cfg().jobs.user_playlist_remove(&playlist.id, file_ids).await?;
                return Ok(true);
            }
            remove_playlist_tracks(&*self.cfg().file_access, &root.id, &playlist, &tracks_by_id, file_ids).await.map_err(|e| e.to_string())?;
            Ok(false)
        }
        .await;
        match outcome {
            Ok(own) => {
                let (id, removed) = (playlist.id.clone(), file_ids.to_vec());
                self.cfg().store.dispatch(move |s| s.navigation.open_playlist_tracks_removed(&id, &removed));
                self.after_playlist_edit(own, &root.id, callbacks);
            }
            Err(error) => self.add_error(&format!("Couldn't remove tracks from \"{}\"", playlist.name), Some(&error)),
        }
    }

    /// Adds the pending "add to playlist" track to `playlist`, then drops it from the open Unplaylisted view immediately rather than
    /// waiting on the rescan - the Unplaylisted finder won't offer it again once the rescanned playlist references it, but that can take
    /// a moment on a big root.
    pub async fn add_pending_track_to_playlist(&self, playlist: &PlaylistRecord, at_start: bool, callbacks: &PlaylistActionCallbacks) {
        let Some(track) = self.cfg().store.read(|s| s.navigation.add_to_playlist_track.clone()) else { return };
        let Some((root, open, _)) = self.open_playlist_of() else { return };
        let outcome: Result<bool, String> = async {
            if self.is_own_playlist(playlist) {
                self.cfg().jobs.user_playlist_add(&playlist.id, std::slice::from_ref(&track.file_id), at_start).await?;
                return Ok(true);
            }
            let position = if at_start { AddPosition::Start } else { AddPosition::End };
            add_tracks_to_playlist(&*self.cfg().file_access, &root.id, playlist, std::slice::from_ref(&track), position).await.map_err(|e| e.to_string())?;
            Ok(false)
        }
        .await;
        match outcome {
            Ok(own) => {
                let (id, removed) = (open.id.clone(), vec![track.file_id.clone()]);
                self.cfg().store.dispatch(move |s| {
                    s.navigation.open_playlist_tracks_removed(&id, &removed);
                    s.navigation.add_to_playlist_track_set(None);
                });
                self.after_playlist_edit(own, &root.id, callbacks);
            }
            Err(error) => self.add_error(&format!("Couldn't add \"{}\" to \"{}\"", track_display_name(&track), playlist.name), Some(&error)),
        }
    }

    /// After an edit: the person's own playlists reload from the store (the server's version arrives with the next check-in); a
    /// playlist file is rescanned.
    fn after_playlist_edit(&self, own: bool, root_id: &str, callbacks: &PlaylistActionCallbacks) {
        if own {
            let this = self.clone();
            self.spawn(async move {
                let _ = this.refresh_library(false).await;
            });
        } else {
            callbacks.rescan(root_id);
        }
    }

    /// Creates one of the person's own playlists (server mode) and opens it. It exists locally at once and reaches the server through
    /// the offline queue, so this works with no connection.
    pub async fn create_user_playlist_and_open(&self, name: &str, file_ids: &[String], callbacks: &PlaylistActionCallbacks) {
        if !self.cfg().jobs.user_playlists_active() {
            return;
        }
        let outcome: Result<(), String> = async {
            let record = self.cfg().jobs.user_playlist_create(name, file_ids).await?;
            self.refresh_library(false).await?;
            let Some(root) = self.cfg().store.read(|s| s.library.granted_roots.iter().find(|r| r.id == record.root_id).cloned()) else { return Ok(()) };
            let tracks = self.cfg().library_store.list_tracks(&root.id).await.map_err(|e| e.to_string())?;
            let (shown, root_id) = (record.clone(), root.id.clone());
            self.cfg().store.dispatch(move |s| s.navigation.playlist_screen_opened(PlaylistContext { root, playlist: shown, tracks }));
            callbacks.opened(&record, &root_id);
            Ok(())
        }
        .await;
        if let Err(error) = outcome {
            self.add_error(&format!("Couldn't create \"{}\"", beatseam_core::js_compat::js_trim(name)), Some(&error));
        }
    }

    /// Renames the open playlist, if it is one of the person's own (server mode).
    pub async fn rename_open_user_playlist(&self, name: &str) {
        let Some((_, playlist, _)) = self.open_playlist_of() else { return };
        if !self.is_own_playlist(&playlist) {
            return;
        }
        match self.cfg().jobs.user_playlist_rename(&playlist.id, name).await {
            Ok(renamed) => {
                let id = playlist.id.clone();
                self.cfg().store.dispatch(move |s| s.navigation.open_playlist_renamed(&id, &renamed.name));
                self.after_playlist_edit(true, "", &PlaylistActionCallbacks::default());
            }
            Err(error) => self.add_error(&format!("Couldn't rename \"{}\"", playlist.name), Some(&error)),
        }
    }

    /// Deletes the open playlist, if it is one of the person's own (server mode), and goes back to the library.
    pub async fn delete_open_user_playlist(&self) {
        let Some((_, playlist, _)) = self.open_playlist_of() else { return };
        if !self.is_own_playlist(&playlist) {
            return;
        }
        match self.cfg().jobs.user_playlist_delete(&playlist.id).await {
            Ok(()) => {
                self.cfg().store.dispatch(|s| s.navigation.library_screen_opened());
                self.after_playlist_edit(true, "", &PlaylistActionCallbacks::default());
            }
            Err(error) => self.add_error(&format!("Couldn't delete \"{}\"", playlist.name), Some(&error)),
        }
    }
}
