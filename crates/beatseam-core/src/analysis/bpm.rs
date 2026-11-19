#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BpmEstimate {
    pub bpm: f64,
    /// Normalized peak autocorrelation strength, roughly in `[0, 1]` - how confidently periodic the estimate is.
    pub confidence: f64,
    /// Seconds from the start of the analyzed window to the first beat that lines up with the detected period - the
    /// beat *phase*, not just the tempo. `bpm` alone says how far apart beats are; this says where one actually falls,
    /// which is what a transition needs to align two tracks' beat grids rather than just matching their rates.
    pub first_beat_offset_seconds: f64,
}

const MIN_BPM: f64 = 60.0;
const MAX_BPM: f64 = 200.0;
/// ~23 ms energy windows, 50% overlap - short enough to track individual beats, long enough to smooth out noise.
const ENVELOPE_WINDOW_SAMPLES: usize = 1024;
const ENVELOPE_HOP_SAMPLES: usize = 512;

use crate::js_compat::js_round;

/// Energy-per-window envelope of a mono signal, at a much lower "envelope rate" than the original sample rate.
/// Stored as `f32` like the TypeScript's `Float32Array`, so values round the same way.
fn compute_energy_envelope(samples: &[f32]) -> Vec<f32> {
    if samples.len() < ENVELOPE_WINDOW_SAMPLES {
        return Vec::new();
    }
    let frame_count = (samples.len() - ENVELOPE_WINDOW_SAMPLES) / ENVELOPE_HOP_SAMPLES + 1;
    (0..frame_count)
        .map(|frame| {
            let start = frame * ENVELOPE_HOP_SAMPLES;
            let sum_squares: f64 = samples[start..start + ENVELOPE_WINDOW_SAMPLES].iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
            (sum_squares / ENVELOPE_WINDOW_SAMPLES as f64) as f32
        })
        .collect()
}

/// Half-wave-rectified first difference - an "onset strength" signal that peaks sharply at beat attacks, not sustained
/// energy.
fn onset_strength(envelope: &[f32]) -> Vec<f32> {
    let mut onset = vec![0.0f32; envelope.len()];
    for i in 1..envelope.len() {
        onset[i] = (f64::from(envelope[i]) - f64::from(envelope[i - 1])).max(0.0) as f32;
    }
    onset
}

fn autocorrelate_at_lag(signal: &[f32], mean: f64, lag: usize) -> f64 {
    let n = signal.len() - lag;
    let mut sum = 0.0;
    for i in 0..n {
        sum += (f64::from(signal[i]) - mean) * (f64::from(signal[i + lag]) - mean);
    }
    sum
}

/// Given the detected beat period (`best_lag`, in envelope frames), finds which phase offset within that period best
/// lines up with the onset signal's peaks - a comb filter: for each candidate phase, sum the onset strength at phase,
/// phase+period, phase+2*period, ... and keep the phase with the highest sum. That phase is where an actual beat
/// falls, not just how far apart beats are.
fn find_best_phase(onset: &[f32], period: usize) -> usize {
    let mut best_phase = 0;
    let mut best_sum = f64::NEG_INFINITY;
    for phase in 0..period {
        let sum: f64 = (phase..onset.len()).step_by(period).map(|i| f64::from(onset[i])).sum();
        if sum > best_sum {
            best_sum = sum;
            best_phase = phase;
        }
    }
    best_phase
}

