use std::f64::consts::FRAC_PI_2;

/// How much a plain `fraction * pi/2` angle's rate of change accelerates toward the very end of `[0, 1]` scales
/// inversely with the fade's own duration - a long fade's ending is already gentle in absolute (real-time) terms, but
/// a short one's isn't, and reads as an abrupt cutoff rather than a fade to silence (confirmed on-device: fine at the
/// natural end-of-track crossfade's default ~8 s duration, audibly abrupt at a manual skip's ~1 s one). Rather than a
/// hard on/off threshold - which would make the curve's own shape visibly discontinuous right at that boundary as the
/// crossfade length is adjusted - [`eased_fraction`] blends smoothly from the original shape (long fades) toward the
/// fully-eased one (very short fades), governed by this reference duration: a fade at exactly this length is eased
/// halfway.
const EASE_REFERENCE_SECONDS: f64 = 1.0;

/// Blends `fraction` toward `2*fraction - fraction^2` (zero slope at both ends of `[0, 1]`, not just the start) by an
/// amount that grows as `duration_seconds` shrinks - see [`EASE_REFERENCE_SECONDS`].
fn eased_fraction(fraction: f64, duration_seconds: f64) -> f64 {
    let ease_amount = if duration_seconds > 0.0 { (EASE_REFERENCE_SECONDS / duration_seconds).min(1.0) } else { 1.0 };
    fraction + ease_amount * fraction * (1.0 - fraction)
}

/// Equal-power crossfade gain at a given `fraction` in `[0, 1]` through a transition of `duration_seconds` seconds. A
/// straight linear fade (gain = fraction) spends much of its duration sounding under-loud on the fading-in side (and
/// abruptly loud then long-tailed-quiet on the fading-out side), because human loudness perception is roughly
/// logarithmic, not linear in gain - a 20 s linear fade-in is still under 10% of its target gain 2 seconds in, which
/// reads as "not playing yet." The cos/sin pair here keeps `outgoing^2 + incoming^2` constant across the whole
/// transition, so combined perceived loudness stays roughly level instead of dipping in the middle.
///
/// The fraction is eased (see [`eased_fraction`]) before being turned into an angle, rather than used directly - since
/// it's applied to the shared angle, `cos(eased)^2 + sin(eased)^2` is still identically 1 for any eased value, so
/// equal-power is unaffected regardless of how much easing is applied.
///
/// This is the curve the real engine must use. (The Phase 0 audio spike's mixer used a plain `cos`/`sin` of
/// `fraction * pi/2` with no easing; that is the `duration >= 1 s`-and-up shape only approximately.)
pub fn equal_power_gain(fraction: f64, is_outgoing: bool, duration_seconds: f64) -> f64 {
    let clamped = fraction.clamp(0.0, 1.0);
    let angle = eased_fraction(clamped, duration_seconds) * FRAC_PI_2;
    if is_outgoing { angle.cos() } else { angle.sin() }
}

