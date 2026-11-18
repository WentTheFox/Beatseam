//! Port of `store/persistence`: the last known persisted playback state, which every change merges onto.

use beatseam_core::library_store::{LoopMode, OpenedPlaylist, PlaybackState};

pub fn default_playback_state() -> PlaybackState {
    PlaybackState {
        playlist_id: None,
        current_track_file_id: None,
        root_id: None,
        // Explicitly "the library screen" (the TypeScript's null), not "state from before this was tracked".
        opened_playlist_id: OpenedPlaylist::Library,
        opened_root_id: None,
        position_seconds: 0.0,
        loop_mode: LoopMode::Off,
        shuffle_enabled: false,
        shuffle_order: None,
        volume: 0.25,
        now_playing_open: false,
    }
}

/// A change to part of the persisted playback state: only the `Some` fields are set (nullable fields are `Option<Option<..>>` so a
/// patch can clear them).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlaybackPatch {
    pub playlist_id: Option<Option<String>>,
    pub current_track_file_id: Option<Option<String>>,
    pub root_id: Option<Option<String>>,
    pub opened_playlist_id: Option<OpenedPlaylist>,
    pub opened_root_id: Option<Option<String>>,
    pub position_seconds: Option<f64>,
    pub loop_mode: Option<LoopMode>,
    pub shuffle_enabled: Option<bool>,
    pub shuffle_order: Option<Option<Vec<String>>>,
    pub volume: Option<f64>,
    pub now_playing_open: Option<bool>,
}

impl PlaybackPatch {
    pub fn apply_to(&self, state: &mut PlaybackState) {
        if let Some(v) = &self.playlist_id {
            state.playlist_id = v.clone();
        }
        if let Some(v) = &self.current_track_file_id {
            state.current_track_file_id = v.clone();
        }
        if let Some(v) = &self.root_id {
            state.root_id = v.clone();
        }
        if let Some(v) = &self.opened_playlist_id {
            state.opened_playlist_id = v.clone();
        }
        if let Some(v) = &self.opened_root_id {
            state.opened_root_id = v.clone();
        }
        if let Some(v) = self.position_seconds {
            state.position_seconds = v;
        }
        if let Some(v) = self.loop_mode {
            state.loop_mode = v;
        }
        if let Some(v) = self.shuffle_enabled {
            state.shuffle_enabled = v;
        }
        if let Some(v) = &self.shuffle_order {
            state.shuffle_order = v.clone();
        }
        if let Some(v) = self.volume {
            state.volume = v;
        }
        if let Some(v) = self.now_playing_open {
            state.now_playing_open = v;
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PersistenceState {
    /// The last known persisted playback state - what every patch merges onto, so no call site needs an async read first.
    pub state: PlaybackState,
    /// False until the startup restore has read what's actually persisted. Until then `state` is only a placeholder default, and a
    /// patch merged onto it would be written back over the real saved state (seen on-device: a dev Fast Refresh remount let the
    /// position tick race ahead of the initial read and silently reverted the Now Playing flag).
    pub ready: bool,
    /// When the playing position was last persisted, in ms; 0 = never.
    pub last_position_persist_at: u64,
}

impl Default for PersistenceState {
    fn default() -> Self {
        Self { state: default_playback_state(), ready: false, last_position_persist_at: 0 }
    }
}

impl PersistenceState {
    /// The persisted state was read (`None` = nothing ever saved): from here on patches are applied and written back.
    pub fn playback_state_read(&mut self, read: Option<PlaybackState>) {
        if let Some(read) = read {
            self.state = read;
        }
        self.ready = true;
    }

    /// Merges a change into the persisted state; the caller then writes it (see [`PersistWrite`]). A no-op before the initial read -
    /// returns whether the patch was applied.
    pub fn playback_patched(&mut self, patch: &PlaybackPatch) -> bool {
        if !self.ready {
            return false;
        }
        patch.apply_to(&mut self.state);
        true
    }

    pub fn position_persist_stamped(&mut self, at_ms: u64) {
        self.last_position_persist_at = at_ms;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_applied_until_the_real_state_has_been_read() {
        let mut state = PersistenceState::default();
        assert!(!state.playback_patched(&PlaybackPatch { now_playing_open: Some(true), ..Default::default() }));
        assert!(!state.state.now_playing_open, "the placeholder was not changed");
        state.playback_state_read(None);
        assert!(state.ready);
        assert!(state.playback_patched(&PlaybackPatch { now_playing_open: Some(true), ..Default::default() }));
        assert!(state.state.now_playing_open);
    }

    #[test]
    fn reading_saved_state_replaces_the_placeholder_and_patches_merge_onto_it() {
        let mut state = PersistenceState::default();
        state.playback_state_read(Some(PlaybackState { playlist_id: Some("p".into()), volume: 0.8, ..default_playback_state() }));
        state.playback_patched(&PlaybackPatch { position_seconds: Some(42.0), shuffle_order: Some(Some(vec!["a".into()])), ..Default::default() });
        assert_eq!((state.state.playlist_id.as_deref(), state.state.volume, state.state.position_seconds, state.state.shuffle_order.clone()), (Some("p"), 0.8, 42.0, Some(vec!["a".to_string()])));
    }

    #[test]
    fn a_patch_can_clear_a_nullable_field() {
        let mut state = PersistenceState::default();
        state.playback_state_read(Some(PlaybackState { current_track_file_id: Some("t".into()), shuffle_order: Some(vec!["t".into()]), ..default_playback_state() }));
        state.playback_patched(&PlaybackPatch { current_track_file_id: Some(None), shuffle_order: Some(None), ..Default::default() });
        assert_eq!((state.state.current_track_file_id, state.state.shuffle_order), (None, None));
    }

    #[test]
    fn the_defaults_match_the_typescript() {
        let d = default_playback_state();
        assert_eq!((d.volume, d.opened_playlist_id.clone(), d.now_playing_open), (0.25, OpenedPlaylist::Library, false));
    }

    #[test]
    fn the_position_stamp_is_recorded() {
        let mut state = PersistenceState::default();
        state.position_persist_stamped(123);
        assert_eq!(state.last_position_persist_at, 123);
    }
}
