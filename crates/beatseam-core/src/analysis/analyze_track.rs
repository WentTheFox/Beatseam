use super::loudness::{compute_normalization_gain, measure_loudness_db};
use super::mono::mix_to_mono;
use super::silence::find_content_bounds;
use crate::audio::DecodedAudio;

/// Bump this whenever a change to `analyze_track`/loudness would produce different results for a file that's already
/// been analyzed - a stored result whose algorithm version doesn't match this is treated the same as a changed file
/// size/mtime: stale, recompute. Without this, an algorithm fix silently never applies to already-analyzed files until
/// someone manually clears the analysis table.
///
/// Shared with the TypeScript apps (it is stored in the same database), so it only moves when the *behaviour* does.
pub const ANALYSIS_ALGORITHM_VERSION: u32 = 2;

const ANALYSIS_WINDOW_SECONDS: f64 = 30.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackAnalysis {
    pub normalization_gain: f64,
}

/// Loudness-only analysis over a decoded track: trim leading/trailing silence, then measure loudness over the pooled
/// first/last `ANALYSIS_WINDOW_SECONDS` of the remaining content (deduping the overlap on a short track rather than
/// double-counting it) to compute a normalization gain.
///
/// The TypeScript also used to run a whole-track BPM/beat-anchor search over each end; per the crossfade rework in
/// CLAUDE.md, BPM matching is now a live "past few seconds" concern instead of a precomputed one, so only loudness
/// remains, since there's no live-calibrated replacement for normalization gain yet.
///
/// Synchronous. The TypeScript yields to the event loop between phases because it runs on the UI thread; here, call it
/// from a worker thread or executor (it is a few full passes over the decoded samples).
pub fn analyze_track(audio: &DecodedAudio) -> TrackAnalysis {
    let mono = mix_to_mono(audio);
    let bounds = find_content_bounds(audio, Some(&mono));

    let window_samples = (ANALYSIS_WINDOW_SECONDS * f64::from(audio.sample_rate) + 0.5).floor() as usize;
    let first_window_end = (bounds.start_sample + window_samples).min(bounds.end_sample);
    let last_window_start = bounds.end_sample.saturating_sub(window_samples).max(bounds.start_sample);

    // Loudness is a whole-track perceptual property, not a per-side one, so it's measured over both windows pooled
    // together (deduping the overlap on a short track rather than double-counting it).
    let pooled: Vec<f32> = if last_window_start <= first_window_end {
        mono[bounds.start_sample..bounds.end_sample].to_vec()
    } else {
        [&mono[bounds.start_sample..first_window_end], &mono[last_window_start..bounds.end_sample]].concat()
    };
    TrackAnalysis { normalization_gain: compute_normalization_gain(measure_loudness_db(&pooled)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 8000;

    fn square(seconds: f64, amplitude: f32) -> Vec<f32> {
        (0..(seconds * f64::from(SR)) as usize).map(|i| if (i / 20) % 2 == 0 { amplitude } else { -amplitude }).collect()
    }

    fn audio(samples: Vec<f32>) -> DecodedAudio {
        DecodedAudio { sample_rate: SR, channel_data: vec![samples] }
    }

    #[test]
    fn a_track_at_the_reference_loudness_gets_a_gain_of_about_one() {
        // A square wave of amplitude a has RMS a; -18 dBFS is about 0.1259.
        let g = analyze_track(&audio(square(5.0, 0.1259))).normalization_gain;
        assert!((g - 1.0).abs() < 0.01, "{g}");
    }

    #[test]
    fn quiet_tracks_are_boosted_and_loud_ones_attenuated_within_the_clamp() {
        assert!(analyze_track(&audio(square(5.0, 0.02))).normalization_gain > 2.0);
        assert!(analyze_track(&audio(square(5.0, 0.9))).normalization_gain < 0.5);
        assert_eq!(analyze_track(&audio(square(5.0, 0.0001))).normalization_gain, 4.0);
    }

    #[test]
    fn leading_and_trailing_silence_do_not_drag_the_loudness_down() {
        let plain = analyze_track(&audio(square(5.0, 0.3))).normalization_gain;
        let padded = analyze_track(&audio([vec![0.0; 3 * SR as usize], square(5.0, 0.3), vec![0.0; 3 * SR as usize]].concat())).normalization_gain;
        assert!((plain - padded).abs() < 0.01, "{plain} vs {padded}");
    }

    #[test]
    fn a_long_track_measures_only_the_first_and_last_30_seconds() {
        // 100 s: loud first 30 s, very quiet middle, loud last 30 s. The middle must not count.
        let loud = square(30.0, 0.3);
        let samples = [loud.clone(), square(40.0, 0.01), loud].concat();
        let long = analyze_track(&audio(samples)).normalization_gain;
        let short = analyze_track(&audio(square(60.0, 0.3))).normalization_gain;
        assert!((long - short).abs() < 0.01, "{long} vs {short}");
    }

    #[test]
    fn silence_and_empty_input_do_not_fail() {
        assert_eq!(analyze_track(&audio(vec![0.0; 10_000])).normalization_gain, 4.0);
        assert_eq!(analyze_track(&audio(vec![])).normalization_gain, 4.0);
    }

    #[test]
    fn the_algorithm_version_matches_the_typescript() {
        assert_eq!(ANALYSIS_ALGORITHM_VERSION, 2);
    }
}
