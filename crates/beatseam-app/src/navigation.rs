//! Port of `store/navigation/navigationSlice`: which screen is open, overlay/modal visibility, the multi-step create-playlist
//! flow, and which playlist is actually playing.
//!
//! Persisting the Now Playing flag for restore is a playback concern and stays at the call sites, not in these reducers.

use std::collections::HashMap;

use beatseam_core::file_access::GrantedRoot;
use beatseam_core::library_store::{PlaylistRecord, TrackRecord};
use beatseam_core::playlist::{Position, move_within};

/// The folder a new playlist will be created from, once the folder browser has picked one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatePlaylistTarget {
    pub root_id: String,
    pub folder_relative_path: String,
    pub folder_display_name: String,
}

/// A playlist as opened on screen (or playing): a self-contained snapshot, not ids to look up. The playlist can be virtual
/// (Unplaylisted, Now Playing), and Unplaylisted's tracks include freshly-discovered ones the library slice doesn't have yet,
/// so the snapshot carries its own tracks.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaylistContext {
    pub root: GrantedRoot,
    pub playlist: PlaylistRecord,
    pub tracks: Vec<TrackRecord>,
}

impl PlaylistContext {
    /// The tracks as the map screens look tracks up in.
    pub fn tracks_by_id(&self) -> HashMap<String, TrackRecord> {
        self.tracks.iter().map(|t| (t.file_id.clone(), t.clone())).collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ScreenState {
    Library,
    Playlist(PlaylistContext),
}

/// The "Edit raw" overlay's target - only ever a real, file-backed playlist.
#[derive(Clone, Debug, PartialEq)]
pub struct EditingPlaylistFile {
    pub root: GrantedRoot,
    pub playlist: PlaylistRecord,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NavigationState {
    pub settings_open: bool,
    /// The "External connections" sub-screen reached from within Settings - always closes before Settings does.
    pub external_connections_open: bool,
    /// The "Beatseam server" sub-screen reached from within Settings - closes before Settings does, like External connections.
    pub server_screen_open: bool,
    /// Whether the Now Playing overlay is open. Meaningful as an overlay only in the narrow tier; docked tiers ignore it.
    pub now_playing_open: bool,
    /// Lifted out of the lyrics section so the header menu can open them from outside Now Playing.
    pub lyrics_editor_open: bool,
    pub lyrics_picker_open: bool,
    /// Step 1 of create-playlist: the root whose folder browser is showing.
    pub create_playlist_root_id: Option<String>,
    /// Step 2: the chosen folder, whose create-playlist screen is showing.
    pub create_playlist_target: Option<CreatePlaylistTarget>,
    /// Whichever screen is currently OPEN.
    pub screen: ScreenState,
    /// Which real playlist is actually playing (not necessarily `screen`: the person may have navigated back to the library while
    /// a track keeps playing). Set when a track starts from an open playlist, and restored from the playback state on launch - a
    /// freshly-resumed session with no tap yet still needs it for the "Now Playing" automatic row and for shuffle to switch to
    /// it. `None` only before that restore has resolved.
    pub playing_context: Option<PlaylistContext>,
    pub editing_playlist_file: Option<EditingPlaylistFile>,
    /// The track an "add to playlist" tap on an Unplaylisted row is offering to add.
    pub add_to_playlist_track: Option<TrackRecord>,
    /// True while the Unplaylisted directory walk for the open screen is in flight.
    pub is_finding_unplaylisted: bool,
    /// Bumped per Unplaylisted request so a stale walk's result can't land after the person navigated on.
    pub unplaylisted_request_id: u64,
}

impl Default for NavigationState {
    fn default() -> Self {
        Self {
            settings_open: false,
            external_connections_open: false,
            server_screen_open: false,
            now_playing_open: false,
            lyrics_editor_open: false,
            lyrics_picker_open: false,
            create_playlist_root_id: None,
            create_playlist_target: None,
            screen: ScreenState::Library,
            playing_context: None,
            editing_playlist_file: None,
            add_to_playlist_track: None,
            is_finding_unplaylisted: false,
            unplaylisted_request_id: 0,
        }
    }
}

impl NavigationState {
    pub fn settings_opened(&mut self) {
        self.settings_open = true;
    }

    /// Closing Settings also closes its sub-screens, so a child can never outlive its parent.
    pub fn settings_closed(&mut self) {
        self.settings_open = false;
        self.external_connections_open = false;
        self.server_screen_open = false;
    }

    pub fn server_screen_opened(&mut self) {
        self.server_screen_open = true;
    }

    pub fn server_screen_closed(&mut self) {
        self.server_screen_open = false;
    }

    pub fn external_connections_opened(&mut self) {
        self.external_connections_open = true;
    }

    pub fn external_connections_closed(&mut self) {
        self.external_connections_open = false;
    }

    pub fn now_playing_open_changed(&mut self, open: bool) {
        self.now_playing_open = open;
    }

    pub fn lyrics_editor_open_changed(&mut self, open: bool) {
        self.lyrics_editor_open = open;
    }

    pub fn lyrics_picker_open_changed(&mut self, open: bool) {
        self.lyrics_picker_open = open;
    }

    /// From the header lyrics menu: the editor lives inside Now Playing, so open that too.
    pub fn lyrics_editor_opened_from_menu(&mut self) {
        self.lyrics_editor_open = true;
        self.now_playing_open = true;
    }

    pub fn lyrics_picker_opened_from_menu(&mut self) {
        self.lyrics_picker_open = true;
        self.now_playing_open = true;
    }

    pub fn create_playlist_started(&mut self, root_id: String) {
        self.create_playlist_root_id = Some(root_id);
    }

    /// The folder browser's pick: moves from step 1 to step 2.
    pub fn create_playlist_folder_chosen(&mut self, target: CreatePlaylistTarget) {
        self.create_playlist_root_id = None;
        self.create_playlist_target = Some(target);
    }

    /// Cancelling or finishing at either step ends the whole flow.
    pub fn create_playlist_ended(&mut self) {
        self.create_playlist_root_id = None;
        self.create_playlist_target = None;
    }

    pub fn library_screen_opened(&mut self) {
        self.screen = ScreenState::Library;
    }

    pub fn playlist_screen_opened(&mut self, context: PlaylistContext) {
        self.screen = ScreenState::Playlist(context);
    }

    fn open_playlist_mut(&mut self, playlist_id: &str) -> Option<&mut PlaylistContext> {
        match &mut self.screen {
            ScreenState::Playlist(context) if context.playlist.id == playlist_id => Some(context),
            _ => None,
        }
    }

    /// The Unplaylisted walk finished: fills in the open screen, if it's still that playlist.
    pub fn unplaylisted_loaded(&mut self, playlist: PlaylistRecord, tracks: Vec<TrackRecord>) {
        if let Some(open) = self.open_playlist_mut(&playlist.id.clone()) {
            open.playlist = playlist;
            open.tracks = tracks;
        }
    }

    /// Reorders the open playlist immediately rather than waiting on the rescan, so the screen doesn't look unchanged until
    /// reopened (the `.m3u8` itself is already correct). A no-op if the anchor is gone or another playlist is open.
    pub fn open_playlist_tracks_moved(&mut self, playlist_id: &str, moving_file_ids: &[String], anchor_file_id: &str, position: Position) {
        if let Some(open) = self.open_playlist_mut(playlist_id) {
            if let Some(moved) = move_within(&open.playlist.track_file_ids, moving_file_ids, anchor_file_id, position) {
                open.playlist.track_file_ids = moved;
            }
        }
    }

    /// Drops tracks from the open playlist immediately (a removal, or a just-added track leaving the Unplaylisted view) rather than
    /// waiting on the rescan.
    pub fn open_playlist_tracks_removed(&mut self, playlist_id: &str, file_ids: &[String]) {
        if let Some(open) = self.open_playlist_mut(playlist_id) {
            open.playlist.track_file_ids.retain(|id| !file_ids.contains(id));
        }
    }

    /// The open playlist was renamed.
    pub fn open_playlist_renamed(&mut self, playlist_id: &str, name: &str) {
        if let Some(open) = self.open_playlist_mut(playlist_id) {
            open.playlist.name = name.to_owned();
        }
    }

    /// A track started playing from the open playlist: that playlist is now the playing one.
    pub fn playing_context_set_from_screen(&mut self) {
        if let ScreenState::Playlist(context) = &self.screen {
            self.playing_context = Some(context.clone());
        }
    }

    pub fn playing_context_set(&mut self, context: Option<PlaylistContext>) {
        self.playing_context = context;
    }

    /// A rescan re-read the playing playlist: keep the playing context (what the player resolves tracks through) in step with it. A
    /// no-op if something else is playing.
    pub fn playing_context_refreshed(&mut self, playlist: PlaylistRecord, tracks: Vec<TrackRecord>) {
        if let Some(context) = self.playing_context.as_mut().filter(|c| c.playlist.id == playlist.id) {
            context.playlist = playlist;
            context.tracks = tracks;
        }
    }

    /// Startup restore resolved: reopen the last open playlist (and Now Playing, if it was open) and record what's playing.
    pub fn playback_restored(&mut self, opened: Option<PlaylistContext>, now_playing_open: bool, playing: PlaylistContext) {
        if let Some(opened) = opened {
            self.screen = ScreenState::Playlist(opened);
        }
        if now_playing_open {
            self.now_playing_open = true;
        }
        self.playing_context = Some(playing);
    }

    pub fn editing_playlist_file_set(&mut self, target: Option<EditingPlaylistFile>) {
        self.editing_playlist_file = target;
    }

    pub fn add_to_playlist_track_set(&mut self, track: Option<TrackRecord>) {
        self.add_to_playlist_track = track;
    }

    /// Starts an Unplaylisted request: bumps the guard id and shows "Finding...". Returns the new request id.
    pub fn unplaylisted_requested(&mut self) -> u64 {
        self.unplaylisted_request_id += 1;
        self.is_finding_unplaylisted = true;
        self.unplaylisted_request_id
    }

    /// Clears "Finding..." only if this is still the latest request.
    pub fn unplaylisted_settled(&mut self, request_id: u64) {
        if self.unplaylisted_request_id == request_id {
            self.is_finding_unplaylisted = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use beatseam_core::file_access::RootKind;

    use super::*;

    fn root() -> GrantedRoot {
        GrantedRoot { id: "r1".into(), display_name: "Music".into(), kind: RootKind::Library, removable: true }
    }

    fn track(id: &str) -> TrackRecord {
        TrackRecord { file_id: id.into(), root_id: "r1".into(), relative_path: format!("{id}.mp3"), size_bytes: 1, last_modified_ms: 1.0, missing: false }
    }

    fn playlist(id: &str, ids: &[&str]) -> PlaylistRecord {
        PlaylistRecord { id: id.into(), root_id: "r1".into(), file_id: id.into(), name: id.into(), track_file_ids: ids.iter().map(|s| s.to_string()).collect() }
    }

    fn context(id: &str, ids: &[&str]) -> PlaylistContext {
        PlaylistContext { root: root(), playlist: playlist(id, ids), tracks: ids.iter().map(|i| track(i)).collect() }
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn open_ids(state: &NavigationState) -> Vec<String> {
        match &state.screen {
            ScreenState::Playlist(c) => c.playlist.track_file_ids.clone(),
            ScreenState::Library => panic!("not a playlist"),
        }
    }

    #[test]
    fn starts_with_everything_closed() {
        let state = NavigationState::default();
        assert!(!state.settings_open && !state.now_playing_open && !state.lyrics_editor_open && !state.is_finding_unplaylisted);
        assert_eq!((state.screen.clone(), state.playing_context.clone(), state.create_playlist_root_id.clone()), (ScreenState::Library, None, None));
    }

    #[test]
    fn closing_settings_closes_its_sub_screens_but_a_sub_screen_closes_alone() {
        let mut state = NavigationState::default();
        state.settings_opened();
        state.external_connections_opened();
        state.server_screen_opened();
        state.server_screen_closed();
        assert!(state.settings_open && state.external_connections_open && !state.server_screen_open);
        state.server_screen_opened();
        state.settings_closed();
        assert!(!state.settings_open && !state.external_connections_open && !state.server_screen_open);
        state.external_connections_opened();
        state.external_connections_closed();
        assert!(!state.external_connections_open);
    }

    #[test]
    fn opening_the_lyrics_editor_or_picker_from_the_menu_also_opens_now_playing_and_closing_it_leaves_the_flags() {
        let mut state = NavigationState::default();
        state.lyrics_editor_opened_from_menu();
        assert!(state.lyrics_editor_open && state.now_playing_open);
        let mut picker = NavigationState::default();
        picker.lyrics_picker_opened_from_menu();
        assert!(picker.lyrics_picker_open && picker.now_playing_open);
        state.now_playing_open_changed(false);
        assert!(state.lyrics_editor_open, "closing Now Playing leaves the lyrics flags alone");
        state.lyrics_editor_open_changed(false);
        state.lyrics_picker_open_changed(true);
        assert!(!state.lyrics_editor_open && state.lyrics_picker_open);
    }

    #[test]
    fn the_create_playlist_flow_goes_pick_a_root_choose_a_folder_end_and_cancelling_ends_it() {
        let mut state = NavigationState::default();
        state.create_playlist_started("r1".into());
        assert_eq!(state.create_playlist_root_id.as_deref(), Some("r1"));
        let target = CreatePlaylistTarget { root_id: "r1".into(), folder_relative_path: "Rock".into(), folder_display_name: "Rock".into() };
        state.create_playlist_folder_chosen(target.clone());
        assert_eq!((state.create_playlist_root_id.clone(), state.create_playlist_target.clone()), (None, Some(target)));
        state.create_playlist_ended();
        assert!(state.create_playlist_target.is_none());
        state.create_playlist_started("r1".into());
        state.create_playlist_ended();
        assert!(state.create_playlist_root_id.is_none());
    }

    #[test]
    fn open_playlist_edits_apply_only_to_that_playlist() {
        let mut state = NavigationState::default();
        state.playlist_screen_opened(context("p1", &["a", "b", "c", "d"]));
        state.open_playlist_tracks_moved("p1", &ids(&["d"]), "a", Position::Before);
        assert_eq!(open_ids(&state), ["d", "a", "b", "c"]);
        state.open_playlist_tracks_moved("other", &ids(&["a"]), "b", Position::After);
        state.open_playlist_tracks_moved("p1", &ids(&["a"]), "gone", Position::After);
        assert_eq!(open_ids(&state), ["d", "a", "b", "c"], "another playlist, or a missing anchor: a no-op");
        state.open_playlist_tracks_removed("p1", &ids(&["a", "c"]));
        assert_eq!(open_ids(&state), ["d", "b"]);
        state.open_playlist_renamed("p1", "Renamed");
        state.open_playlist_renamed("other", "Nope");
        match &state.screen {
            ScreenState::Playlist(c) => assert_eq!(c.playlist.name, "Renamed"),
            ScreenState::Library => unreachable!(),
        }
    }

    #[test]
    fn edits_do_nothing_while_the_library_is_open() {
        let mut state = NavigationState::default();
        state.open_playlist_tracks_removed("p1", &ids(&["a"]));
        state.open_playlist_renamed("p1", "x");
        state.unplaylisted_loaded(playlist("p1", &["a"]), vec![track("a")]);
        assert_eq!(state.screen, ScreenState::Library);
    }

    #[test]
    fn a_playing_playlist_is_recorded_from_the_screen_and_refreshed_in_step_with_a_rescan() {
        let mut state = NavigationState::default();
        state.playing_context_set_from_screen();
        assert!(state.playing_context.is_none(), "nothing open to take it from");
        state.playlist_screen_opened(context("p1", &["a", "b"]));
        state.playing_context_set_from_screen();
        state.library_screen_opened();
        assert_eq!(state.playing_context.as_ref().map(|c| c.playlist.id.as_str()), Some("p1"), "still playing though the library is showing");
        state.playing_context_refreshed(playlist("p1", &["a", "b", "c"]), vec![track("a"), track("b"), track("c")]);
        assert_eq!(state.playing_context.as_ref().map(|c| c.tracks.len()), Some(3));
        state.playing_context_refreshed(playlist("other", &["x"]), vec![track("x")]);
        assert_eq!(state.playing_context.as_ref().map(|c| c.playlist.id.as_str()), Some("p1"), "something else is playing: a no-op");
        state.playing_context_set(None);
        assert!(state.playing_context.is_none());
    }

    #[test]
    fn unplaylisted_results_land_only_on_the_playlist_they_were_asked_for() {
        let mut state = NavigationState::default();
        state.playlist_screen_opened(context("unplaylisted", &[]));
        state.unplaylisted_loaded(playlist("unplaylisted", &["x"]), vec![track("x")]);
        match &state.screen {
            ScreenState::Playlist(c) => assert_eq!((c.playlist.track_file_ids.clone(), c.tracks.len()), (ids(&["x"]), 1)),
            ScreenState::Library => unreachable!(),
        }
        state.unplaylisted_loaded(playlist("other", &["y"]), vec![track("y")]);
        assert_eq!(open_ids(&state), ["x"]);
    }

    #[test]
    fn a_stale_unplaylisted_request_cannot_clear_a_newer_ones_finding_flag() {
        let mut state = NavigationState::default();
        let first = state.unplaylisted_requested();
        let second = state.unplaylisted_requested();
        assert!(state.is_finding_unplaylisted && first != second);
        state.unplaylisted_settled(first);
        assert!(state.is_finding_unplaylisted, "the first walk is no longer the latest");
        state.unplaylisted_settled(second);
        assert!(!state.is_finding_unplaylisted);
    }

    #[test]
    fn restoring_playback_reopens_the_playlist_and_now_playing_and_records_what_is_playing() {
        let mut state = NavigationState::default();
        state.playback_restored(None, false, context("p1", &["a"]));
        assert_eq!((state.screen.clone(), state.now_playing_open), (ScreenState::Library, false));
        assert_eq!(state.playing_context.as_ref().map(|c| c.playlist.id.as_str()), Some("p1"));
        state.playback_restored(Some(context("p2", &["b"])), true, context("p1", &["a"]));
        assert!(matches!(&state.screen, ScreenState::Playlist(c) if c.playlist.id == "p2"));
        assert!(state.now_playing_open);
    }

    #[test]
    fn a_context_offers_its_tracks_as_a_lookup_map_and_the_small_setters_store_their_value() {
        let c = context("p1", &["a", "b"]);
        assert_eq!(c.tracks_by_id()["b"].file_id, "b");
        let mut state = NavigationState::default();
        state.add_to_playlist_track_set(Some(track("a")));
        state.editing_playlist_file_set(Some(EditingPlaylistFile { root: root(), playlist: playlist("p1", &[]) }));
        assert!(state.add_to_playlist_track.is_some() && state.editing_playlist_file.is_some());
        state.add_to_playlist_track_set(None);
        state.editing_playlist_file_set(None);
        assert!(state.add_to_playlist_track.is_none() && state.editing_playlist_file.is_none());
    }
}
