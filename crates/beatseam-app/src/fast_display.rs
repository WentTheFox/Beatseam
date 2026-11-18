//! Port of `store/playback/fastDisplay`: what the now-playing UI derives from the player's fast-moving state.

use beatseam_core::crossfade::{TransitionPlan, compute_transition_plan, equal_power_gain, real_time_for_outgoing_position};
use beatseam_core::playback::{PlaylistPlayerState, TrackPlayerStatus};

/// Everything the now-playing UI derives from the player's fast-moving state (position, crossfade progress) - recomputed every tick,
/// so it lives in the few leaf views that draw it rather than at the app root. The slow parts (which track, its art and title,
/// whether it's playing) come from the store's coarse mirror instead.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FastDisplay {
    /// The incoming track's position/duration during a crossfade (the display switches to it immediately, instead of the bar climbing
    /// toward the outgoing track's length and snapping back at the swap), else the current track's.
    pub display_position_seconds: f64,
    pub display_duration_seconds: f64,
    /// The outgoing track's own progress through itself, 0-1 - feeds the tonearm; it keeps advancing independently of any pending
    /// crossfade.
    pub outgoing_progress: f64,
    /// Equal-power gains sampled at the current crossfade progress - the same curve the real audio fade uses, so the art dissolves at
    /// exactly the rate the audio does.
    pub outgoing_gain: f64,
    pub incoming_gain: f64,
}

pub fn compute_fast_display(state: &PlaylistPlayerState, crossfade_seconds: f64) -> FastDisplay {
    let track = &state.track;
    let pending = track.pending_incoming;

    // While a crossfade is genuinely happening, reflect its real duration (a manual skip's is much shorter than the natural
    // end-of-track default), not a static preview of a future one.
    let plan: Option<TransitionPlan> = match pending {
        Some(p) => Some(TransitionPlan { fade_start_seconds: 0.0, fade_duration_seconds: p.fade_duration_seconds, incoming_start_seconds: 0.0 }),
        None if track.duration_seconds > 0.0 => Some(compute_transition_plan(track.duration_seconds, crossfade_seconds)),
        None => None,
    };

    // The timeline is relative to the fade start (t = 0). In flight, the incoming position IS that elapsed time directly; otherwise
    // it's a preview of a future natural-end crossfade from the outgoing position.
    let progress_seconds = match (pending, &plan) {
        (Some(p), _) => Some(p.position_seconds),
        (None, Some(plan)) => Some(real_time_for_outgoing_position(plan, track.position_seconds)),
        (None, None) => None,
    };

    let fade_duration = plan.map_or(0.0, |p| p.fade_duration_seconds);
    let fraction = progress_seconds.map(|progress| if fade_duration > 0.0 { progress / fade_duration } else if progress >= 0.0 { 1.0 } else { 0.0 });

    // A crossfade only runs while playing, so a pending incoming implies playing; the override only matters when paused, where the
    // outgoing gain must be 0 even though `fraction` is some out-of-range value there.
    let is_playing = pending.is_some() || track.status == TrackPlayerStatus::Playing;

    FastDisplay {
        display_position_seconds: pending.map_or(track.position_seconds, |p| p.position_seconds),
        display_duration_seconds: pending.map_or(track.duration_seconds, |p| p.duration_seconds),
        outgoing_progress: if track.duration_seconds > 0.0 { track.position_seconds / track.duration_seconds } else { 0.0 },
        outgoing_gain: if is_playing { fraction.map_or(1.0, |f| equal_power_gain(f, true, fade_duration)) } else { 0.0 },
        incoming_gain: fraction.map_or(0.0, |f| equal_power_gain(f, false, fade_duration)),
    }
}

#[cfg(test)]
mod tests {
    use beatseam_core::library_store::LoopMode;
    use beatseam_core::playback::{PendingIncoming, TrackPlayerState};

    use super::*;

    fn state(status: TrackPlayerStatus, position: f64, duration: f64, pending: Option<PendingIncoming>) -> PlaylistPlayerState {
        PlaylistPlayerState {
            total_tracks: 2,
            position: 0,
            current_file_id: Some("a".into()),
            loop_mode: LoopMode::Off,
            shuffle_enabled: false,
            track: TrackPlayerState { status, is_audible: status == TrackPlayerStatus::Playing, position_seconds: position, duration_seconds: duration, pending_incoming: pending },
            pending_crossfade_file_ids: None,
            is_loading_for_playback: false,
        }
    }

    #[test]
    fn with_nothing_loaded_everything_is_zero_except_the_resting_outgoing_gain_of_a_playing_track() {
        let idle = compute_fast_display(&state(TrackPlayerStatus::Idle, 0.0, 0.0, None), 8.0);
        assert_eq!((idle.display_duration_seconds, idle.outgoing_progress, idle.outgoing_gain, idle.incoming_gain), (0.0, 0.0, 0.0, 0.0));
        let playing = compute_fast_display(&state(TrackPlayerStatus::Playing, 0.0, 0.0, None), 8.0);
        assert_eq!((playing.outgoing_gain, playing.incoming_gain), (1.0, 0.0), "no plan: the outgoing track is simply audible");
    }

    #[test]
    fn early_in_a_track_the_preview_shows_full_outgoing_gain_and_silent_incoming() {
        let d = compute_fast_display(&state(TrackPlayerStatus::Playing, 10.0, 200.0, None), 8.0);
        assert_eq!((d.display_position_seconds, d.display_duration_seconds, d.outgoing_progress), (10.0, 200.0, 0.05));
        assert!((d.outgoing_gain - 1.0).abs() < 1e-9 && d.incoming_gain.abs() < 1e-9);
    }

    #[test]
    fn the_preview_reaches_the_end_of_the_track_with_the_incoming_track_fully_in() {
        let d = compute_fast_display(&state(TrackPlayerStatus::Playing, 200.0, 200.0, None), 8.0);
        assert!(d.outgoing_gain.abs() < 1e-6 && (d.incoming_gain - 1.0).abs() < 1e-6, "{d:?}");
    }

    #[test]
    fn during_a_real_crossfade_the_display_switches_to_the_incoming_track_and_follows_its_own_fade_length() {
        let pending = PendingIncoming { position_seconds: 0.5, duration_seconds: 240.0, fade_duration_seconds: 1.0 };
        let d = compute_fast_display(&state(TrackPlayerStatus::Playing, 100.0, 200.0, Some(pending)), 8.0);
        assert_eq!((d.display_position_seconds, d.display_duration_seconds, d.outgoing_progress), (0.5, 240.0, 0.5));
        assert!((d.outgoing_gain - equal_power_gain(0.5, true, 1.0)).abs() < 1e-12);
        assert!((d.incoming_gain - equal_power_gain(0.5, false, 1.0)).abs() < 1e-12);
        assert!(d.outgoing_gain > 0.0 && d.incoming_gain > 0.0);
    }

    #[test]
    fn a_paused_track_has_no_outgoing_gain_whatever_the_preview_says() {
        let d = compute_fast_display(&state(TrackPlayerStatus::Paused, 10.0, 200.0, None), 8.0);
        assert_eq!(d.outgoing_gain, 0.0);
    }
}
