/// Reference loudness target, in dBFS (RMS). Every track's normalization gain is computed relative to this single fixed
/// target - not a "correct" value in any broadcast-standard sense, just a consistent reference point so a quiet and a
/// loud track in the library end up sounding similarly loud.
const REFERENCE_LOUDNESS_DB: f64 = -18.0;

/// Clamp bounds (linear amplitude) so a near-silent or clipping-loud outlier track doesn't get an absurd gain.
const MIN_GAIN: f64 = 0.25; // -12 dB
const MAX_GAIN: f64 = 4.0; // +12 dB

const SILENCE_FLOOR_DB: f64 = -80.0;

/// RMS loudness of a signal, in dBFS. The silence floor for true silence (and for an empty signal).
pub fn measure_loudness_db(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return SILENCE_FLOOR_DB;
    }
    let sum_squares: f64 = samples.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
    let rms = (sum_squares / samples.len() as f64).sqrt();
    if rms <= 0.0 {
        return SILENCE_FLOOR_DB;
    }
    SILENCE_FLOOR_DB.max(20.0 * rms.log10())
}

/// Gain multiplier (linear amplitude) to bring a track measured at `loudness_db` to the reference target.
pub fn compute_normalization_gain(loudness_db: f64) -> f64 {
    let gain_db = REFERENCE_LOUDNESS_DB - loudness_db;
    let gain = 10f64.powf(gain_db / 20.0);
    gain.max(MIN_GAIN).min(MAX_GAIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn constant(value: f32) -> Vec<f32> {
        vec![value; 10_000]
    }

    #[test]
    fn measures_full_scale_amplitude_as_0_dbfs() {
        assert!(measure_loudness_db(&constant(1.0)).abs() < 0.05);
    }

    #[test]
    fn measures_half_amplitude_as_roughly_minus_6_db() {
        assert!((measure_loudness_db(&constant(0.5)) + 6.0).abs() < 0.5);
    }

    #[test]
    fn floors_silence_instead_of_returning_negative_infinity() {
        assert_eq!(measure_loudness_db(&constant(0.0)), -80.0);
        assert_eq!(measure_loudness_db(&[]), -80.0);
    }

    #[test]
    fn a_signal_quieter_than_the_floor_is_clamped_to_it() {
        assert_eq!(measure_loudness_db(&constant(1e-6)), -80.0); // -120 dBFS
    }

    #[test]
    fn boosts_a_quieter_than_target_track() {
        assert!(compute_normalization_gain(-30.0) > 1.0);
    }

    #[test]
    fn attenuates_a_louder_than_target_track() {
        assert!(compute_normalization_gain(-6.0) < 1.0);
    }

    #[test]
    fn leaves_an_at_target_track_roughly_unchanged() {
        assert!((compute_normalization_gain(-18.0) - 1.0).abs() < 0.05);
    }

    #[test]
    fn clamps_extreme_boosts_and_attenuation() {
        assert_eq!(compute_normalization_gain(-80.0), 4.0);
        assert_eq!(compute_normalization_gain(0.0), 0.25);
    }
}
