#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransitionPlan {
    /// Position (seconds) in the OUTGOING track's own timeline where the audible gain crossfade begins - a fixed offset
    /// from the track's end, not beat-snapped.
    pub fade_start_seconds: f64,
    /// How long the audible gain crossfade lasts (seconds).
    pub fade_duration_seconds: f64,
    /// Position (seconds) in the INCOMING track to start playback from - always its very beginning for now.
    pub incoming_start_seconds: f64,
}

/// Computes a crossfade transition plan between two tracks.
///
/// This used to compute a beat-snapped tempo ramp too (the outgoing track sped up/slowed down to match the incoming
/// track's BPM before the fade). Per the crossfade rework in CLAUDE.md, rate/speed manipulation is dropped for this
/// round entirely - BPM matching becomes a live, "past few seconds" concern for a later round, not something this plan
/// computes up front. So this is a fixed-lead-time, volume-only fade: no analysis input needed at all, just durations.
pub fn compute_transition_plan(outgoing_duration_seconds: f64, crossfade_seconds: f64) -> TransitionPlan {
    let fade_duration_seconds = crossfade_seconds.max(0.0);
    let fade_start_seconds = (outgoing_duration_seconds - fade_duration_seconds).max(0.0);
    TransitionPlan { fade_start_seconds, fade_duration_seconds, incoming_start_seconds: 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_the_fade_fade_duration_seconds_before_the_track_ends() {
        let plan = compute_transition_plan(180.0, 7.0);
        assert_eq!(plan.fade_duration_seconds, 7.0);
        assert_eq!(plan.fade_start_seconds, 173.0);
    }

    #[test]
    fn starts_the_incoming_track_from_its_very_beginning() {
        assert_eq!(compute_transition_plan(180.0, 5.0).incoming_start_seconds, 0.0);
    }

    #[test]
    fn clamps_a_negative_or_zero_crossfade_duration_to_0_rather_than_failing() {
        assert_eq!(compute_transition_plan(180.0, -3.0).fade_duration_seconds, 0.0);
        assert_eq!(compute_transition_plan(180.0, 0.0).fade_start_seconds, 180.0);
    }

    #[test]
    fn never_starts_the_fade_before_0_even_when_the_crossfade_is_longer_than_the_whole_track() {
        assert_eq!(compute_transition_plan(3.0, 10.0).fade_start_seconds, 0.0);
    }
}
