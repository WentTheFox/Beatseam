//! Port of `store/playback/playbackSlice`: the slow-moving part of the player's state.

use beatseam_core::library_store::LoopMode;
use beatseam_core::playback::{CrossfadeFileIds, PlaylistPlayerState, TrackPlayerStatus};

/// The slow-moving part of the player's state: changes on track changes, play/pause, mode toggles and crossfades starting/ending -
/// never per tick. Position and crossfade progress stay in the live player state.
#[derive(Clone, Debug, PartialEq)]
pub struct CoarsePlayback {
    pub current_file_id: Option<String>,
    pub status: TrackPlayerStatus,
    pub is_audible: bool,
    /// Index into playback order (post-shuffle); -1 if nothing is loaded.
    pub position: i64,
    pub total_tracks: usize,
    /// Length of the current track, so the seek bar's range is known without reading the live position.
    pub duration_seconds: f64,
    pub loop_mode: LoopMode,
    pub shuffle_enabled: bool,
    pub is_loading_for_playback: bool,
    pub pending_crossfade_file_ids: Option<CrossfadeFileIds>,
}

impl From<&PlaylistPlayerState> for CoarsePlayback {
    fn from(state: &PlaylistPlayerState) -> Self {
        Self {
            current_file_id: state.current_file_id.clone(),
            status: state.track.status,
            is_audible: state.track.is_audible,
            position: state.position,
            total_tracks: state.total_tracks,
            duration_seconds: state.track.duration_seconds,
            loop_mode: state.loop_mode,
            shuffle_enabled: state.shuffle_enabled,
            is_loading_for_playback: state.is_loading_for_playback,
            pending_crossfade_file_ids: state.pending_crossfade_file_ids.clone(),
        }
    }
}

impl Default for CoarsePlayback {
    fn default() -> Self {
        Self {
            current_file_id: None,
            status: TrackPlayerStatus::Idle,
            is_audible: false,
            position: -1,
            total_tracks: 0,
            duration_seconds: 0.0,
            loop_mode: LoopMode::Off,
            shuffle_enabled: false,
            is_loading_for_playback: false,
            pending_crossfade_file_ids: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlaybackSliceState {
    pub coarse: CoarsePlayback,
    /// Distinguishes "merely restored on launch" from "actually played this session": gating the OS/browser media session on the
    /// current track alone would surface media controls (and a playback state) on every launch, since the restore path decodes
    /// the last track silently without autoplaying it.
    pub has_started_playback: bool,
}

impl PlaybackSliceState {
    /// The player's coarse state changed.
    pub fn synced(&mut self, coarse: CoarsePlayback) {
        if coarse.status == TrackPlayerStatus::Playing {
            self.has_started_playback = true;
        }
        self.coarse = coarse;
    }

    /// Mirrors the player's state in only when its coarse part actually changed - so the ~5 Hz position ticks never touch the
    /// store. Returns whether anything changed.
    pub fn sync_if_changed(&mut self, player: &PlaylistPlayerState) -> bool {
        let coarse = CoarsePlayback::from(player);
        if coarse == self.coarse {
            return false;
        }
        self.synced(coarse);
        true
    }
}

#[cfg(test)]
mod tests {
    use beatseam_core::playback::TrackPlayerState;

    use super::*;

    fn player(status: TrackPlayerStatus, position_seconds: f64) -> PlaylistPlayerState {
        PlaylistPlayerState {
            total_tracks: 3,
            position: 1,
            current_file_id: Some("b".into()),
            loop_mode: LoopMode::All,
            shuffle_enabled: true,
            track: TrackPlayerState { status, is_audible: status == TrackPlayerStatus::Playing, position_seconds, duration_seconds: 200.0, pending_incoming: None },
            pending_crossfade_file_ids: None,
            is_loading_for_playback: false,
        }
    }

    #[test]
    fn mirrors_the_coarse_fields_and_remembers_that_playback_started() {
        let mut state = PlaybackSliceState::default();
        assert_eq!(state.coarse.position, -1);
        assert!(state.sync_if_changed(&player(TrackPlayerStatus::Stopped, 0.0)));
        assert!(!state.has_started_playback, "a silent restore is not a played track");
        assert!(state.sync_if_changed(&player(TrackPlayerStatus::Playing, 0.0)));
        assert!(state.has_started_playback);
        assert!(state.sync_if_changed(&player(TrackPlayerStatus::Paused, 5.0)));
        assert!(state.has_started_playback, "stays set once it has played");
        assert_eq!((state.coarse.current_file_id.as_deref(), state.coarse.duration_seconds, state.coarse.loop_mode), (Some("b"), 200.0, LoopMode::All));
    }

    #[test]
    fn position_ticks_alone_never_count_as_a_change() {
        let mut state = PlaybackSliceState::default();
        state.sync_if_changed(&player(TrackPlayerStatus::Playing, 1.0));
        assert!(!state.sync_if_changed(&player(TrackPlayerStatus::Playing, 1.2)));
        assert!(!state.sync_if_changed(&player(TrackPlayerStatus::Playing, 99.0)));
    }

    #[test]
    fn a_crossfade_starting_is_a_change() {
        let mut state = PlaybackSliceState::default();
        state.sync_if_changed(&player(TrackPlayerStatus::Playing, 1.0));
        let mut crossfading = player(TrackPlayerStatus::Playing, 1.0);
        crossfading.pending_crossfade_file_ids = Some(CrossfadeFileIds { outgoing: "b".into(), incoming: "c".into() });
        assert!(state.sync_if_changed(&crossfading));
        assert!(state.coarse.pending_crossfade_file_ids.is_some());
    }
}
