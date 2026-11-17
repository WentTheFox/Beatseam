//! Platform-independent audio types shared by the analysis code and the engine traits (more arrive with `beatseam-audio`).

/// Decoded PCM audio, platform-independent: one `Vec<f32>` per channel, all the same length.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DecodedAudio {
    pub sample_rate: u32,
    pub channel_data: Vec<Vec<f32>>,
}

impl DecodedAudio {
    pub fn number_of_channels(&self) -> usize {
        self.channel_data.len()
    }

    pub fn duration_seconds(&self) -> f64 {
        self.channel_data.first().map_or(0.0, |c| c.len() as f64) / f64::from(self.sample_rate)
    }
}
