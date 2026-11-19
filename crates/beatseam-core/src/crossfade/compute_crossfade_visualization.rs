use super::compute_transition_plan::TransitionPlan;
use super::equal_power_gain::equal_power_gain;

#[derive(Clone, Debug, PartialEq)]
pub struct TrackVisualization {
    pub bpm: f64,
    /// Gain samples over `[timeline_start_seconds, timeline_end_seconds]`, evenly spaced.
    pub gain_curve: Vec<f64>,
    /// Beat timestamps (seconds, relative to the fade start at t=0) within the timeline.
    pub beat_times_seconds: Vec<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CrossfadeVisualization {
    /// The full visible timeline (seconds, relative to the fade start at t=0).
    pub timeline_start_seconds: f64,
    pub timeline_end_seconds: f64,
    /// Always equal (a zero-width "ramp" phase) - kept only so the preview's existing ramp-phase rendering degrades to
    /// its already-correct "fade only" path. No rate ramp exists this round - see `compute_transition_plan`.
    pub ramp_start_seconds: f64,
    pub ramp_end_seconds: f64,
    /// The audible gain crossfade spans `[0, fade_duration_seconds]` within the timeline.
    pub fade_duration_seconds: f64,
    pub outgoing: TrackVisualization,
    pub incoming: TrackVisualization,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CrossfadeVisualizationOptions {
    /// Seconds of the outgoing track's normal playback (before the fade) to include for context. Default 5.
    pub context_seconds_before: Option<f64>,
    /// Seconds of the incoming track's normal (post-fade) playback to include for context. Default 5.
    pub context_seconds_after: Option<f64>,
    /// How many points to sample each gain curve at. Default 60.
    pub sample_count: Option<usize>,
}

const DEFAULT_CONTEXT_SECONDS: f64 = 5.0;
const DEFAULT_SAMPLE_COUNT: usize = 60;
/// Safety valve on the beat loops - never reached at any sane bpm/timeline size.
const MAX_BEATS_EACH_WAY: i64 = 2000;

/// Real time (relative to the fade start at t=0) at which the outgoing track's *own* track-time position reaches
/// `target_track_position`. Both tracks play at a constant rate 1 throughout (no rate ramp this round), so this is
/// just a linear offset from the fade start. Exported so callers with a live playback position (e.g. a debug
/// preview's progress line) can convert it into this same timeline.
pub fn real_time_for_outgoing_position(plan: &TransitionPlan, target_track_position: f64) -> f64 {
    target_track_position - plan.fade_start_seconds
}

/// A bpm that cannot give a beat period (zero, negative, NaN or infinite). The TypeScript only rejected `<= 0`, which
/// let NaN fill the list with 2,000 NaN beats; no caller wants that.
fn usable_bpm(bpm: f64) -> bool {
    bpm > 0.0 && bpm.is_finite()
}

fn beat_times_for_outgoing(bpm: f64, plan: &TransitionPlan, timeline_start_seconds: f64) -> Vec<f64> {
    if !usable_bpm(bpm) {
        return Vec::new();
    }
    let period = 60.0 / bpm;
    let mut after = Vec::new();
    for n in 0..=MAX_BEATS_EACH_WAY + 1 {
        let t = n as f64 * period;
        if t > plan.fade_duration_seconds + 1e-9 {
            break;
        }
        if t >= timeline_start_seconds - 1e-9 {
            after.push(t);
        }
    }
    // Beats before the fade starts (negative times), nearest first, then reversed into ascending order.
    let mut before = Vec::new();
    for n in (-MAX_BEATS_EACH_WAY - 1..=-1).rev() {
        let t = n as f64 * period;
        if t < timeline_start_seconds - 1e-9 {
            break;
        }
        before.push(t);
    }
    before.reverse();
    before.extend(after);
    before
}

fn beat_times_for_incoming(bpm: f64, timeline_end_seconds: f64) -> Vec<f64> {
    if !usable_bpm(bpm) {
        return Vec::new();
    }
    let period = 60.0 / bpm;
    let mut times = Vec::new();
    for n in 0..=MAX_BEATS_EACH_WAY + 1 {
        let t = n as f64 * period;
        if t > timeline_end_seconds + 1e-9 {
            break;
        }
        times.push(t);
    }
    times
}

fn sample_gain_curve(timeline_start: f64, timeline_end: f64, fade_duration: f64, is_outgoing: bool, sample_count: usize) -> Vec<f64> {
    let span = timeline_end - timeline_start;
    (0..=sample_count)
        .map(|i| {
            let t = timeline_start + if span > 0.0 { i as f64 / sample_count as f64 * span } else { 0.0 };
            let fraction = if fade_duration > 0.0 {
                t / fade_duration
            } else if t >= 0.0 {
                1.0
            } else {
                0.0
            };
            equal_power_gain(fraction, is_outgoing, fade_duration)
        })
        .collect()
}

/// Sample data for rendering the crossfade - the same [`TransitionPlan`] that drives actual playback scheduling,
/// turned into something a debug view can draw, so what's shown is provably the same math that will actually run, not
/// a separate approximation of it. Includes a few seconds of normal playback before the fade begins and after it
/// completes, for context.
pub fn compute_crossfade_visualization(
    plan: &TransitionPlan,
    outgoing_bpm: f64,
    incoming_bpm: f64,
    options: CrossfadeVisualizationOptions,
) -> CrossfadeVisualization {
    let context_before = options.context_seconds_before.unwrap_or(DEFAULT_CONTEXT_SECONDS);
    let context_after = options.context_seconds_after.unwrap_or(DEFAULT_CONTEXT_SECONDS);
    let sample_count = options.sample_count.unwrap_or(DEFAULT_SAMPLE_COUNT);
    let timeline_start_seconds = -context_before;
    let timeline_end_seconds = plan.fade_duration_seconds + context_after;

    CrossfadeVisualization {
        timeline_start_seconds,
        timeline_end_seconds,
        ramp_start_seconds: 0.0,
        ramp_end_seconds: 0.0,
        fade_duration_seconds: plan.fade_duration_seconds,
        outgoing: TrackVisualization {
            bpm: outgoing_bpm,
            gain_curve: sample_gain_curve(timeline_start_seconds, timeline_end_seconds, plan.fade_duration_seconds, true, sample_count),
            beat_times_seconds: beat_times_for_outgoing(outgoing_bpm, plan, timeline_start_seconds),
        },
        incoming: TrackVisualization {
            bpm: incoming_bpm,
            gain_curve: sample_gain_curve(timeline_start_seconds, timeline_end_seconds, plan.fade_duration_seconds, false, sample_count),
            beat_times_seconds: beat_times_for_incoming(incoming_bpm, timeline_end_seconds),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crossfade::compute_transition_plan;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    fn gaps(times: &[f64]) -> Vec<f64> {
        times.windows(2).map(|w| w[1] - w[0]).collect()
    }

    #[test]
    fn produces_gain_curves_that_fade_outgoing_1_to_0_and_incoming_0_to_1() {
        let viz = compute_crossfade_visualization(&compute_transition_plan(180.0, 5.0), 120.0, 120.0, Default::default());
        // The curves span the whole timeline (context included), so the ends sit in the held-at-1 / held-at-0 regions.
        assert!(close(viz.outgoing.gain_curve[0], 1.0) && close(*viz.outgoing.gain_curve.last().unwrap(), 0.0));
        assert!(close(viz.incoming.gain_curve[0], 0.0) && close(*viz.incoming.gain_curve.last().unwrap(), 1.0));
        assert_eq!(viz.outgoing.gain_curve.len(), 61);
    }

    #[test]
    fn never_produces_an_outgoing_beat_time_past_the_fade_duration() {
        let viz = compute_crossfade_visualization(&compute_transition_plan(180.0, 5.0), 174.0, 174.0, Default::default());
        assert!(viz.outgoing.beat_times_seconds.iter().all(|t| *t <= viz.fade_duration_seconds + 1e-6));
    }

    #[test]
    fn spaces_beats_evenly_at_the_given_bpm_on_both_lanes() {
        let viz = compute_crossfade_visualization(&compute_transition_plan(180.0, 6.0), 120.0, 100.0, Default::default());
        assert!(gaps(&viz.outgoing.beat_times_seconds).iter().all(|g| close(*g, 0.5)));
        assert!(gaps(&viz.incoming.beat_times_seconds).iter().all(|g| close(*g, 0.6)));
    }

    #[test]
    fn reports_a_zero_width_ramp_window_and_includes_context_before_and_after() {
        let plan = compute_transition_plan(300.0, 5.0);
        let options = CrossfadeVisualizationOptions { context_seconds_before: Some(5.0), context_seconds_after: Some(5.0), ..Default::default() };
        let viz = compute_crossfade_visualization(&plan, 90.0, 130.0, options);

        assert_eq!((viz.ramp_start_seconds, viz.ramp_end_seconds), (0.0, 0.0));
        assert!(close(viz.timeline_start_seconds, -5.0));
        assert!(close(viz.timeline_end_seconds, plan.fade_duration_seconds + 5.0));
        assert!(viz.outgoing.beat_times_seconds.iter().any(|t| *t < 0.0));
        assert!(viz.incoming.beat_times_seconds.iter().any(|t| *t > plan.fade_duration_seconds));
        // Outgoing has already faded out and stopped playing after the transition.
        assert!(!viz.outgoing.beat_times_seconds.iter().any(|t| *t > plan.fade_duration_seconds));
        // Incoming hasn't started yet before the fade begins.
        assert!(!viz.incoming.beat_times_seconds.iter().any(|t| *t < 0.0));
    }

    #[test]
    fn returns_no_beats_when_a_bpm_is_unusable() {
        let plan = compute_transition_plan(180.0, 5.0);
        let viz = compute_crossfade_visualization(&plan, 0.0, 128.0, Default::default());
        assert!(viz.outgoing.beat_times_seconds.is_empty());
        assert!(!viz.incoming.beat_times_seconds.is_empty());
        for bpm in [-1.0, f64::NAN, f64::INFINITY] {
            let v = compute_crossfade_visualization(&plan, bpm, bpm, Default::default());
            assert!(v.outgoing.beat_times_seconds.is_empty() && v.incoming.beat_times_seconds.is_empty(), "{bpm}");
        }
    }

    #[test]
    fn outgoing_beats_are_ascending_and_include_the_beat_at_zero() {
        let viz = compute_crossfade_visualization(&compute_transition_plan(180.0, 6.0), 120.0, 120.0, Default::default());
        let beats = &viz.outgoing.beat_times_seconds;
        assert!(beats.windows(2).all(|w| w[0] < w[1]));
        assert!(beats.contains(&0.0));
        assert!(close(beats[0], -5.0)); // 10 beats of 0.5 s back to the timeline start
        assert!(close(*beats.last().unwrap(), 6.0));
    }

    #[test]
    fn a_zero_length_fade_still_samples_without_dividing_by_zero() {
        let viz = compute_crossfade_visualization(&compute_transition_plan(180.0, 0.0), 120.0, 120.0, Default::default());
        assert!(viz.outgoing.gain_curve.iter().all(|g| g.is_finite()));
        // Before t=0 the outgoing track is at full gain, from t=0 on it is silent.
        assert!(close(viz.outgoing.gain_curve[0], 1.0) && close(*viz.outgoing.gain_curve.last().unwrap(), 0.0));
    }

    #[test]
    fn real_time_is_a_linear_offset_from_fade_start() {
        let plan = compute_transition_plan(180.0, 5.0);
        assert_eq!(real_time_for_outgoing_position(&plan, plan.fade_start_seconds), 0.0);
        assert_eq!(real_time_for_outgoing_position(&plan, plan.fade_start_seconds + 10.0), 10.0);
    }
}