/// Windowed energy envelope + autocorrelation BPM estimator, plus a beat-phase pass on top, since a transition needs
/// to know where a beat actually falls in the analyzed window, not just the track's tempo. Operates on a single mono
/// channel (see `mix_to_mono`) over whatever PCM slice the caller passes in.
pub fn estimate_bpm(samples: &[f32], sample_rate: f64) -> BpmEstimate {
    let none = BpmEstimate { bpm: 0.0, confidence: 0.0, first_beat_offset_seconds: 0.0 };
    let envelope = compute_energy_envelope(samples);
    let onset = onset_strength(&envelope);
    let envelope_rate = sample_rate / ENVELOPE_HOP_SAMPLES as f64;

    let min_lag = js_round(envelope_rate * 60.0 / MAX_BPM).max(1.0);
    let max_lag = (onset.len() as f64 - 1.0).min(js_round(envelope_rate * 60.0 / MIN_BPM));

    if onset.is_empty() || min_lag >= max_lag {
        return none;
    }
    let (min_lag, max_lag) = (min_lag as usize, max_lag as usize);

    let mean = onset.iter().map(|v| f64::from(*v)).sum::<f64>() / onset.len() as f64;

    let zero_lag_energy = autocorrelate_at_lag(&onset, mean, 0);
    if zero_lag_energy <= 0.0 {
        return none;
    }

    let mut best_lag = min_lag;
    let mut best_value = f64::NEG_INFINITY;
    for lag in min_lag..=max_lag {
        let value = autocorrelate_at_lag(&onset, mean, lag);
        if value > best_value {
            best_value = value;
            best_lag = lag;
        }
    }

    let bpm = envelope_rate * 60.0 / best_lag as f64;
    let confidence = (best_value / zero_lag_energy).clamp(0.0, 1.0);
    let best_phase = find_best_phase(&onset, best_lag);
    let first_beat_offset_seconds = (best_phase * ENVELOPE_HOP_SAMPLES) as f64 / sample_rate;

    BpmEstimate { bpm, confidence, first_beat_offset_seconds }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f64 = 44100.0;

    /// A click train: a short burst of tone every beat, starting at `first_beat_seconds` - a clean synthetic stand-in
    /// for a track with a steady beat, so the recovered bpm/phase can be checked against a known-correct answer.
    fn click_train(duration_seconds: f64, bpm: f64, first_beat_seconds: f64) -> Vec<f32> {
        let period = 60.0 / bpm;
        let length = (duration_seconds * SR).round() as usize;
        let mut out = vec![0.0f32; length];
        let mut beat_time = first_beat_seconds;
        while beat_time < duration_seconds {
            let start = (beat_time * SR).round() as usize;
            let end = length.min(start + (0.05 * SR).round() as usize);
            for (i, sample) in out.iter_mut().enumerate().take(end).skip(start) {
                *sample = (0.8 * (2.0 * std::f64::consts::PI * 1000.0 * (i - start) as f64 / SR).sin()) as f32;
            }
            beat_time += period;
        }
        out
    }

    #[test]
    fn recovers_the_tempo_of_a_steady_click_train() {
        let e = estimate_bpm(&click_train(20.0, 120.0, 0.0), SR);
        assert!(e.bpm > 115.0 && e.bpm < 125.0 && e.confidence > 0.3, "{e:?}");
    }

    #[test]
    fn recovers_the_beat_phase_not_just_the_tempo() {
        let e = estimate_bpm(&click_train(20.0, 120.0, 0.2), SR);
        // The phase search only resolves to one envelope hop (~12 ms) and finds *a* beat consistent with the period,
        // not necessarily the very first one - it should land within one beat period of 0.2 s.
        let period = 0.5;
        let nearest = ((e.first_beat_offset_seconds - 0.2) / period).round() * period + 0.2;
        assert!((e.first_beat_offset_seconds - nearest).abs() < 0.05, "{e:?}");
    }

    #[test]
    fn finds_a_different_tempo_correctly() {
        let e = estimate_bpm(&click_train(20.0, 90.0, 0.0), SR);
        assert!(e.bpm > 85.0 && e.bpm < 95.0, "{e:?}");
    }

    #[test]
    fn has_zero_confidence_for_silence() {
        assert_eq!(estimate_bpm(&vec![0.0; (20.0 * SR) as usize], SR).confidence, 0.0);
    }

    #[test]
    fn does_not_fail_on_a_too_short_window() {
        assert_eq!(estimate_bpm(&[0.0; 100], SR), BpmEstimate { bpm: 0.0, confidence: 0.0, first_beat_offset_seconds: 0.0 });
        let _ = estimate_bpm(&[], SR);
    }
}
