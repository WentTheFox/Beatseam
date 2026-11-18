//! Decoding a whole file to planar f32 PCM with symphonia.

use std::io::Cursor;

use beatseam_core::audio::DecodedAudio;
use symphonia::core::audio::sample::Sample;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error;
use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::TrackType;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// Decodes an entire audio file held in memory. `extension` (e.g. `"mp3"`) is a hint that makes probing cheaper and more reliable.
/// Mono stays mono and everything else keeps its channels, at the file's own sample rate (the mixer resamples on the way out).
/// A damaged packet is skipped rather than failing the track: a few lost milliseconds beat a track that won't play.
pub fn decode_audio(bytes: Vec<u8>, extension: Option<&str>) -> Result<DecodedAudio, String> {
    let stream = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = extension {
        hint.with_extension(extension);
    }
    let mut format = symphonia::default::get_probe().probe(&hint, stream, FormatOptions::default(), MetadataOptions::default()).map_err(|e| format!("Couldn't read this audio file: {e}"))?;
    let track = format.default_track(TrackType::Audio).ok_or("The file has no audio track")?;
    let track_id = track.id;
    let params = track.codec_params.as_ref().and_then(|p| p.audio()).ok_or("The file's first track is not audio")?;
    let mut decoder = symphonia::default::get_codecs().make_audio_decoder(params, &AudioDecoderOptions::default()).map_err(|e| format!("No decoder for this audio format: {e}"))?;

    let mut planar: Vec<Vec<f32>> = Vec::new();
    let mut sample_rate = 0u32;
    let mut scratch: Vec<f32> = Vec::new();
    while let Some(packet) = format.next_packet().map_err(|e| format!("Couldn't read the audio: {e}"))? {
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buffer) => {
                let spec = buffer.spec();
                sample_rate = spec.rate();
                let channels = spec.channels().count().max(1);
                if planar.is_empty() {
                    planar = vec![Vec::new(); channels];
                }
                scratch.resize(buffer.samples_interleaved(), f32::MID);
                buffer.copy_to_slice_interleaved(&mut scratch);
                for frame in scratch.chunks(channels) {
                    for (channel, sample) in planar.iter_mut().zip(frame) {
                        channel.push(*sample);
                    }
                }
            }
            Err(Error::DecodeError(_)) => continue,
            Err(e) => return Err(format!("Couldn't decode the audio: {e}")),
        }
    }
    if sample_rate == 0 || planar.first().is_none_or(Vec::is_empty) {
        return Err("The file contains no audio".into());
    }
    Ok(DecodedAudio { sample_rate, channel_data: planar })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(channels: u16, rate: u32, frames: usize, sample: impl Fn(usize, u16) -> i16) -> Vec<u8> {
        let mut bytes = Vec::new();
        let spec = hound::WavSpec { channels, sample_rate: rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        {
            let mut writer = hound::WavWriter::new(Cursor::new(&mut bytes), spec).unwrap();
            for i in 0..frames {
                for c in 0..channels {
                    writer.write_sample(sample(i, c)).unwrap();
                }
            }
            writer.finalize().unwrap();
        }
        bytes
    }

    #[test]
    fn a_stereo_wav_decodes_to_planar_channels_at_its_own_rate() {
        let audio = decode_audio(wav(2, 22_050, 1000, |i, c| if c == 0 { i as i16 } else { -(i as i16) }), Some("wav")).unwrap();
        assert_eq!((audio.sample_rate, audio.number_of_channels(), audio.channel_data[0].len()), (22_050, 2, 1000));
        assert!((audio.channel_data[0][500] - 500.0 / 32768.0).abs() < 1e-6);
        assert!((audio.channel_data[1][500] + 500.0 / 32768.0).abs() < 1e-6);
        assert!((audio.duration_seconds() - 1000.0 / 22_050.0).abs() < 1e-9);
    }

    #[test]
    fn mono_stays_mono_and_the_extension_hint_is_optional() {
        let audio = decode_audio(wav(1, 8000, 100, |i, _| (i * 100) as i16), None).unwrap();
        assert_eq!((audio.number_of_channels(), audio.sample_rate), (1, 8000));
    }

    #[test]
    fn things_that_are_not_audio_fail_with_a_message() {
        for bytes in [b"just some text, not audio at all".to_vec(), Vec::new(), vec![0u8; 64]] {
            let error = decode_audio(bytes, Some("mp3")).unwrap_err();
            assert!(!error.is_empty(), "{error}");
        }
    }

    #[test]
    fn a_wav_with_no_samples_is_reported_as_empty() {
        assert!(decode_audio(wav(2, 44_100, 0, |_, _| 0), Some("wav")).is_err());
    }
}
