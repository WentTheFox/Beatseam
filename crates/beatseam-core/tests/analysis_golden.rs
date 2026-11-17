//! Differential test for the analysis port: the same synthetic signals (rebuilt here from recipes, with the same
//! arithmetic) go through the Rust code, and the results are compared with what the original TypeScript produced
//! (`tools/gen-analysis-golden.mjs` -> `tests/data/analysis_golden.json`).

use beatseam_core::analysis::{analyze_track, compute_normalization_gain, estimate_bpm, find_content_bounds, measure_loudness_db, mix_to_mono};
use beatseam_core::audio::DecodedAudio;
use serde_json::Value;

fn build(r: &Value) -> Vec<f32> {
    let f = |k: &str| r[k].as_f64().unwrap();
    match r["k"].as_str().unwrap() {
        "silence" => vec![0.0; f("n") as usize],
        "square" => {
            let (amp, hz, sr) = (f("amp"), f("hz"), f("sr"));
            (0..f("n") as usize).map(|i| (amp * if (i as f64 * 2.0 * hz / sr).floor() % 2.0 == 0.0 { 1.0 } else { -1.0 }) as f32).collect()
        }
        "noise" => {
            let mut s = r["seed"].as_u64().unwrap() as u32;
            let amp = f("amp");
            (0..f("n") as usize)
                .map(|_| {
                    s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                    ((f64::from(s) / 4294967296.0 * 2.0 - 1.0) * amp) as f32
                })
                .collect()
        }
        "clicks" => {
            let (sr, dur, bpm, first, amp) = (f("sr"), f("dur"), f("bpm"), f("first"), f("amp"));
            let length = (dur * sr + 0.5).floor() as usize;
            let mut out = vec![0.0f32; length];
            let period = 60.0 / bpm;
            let mut beat = first;
            while beat < dur {
                let start = (beat * sr + 0.5).floor() as usize;
                let end = length.min(start + (0.05 * sr + 0.5).floor() as usize);
                for i in start..end {
                    out[i] = (amp * if (((i - start) as f64 * 2.0 * 1000.0) / sr).floor() % 2.0 == 0.0 { 1.0 } else { -1.0 }) as f32;
                }
                beat += period;
            }
            out
        }
        "concat" => r["parts"].as_array().unwrap().iter().flat_map(build).collect(),
        other => panic!("unknown recipe {other}"),
    }
}

fn close(a: f64, b: f64, what: &str, case: &str) {
    assert!((a - b).abs() <= b.abs() * 1e-9 + 1e-12, "{case}: {what}: Rust {a}, TS {b}");
}

#[test]
fn analysis_matches_the_typescript_on_every_signal() {
    let golden: Value = serde_json::from_str(include_str!("data/analysis_golden.json")).unwrap();
    let cases = golden.as_array().unwrap();
    assert!(cases.len() > 40);
    let mut confident = 0;

    for c in cases {
        let name = c["name"].as_str().unwrap();
        let sample_rate = c["sr"].as_u64().unwrap() as u32;
        let audio = DecodedAudio { sample_rate, channel_data: c["channels"].as_array().unwrap().iter().map(build).collect() };

        let mono = mix_to_mono(&audio);
        assert_eq!(mono.len(), c["monoLen"].as_u64().unwrap() as usize, "{name}: mono length");
        close(mono.iter().map(|v| f64::from(*v)).sum(), c["monoSum"].as_f64().unwrap(), "mono sum", name);
        if !mono.is_empty() {
            assert_eq!(f64::from(mono[mono.len() >> 1]), c["monoMid"].as_f64().unwrap(), "{name}: mono middle sample");
        }

        let bpm = estimate_bpm(&mono, f64::from(sample_rate));
        let want = &c["bpm"];
        close(bpm.bpm, want["bpm"].as_f64().unwrap(), "bpm", name);
        close(bpm.confidence, want["confidence"].as_f64().unwrap(), "confidence", name);
        close(bpm.first_beat_offset_seconds, want["firstBeatOffsetSeconds"].as_f64().unwrap(), "first beat offset", name);
        if bpm.confidence > 0.3 {
            confident += 1;
        }

        let loudness = measure_loudness_db(&mono);
        close(loudness, c["loudnessDb"].as_f64().unwrap(), "loudness", name);
        close(compute_normalization_gain(loudness), c["normalizationGainFromLoudness"].as_f64().unwrap(), "gain from loudness", name);

        for (key, given) in [("bounds", None), ("boundsGiven", Some(mono.as_slice()))] {
            let b = find_content_bounds(&audio, given);
            let want = &c[key];
            assert_eq!(b.start_sample as u64, want["startSample"].as_u64().unwrap(), "{name}: {key} start");
            assert_eq!(b.end_sample as u64, want["endSample"].as_u64().unwrap(), "{name}: {key} end");
        }

        close(analyze_track(&audio).normalization_gain, c["analysisGain"].as_f64().unwrap(), "analyze_track gain", name);
    }
    assert!(confident > 15, "only {confident} confident estimates - the click-train cases are not exercising the estimator");
}
