//! symphonia decode to stereo f32 at the output sample rate.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use symphonia::core::audio::sample::Sample;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error;
use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::TrackType;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use crate::mixer::Frames;

pub struct Decoded {
    pub frames: Frames,
    pub source_rate: u32,
    pub source_channels: usize,
    pub decode_ms: u128,
}

/// Decode up to `max_secs` of `path`, mix to stereo and resample (linear, spike quality) to `out_rate`.
pub fn decode(path: &Path, out_rate: u32, max_secs: f32) -> Result<Decoded, String> {
    let started = std::time::Instant::now();
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| format!("probe: {e}"))?;
    let track = format.default_track(TrackType::Audio).ok_or("no audio track")?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or("not an audio track")?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .map_err(|e| format!("decoder: {e}"))?;

    let mut interleaved: Vec<f32> = Vec::new();
    let mut scratch: Vec<f32> = Vec::new();
    let mut source_rate = 0u32;
    let mut channels = 0usize;
    let mut stereo: Vec<[f32; 2]> = Vec::new();

    while let Some(packet) = format.next_packet().map_err(|e| format!("packet: {e}"))? {
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                let spec = buf.spec();
                source_rate = spec.rate();
                channels = spec.channels().count();
                scratch.resize(buf.samples_interleaved(), f32::MID);
                buf.copy_to_slice_interleaved(&mut scratch);
                interleaved.clear();
                interleaved.extend_from_slice(&scratch);
                for f in interleaved.chunks(channels) {
                    stereo.push(match channels {
                        1 => [f[0], f[0]],
                        _ => [f[0], f[1]],
                    });
                }
                if source_rate > 0 && stereo.len() as f32 / source_rate as f32 >= max_secs {
                    break;
                }
            }
            Err(Error::DecodeError(_)) => continue,
            Err(e) => return Err(format!("decode: {e}")),
        }
    }
    if source_rate == 0 || stereo.is_empty() {
        return Err("no audio decoded".into());
    }

    let frames = if source_rate == out_rate { stereo } else { resample_linear(&stereo, source_rate, out_rate) };
    Ok(Decoded {
        frames: Arc::new(frames),
        source_rate,
        source_channels: channels,
        decode_ms: started.elapsed().as_millis(),
    })
}

fn resample_linear(input: &[[f32; 2]], from: u32, to: u32) -> Vec<[f32; 2]> {
    let ratio = from as f64 / to as f64;
    let out_len = (input.len() as f64 / ratio) as usize;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * ratio;
            let i0 = pos as usize;
            let frac = (pos - i0 as f64) as f32;
            let a = input[i0.min(input.len() - 1)];
            let b = input[(i0 + 1).min(input.len() - 1)];
            [a[0] + (b[0] - a[0]) * frac, a[1] + (b[1] - a[1]) * frac]
        })
        .collect()
}