/// Samples the curve at `sample_count + 1` evenly-spaced points across `[0, 1]` - shared by both the real playback
/// scheduling and the debug preview, so what's drawn matches what actually plays.
pub fn sample_equal_power_gain_curve(sample_count: usize, is_outgoing: bool, duration_seconds: f64) -> Vec<f64> {
    (0..=sample_count)
        .map(|i| equal_power_gain(i as f64 / sample_count as f64, is_outgoing, duration_seconds))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DURATIONS: [f64; 4] = [0.5, 1.0, 8.0, 20.0];

    #[test]
    fn always_starts_outgoing_at_1_and_incoming_at_0_regardless_of_duration() {
        for d in DURATIONS {
            assert!((equal_power_gain(0.0, true, d) - 1.0).abs() < 1e-9);
            assert!(equal_power_gain(0.0, false, d).abs() < 1e-9);
        }
    }

    #[test]
    fn always_ends_outgoing_at_0_and_incoming_at_1_regardless_of_duration() {
        for d in DURATIONS {
            assert!(equal_power_gain(1.0, true, d).abs() < 1e-9);
            assert!((equal_power_gain(1.0, false, d) - 1.0).abs() < 1e-9);
        }
    }

    #[test]
    fn keeps_outgoing_squared_plus_incoming_squared_constant_at_any_duration() {
        for d in DURATIONS {
            for f in [0.0, 0.25, 0.5, 0.75, 1.0] {
                let (o, i) = (equal_power_gain(f, true, d), equal_power_gain(f, false, d));
                assert!((o * o + i * i - 1.0).abs() < 1e-9, "d={d} f={f}");
            }
        }
    }

    #[test]
    fn front_loads_more_of_a_short_fades_drop_earlier_than_a_long_one() {
        assert!(equal_power_gain(0.9, true, 1.0) < equal_power_gain(0.9, true, 8.0));
    }

    #[test]
    fn has_near_zero_slope_at_both_ends_for_a_very_short_fade() {
        let (d, eps) = (0.5, 1e-4);
        let start = (equal_power_gain(eps, true, d) - equal_power_gain(0.0, true, d)) / eps;
        let end = (equal_power_gain(1.0, true, d) - equal_power_gain(1.0 - eps, true, d)) / eps;
        assert!(start.abs() < 0.1 && end.abs() < 0.1, "{start} {end}");
    }

    /// (duration, fraction, outgoing, incoming) computed by the TypeScript implementation (Node, f64), including
    /// out-of-range fractions (clamped) and a zero duration (fully eased).
    #[rustfmt::skip]
    const FROM_TS: &[(f64, f64, f64, f64)] = &[
        (0.0, -0.5, 1.0, 0.0), (0.0, 0.0, 1.0, 0.0),
        (0.0, 0.1, 0.9557930147983301, 0.29404032523230395),
        (0.0, 0.25, 0.773010453362737, 0.6343932841636455),
        (0.0, 0.5, 0.38268343236508984, 0.9238795325112867),
        (0.0, 0.9, 0.015707317311820648, 0.9998766324816606),
        (0.0, 1.0, 6.123233995736766e-17, 1.0), (0.0, 1.5, 6.123233995736766e-17, 1.0),
        (0.5, 0.1, 0.9557930147983301, 0.29404032523230395),
        (0.5, 0.5, 0.38268343236508984, 0.9238795325112867),
        (1.0, 0.25, 0.773010453362737, 0.6343932841636455),
        (1.0, 0.9, 0.015707317311820648, 0.9998766324816606),
        (8.0, -0.5, 1.0, 0.0), (8.0, 0.0, 1.0, 0.0),
        (8.0, 0.1, 0.9847698454203168, 0.17386302525507108),
        (8.0, 0.25, 0.9091679830905224, 0.41642956009763715),
        (8.0, 0.5, 0.6715589548470184, 0.7409511253549591),
        (8.0, 0.9, 0.1389570546712437, 0.990298408035221),
        (8.0, 1.0, 6.123233995736766e-17, 1.0), (8.0, 1.5, 6.123233995736766e-17, 1.0),
        (20.0, 0.1, 0.9865579049714237, 0.1634120562822566),
        (20.0, 0.25, 0.9181440823722039, 0.3962466958914656),
        (20.0, 0.5, 0.693087362545636, 0.7208535967029187),
        (20.0, 0.9, 0.14944905760268384, 0.98876942670254),
        (20.0, 1.0, 6.123233995736766e-17, 1.0), (20.0, 1.5, 6.123233995736766e-17, 1.0),
    ];

    #[test]
    fn matches_the_typescript_implementation_numerically() {
        for &(d, f, out, inc) in FROM_TS {
            assert!((equal_power_gain(f, true, d) - out).abs() < 1e-12, "outgoing d={d} f={f}");
            assert!((equal_power_gain(f, false, d) - inc).abs() < 1e-12, "incoming d={d} f={f}");
        }
    }

    #[test]
    fn sampling_gives_sample_count_plus_one_points_ending_at_the_curve_ends() {
        let curve = sample_equal_power_gain_curve(60, true, 8.0);
        assert_eq!(curve.len(), 61);
        assert!((curve[0] - 1.0).abs() < 1e-12 && curve[60].abs() < 1e-12);
        assert!(curve.windows(2).all(|w| w[1] <= w[0]), "outgoing never rises");
    }
}
