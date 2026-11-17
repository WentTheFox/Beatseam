use crate::audio::DecodedAudio;

/// Averages all channels down to one - analysis only ever needs a single representative signal, playback stays
/// full-channel.
///
/// Accumulates into an `f32` buffer one channel at a time, rounding to `f32` after every addition, exactly like the
/// TypeScript's `Float32Array`, so the result is bit-identical to what the TS stored.
pub fn mix_to_mono(audio: &DecodedAudio) -> Vec<f32> {
    if audio.channel_data.len() == 1 {
        return audio.channel_data[0].clone();
    }
    let length = audio.channel_data.first().map_or(0, Vec::len);
    let channels = audio.channel_data.len() as f64;
    let mut mono = vec![0.0f32; length];
    for channel in &audio.channel_data {
        for (i, out) in mono.iter_mut().enumerate() {
            let sample = f64::from(channel.get(i).copied().unwrap_or(0.0));
            *out = (f64::from(*out) + sample / channels) as f32;
        }
    }
    mono
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(channels: Vec<Vec<f32>>) -> DecodedAudio {
        DecodedAudio { sample_rate: 44100, channel_data: channels }
    }

    #[test]
    fn a_mono_track_is_returned_as_is() {
        assert_eq!(mix_to_mono(&audio(vec![vec![0.1, 0.2, 0.3]])), vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn averages_channels() {
        assert_eq!(mix_to_mono(&audio(vec![vec![1.0, 0.0, -1.0], vec![0.0, 1.0, -1.0]])), vec![0.5, 0.5, -1.0]);
    }

    #[test]
    fn a_shorter_channel_counts_as_silence_past_its_end() {
        assert_eq!(mix_to_mono(&audio(vec![vec![1.0, 1.0], vec![1.0]])), vec![1.0, 0.5]);
    }

    #[test]
    fn no_channels_gives_nothing() {
        assert!(mix_to_mono(&audio(vec![])).is_empty());
    }
}
