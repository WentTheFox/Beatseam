use super::mono::mix_to_mono;
use crate::audio::DecodedAudio;

/// -45 dBFS - well above noise floor, well below any audible content.
const SILENCE_AMPLITUDE_THRESHOLD: f64 = 0.0056;
/// ~20 ms per window - short enough to find the true edge of the content closely.
const WINDOW_SECONDS: f64 = 0.02;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentBounds {
    pub start_sample: usize,
    pub end_sample: usize,
}

fn window_rms(samples: &[f32], start_sample: usize, window_length: usize) -> f64 {
    let end = (start_sample + window_length).min(samples.len());
    if end <= start_sample {
        return 0.0;
    }
    let sum_squares: f64 = samples[start_sample..end].iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
    (sum_squares / (end - start_sample) as f64).sqrt()
}

/// Finds the `[start_sample, end_sample)` range of actual audible content, trimming leading/trailing near-silence
/// (fade-ins/outs, dead air): without it, a track whose literal last 30 s is mostly silence would give the BPM/loudness
/// analysis (and a crossfade window) far less than 30 s of real musical content to work with.
///
/// Returns the full range unchanged if the track never exceeds the silence threshold (nothing to trim, or the whole
/// track is silent).
///
/// `mono` lets a caller that already downmixed the track (`analyze_track` does) pass it straight in instead of this
/// doing that full-track pass again; `None` downmixes `audio` itself.
pub fn find_content_bounds(audio: &DecodedAudio, mono: Option<&[f32]>) -> ContentBounds {
    let owned;
    let mono_samples = match mono {
        Some(m) => m,
        None => {
            owned = mix_to_mono(audio);
            &owned
        }
    };
    let total = mono_samples.len();
    // Math.round(0.02 * sampleRate), at least 1 (halves round up, like JS)
    let window_length = ((WINDOW_SECONDS * f64::from(audio.sample_rate) + 0.5).floor() as usize).max(1);

    let mut start_sample = 0;
    let mut i = 0;
    while i < total {
        if window_rms(mono_samples, i, window_length) >= SILENCE_AMPLITUDE_THRESHOLD {
            start_sample = i;
            break;
        }
        start_sample = total; // reached the end without finding content
        i += window_length;
    }

    let mut end_sample = total;
    // Signed: for a track shorter than one window the first candidate start is negative.
    let mut i = total as i64 - window_length as i64;
    while i + window_length as i64 > start_sample as i64 {
        let from = i.max(0) as usize;
        if window_rms(mono_samples, from, window_length) >= SILENCE_AMPLITUDE_THRESHOLD {
            end_sample = from + window_length;
            break;
        }
        end_sample = start_sample; // reached the start without finding content
        i -= window_length as i64;
    }

    if start_sample >= end_sample {
        // Entirely silent (or degenerate) track - nothing usable to trim to, fall back to the untrimmed range rather
        // than an empty one.
        return ContentBounds { start_sample: 0, end_sample: total };
    }
    ContentBounds { start_sample, end_sample: end_sample.min(total) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 44100;

    fn tone(seconds: f64, amplitude: f64) -> Vec<f32> {
        let n = (seconds * f64::from(SR)).round() as usize;
        (0..n).map(|i| (amplitude * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / f64::from(SR)).sin()) as f32).collect()
    }

    fn silence(seconds: f64) -> Vec<f32> {
        vec![0.0; (seconds * f64::from(SR)).round() as usize]
    }

    fn audio(channels: Vec<Vec<f32>>) -> DecodedAudio {
        DecodedAudio { sample_rate: SR, channel_data: channels }
    }

    fn secs(samples: usize) -> f64 {
        samples as f64 / f64::from(SR)
    }

    #[test]
    fn trims_leading_and_trailing_silence_keeping_only_the_audible_middle() {
        let samples = [silence(2.0), tone(3.0, 0.5), silence(2.0)].concat();
        let b = find_content_bounds(&audio(vec![samples]), None);
        assert!((secs(b.start_sample) - 2.0).abs() < 0.05 && (secs(b.end_sample) - 5.0).abs() < 0.05, "{b:?}");
    }

    #[test]
    fn leaves_an_already_tight_track_unchanged() {
        let samples = tone(3.0, 0.5);
        let len = samples.len();
        let b = find_content_bounds(&audio(vec![samples]), None);
        assert_eq!(b.start_sample, 0);
        assert!(b.end_sample > len - 1000);
    }

    #[test]
    fn falls_back_to_the_full_range_for_an_entirely_silent_track() {
        let a = audio(vec![silence(3.0)]);
        let b = find_content_bounds(&a, None);
        assert_eq!((b.start_sample, b.end_sample), (0, a.channel_data[0].len()));
    }

    #[test]
    fn mixes_multiple_channels_down_before_measuring_not_just_the_first() {
        let left = silence(5.0);
        let right = [silence(2.0), tone(1.0, 0.5), silence(2.0)].concat();
        // Half amplitude after the mono mix (0.25) is still well above the -45 dBFS threshold.
        let b = find_content_bounds(&audio(vec![left, right]), None);
        assert!((secs(b.start_sample) - 2.0).abs() < 0.05 && (secs(b.end_sample) - 3.0).abs() < 0.05, "{b:?}");
    }

    #[test]
    fn a_premixed_mono_signal_gives_the_same_answer_as_letting_it_mix() {
        let a = audio(vec![[silence(1.0), tone(1.0, 0.5)].concat(), [silence(1.0), tone(1.0, 0.1)].concat()]);
        let mono = mix_to_mono(&a);
        assert_eq!(find_content_bounds(&a, Some(&mono)), find_content_bounds(&a, None));
    }

    #[test]
    fn tracks_shorter_than_one_window_do_not_panic() {
        for n in [0usize, 1, 100, 881, 882, 883] {
            let _ = find_content_bounds(&audio(vec![vec![0.5; n]]), None);
            let _ = find_content_bounds(&audio(vec![vec![0.0; n]]), None);
        }
    }
}
