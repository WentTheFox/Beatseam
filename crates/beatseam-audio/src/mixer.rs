//! The mixer: voices (one per playing source) rendered sample-accurately with Web Audio-style gain automation. It is what the
//! engine's output callback pulls frames from, and it is plain arithmetic - no device, no threads of its own - so it renders offline
//! just as well (tests, the CLI's `--render`).

use std::sync::Arc;

use beatseam_core::audio::DecodedAudio;

/// A scheduled change to a parameter, in engine-clock seconds.
#[derive(Clone, Debug, PartialEq)]
enum Event {
    /// Linear from `from` to `to` over `[at, at + dur)`.
    Ramp { at: f64, dur: f64, from: f64, to: f64 },
    /// The sampled `values`, evenly spaced over `[at, at + dur)`, interpolated linearly between them.
    Curve { at: f64, dur: f64, values: Vec<f64> },
}

impl Event {
    fn at(&self) -> f64 {
        match self {
            Self::Ramp { at, .. } | Self::Curve { at, .. } => *at,
        }
    }
}

/// An automatable value: a base that [`set`](Self::set) changes at once, plus scheduled events.
#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    base: f64,
    events: Vec<Event>,
}

impl Param {
    pub fn new(base: f64) -> Self {
        Self { base, events: Vec::new() }
    }

    /// An immediate change; anything scheduled is dropped.
    pub fn set(&mut self, value: f64) {
        self.base = value;
        self.events.clear();
    }

    /// The value at engine time `t`.
    pub fn value_at(&self, t: f64) -> f64 {
        let mut value = self.base;
        for event in &self.events {
            match event {
                Event::Ramp { at, dur, from, to } => {
                    if t < *at {
                        break;
                    }
                    if t < at + dur && *dur > 0.0 {
                        return from + (to - from) * (t - at) / dur;
                    }
                    value = *to;
                }
                Event::Curve { at, dur, values } => {
                    if t < *at {
                        break;
                    }
                    if t < at + dur && *dur > 0.0 && values.len() > 1 {
                        let x = (t - at) / dur * (values.len() - 1) as f64;
                        let i = (x.floor() as usize).min(values.len() - 2);
                        return values[i] + (values[i + 1] - values[i]) * (x - i as f64);
                    }
                    value = values.last().copied().unwrap_or(value);
                }
            }
        }
        value
    }

    /// A linear ramp starting at `at` from whatever the value is then. (Web Audio anchors a ramp to the previous event the same way;
    /// callers rely on a ramp scheduled "now" starting from the live value.)
    pub fn ramp(&mut self, to: f64, at: f64, dur: f64) {
        let from = self.value_at(at);
        self.push(Event::Ramp { at, dur, from, to });
    }

    pub fn curve(&mut self, values: &[f64], at: f64, dur: f64) {
        self.push(Event::Curve { at, dur, values: values.to_vec() });
    }

    fn push(&mut self, event: Event) {
        // Kept ordered by start time; an event that starts at the same time as an earlier one replaces what it overlaps.
        self.events.retain(|e| e.at() < event.at());
        self.events.push(event);
    }
}

/// One source being played.
struct Voice {
    id: u64,
    audio: Arc<DecodedAudio>,
    /// Engine clock frame the voice starts at; `None` until scheduled.
    start_frame: Option<u64>,
    /// Position in the source's own frames (fractional).
    position: f64,
    gain: Param,
    rate: f64,
    /// Engine clock frame the voice is cut off at, if a stop was scheduled.
    stop_frame: Option<u64>,
}

/// The mixer. Not thread-safe by itself: wrap it (the engine shares one behind a mutex between the UI side and the output callback).
pub struct MixerCore {
    sample_rate: u32,
    /// Frames rendered so far: the engine clock.
    frames_rendered: u64,
    voices: Vec<Voice>,
    next_id: u64,
    /// Voices that finished (played to the end, reached a scheduled stop, or were stopped), oldest first, until taken.
    ended: Vec<u64>,
}

impl MixerCore {
    pub fn new(sample_rate: u32) -> Self {
        Self { sample_rate, frames_rendered: 0, voices: Vec::new(), next_id: 0, ended: Vec::new() }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The engine clock, in seconds: the audio rendered so far.
    pub fn now(&self) -> f64 {
        self.frames_rendered as f64 / f64::from(self.sample_rate)
    }

    pub fn add_voice(&mut self, audio: Arc<DecodedAudio>) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.voices.push(Voice { id, audio, start_frame: None, position: 0.0, gain: Param::new(1.0), rate: 1.0, stop_frame: None });
        id
    }

    fn voice(&mut self, id: u64) -> Option<&mut Voice> {
        self.voices.iter_mut().find(|v| v.id == id)
    }

    fn frame_for(&self, seconds: f64) -> u64 {
        (seconds * f64::from(self.sample_rate)).round().max(0.0) as u64
    }

    /// Starts the voice at `when` seconds (never earlier than the next frame to render), `offset` seconds into the audio.
    pub fn schedule_start(&mut self, id: u64, when: f64, offset: f64) -> Result<(), String> {
        if !when.is_finite() || !offset.is_finite() || offset < 0.0 {
            return Err("The provided double value is non-finite.".into());
        }
        let start = self.frame_for(when).max(self.frames_rendered);
        let Some(voice) = self.voice(id) else { return Ok(()) };
        voice.start_frame = Some(start);
        voice.position = offset * f64::from(voice.audio.sample_rate);
        Ok(())
    }

    pub fn set_gain(&mut self, id: u64, value: f64) {
        if let Some(voice) = self.voice(id) {
            voice.gain.set(value);
        }
    }

    pub fn ramp_gain(&mut self, id: u64, to: f64, at: f64, dur: f64) {
        if let Some(voice) = self.voice(id) {
            voice.gain.ramp(to, at, dur);
        }
    }

    pub fn ramp_gain_curve(&mut self, id: u64, values: &[f64], at: f64, dur: f64) {
        if let Some(voice) = self.voice(id) {
            voice.gain.curve(values, at, dur);
        }
    }

    pub fn set_rate(&mut self, id: u64, rate: f64) {
        if let Some(voice) = self.voice(id) {
            voice.rate = rate;
        }
    }

    /// Cuts the voice off at `when` seconds, or now.
    pub fn stop(&mut self, id: u64, when: Option<f64>) {
        match when {
            Some(when) => {
                let frame = self.frame_for(when);
                if let Some(voice) = self.voice(id) {
                    voice.stop_frame = Some(frame);
                }
            }
            None => {
                if let Some(index) = self.voices.iter().position(|v| v.id == id) {
                    self.voices.remove(index);
                    self.ended.push(id);
                }
            }
        }
    }

    /// Voices that have finished since the last call.
    pub fn take_ended(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.ended)
    }

    pub fn active_voices(&self) -> usize {
        self.voices.len()
    }

    /// Fills `out` (interleaved, `channels` per frame) with the next frames and advances the clock. Silence where nothing plays.
    pub fn render(&mut self, out: &mut [f32], channels: usize) {
        let channels = channels.max(1);
        let rate = f64::from(self.sample_rate);
        for (n, frame) in out.chunks_mut(channels).enumerate() {
            let clock = self.frames_rendered + n as u64;
            let seconds = clock as f64 / rate;
            let (mut left, mut right) = (0.0f32, 0.0f32);
            for voice in &mut self.voices {
                let Some(start) = voice.start_frame else { continue };
                if clock < start || voice.stop_frame.is_some_and(|s| clock >= s) {
                    continue;
                }
                let (l, r) = voice.sample();
                let gain = voice.gain.value_at(seconds) as f32;
                left += l * gain;
                right += r * gain;
                voice.position += f64::from(voice.audio.sample_rate) / rate * voice.rate;
            }
            match channels {
                1 => frame[0] = (left + right) * 0.5,
                _ => {
                    frame[0] = left;
                    frame[1] = right;
                    frame[2..].fill(0.0);
                }
            }
        }
        self.frames_rendered += (out.len() / channels) as u64;
        let now = self.frames_rendered;
        let ended = &mut self.ended;
        self.voices.retain(|voice| {
            let finished = voice.start_frame.is_some() && (voice.position >= voice.length() || voice.stop_frame.is_some_and(|s| now >= s));
            if finished {
                ended.push(voice.id);
            }
            !finished
        });
    }
}

impl Voice {
    fn length(&self) -> f64 {
        self.audio.channel_data.first().map_or(0.0, |c| c.len() as f64)
    }

    /// The (left, right) sample at the current position, linearly interpolated; mono plays on both sides.
    fn sample(&self) -> (f32, f32) {
        let channels = &self.audio.channel_data;
        if channels.is_empty() || self.position < 0.0 {
            return (0.0, 0.0);
        }
        let at = |channel: &Vec<f32>| {
            let i = self.position as usize;
            if i >= channel.len() {
                return 0.0;
            }
            let next = channel.get(i + 1).copied().unwrap_or(channel[i]);
            let frac = (self.position - i as f64) as f32;
            channel[i] + (next - channel[i]) * frac
        };
        let left = at(&channels[0]);
        (left, if channels.len() > 1 { at(&channels[1]) } else { left })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(rate: u32, left: f32, right: f32, frames: usize) -> Arc<DecodedAudio> {
        Arc::new(DecodedAudio { sample_rate: rate, channel_data: vec![vec![left; frames], vec![right; frames]] })
    }

    fn render(core: &mut MixerCore, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * 2];
        core.render(&mut out, 2);
        out
    }

    #[test]
    fn a_param_holds_its_base_ramps_linearly_and_stays_at_the_end_value() {
        let mut p = Param::new(1.0);
        p.ramp(0.0, 2.0, 4.0);
        assert_eq!((p.value_at(0.0), p.value_at(2.0), p.value_at(4.0), p.value_at(6.0), p.value_at(100.0)), (1.0, 1.0, 0.5, 0.0, 0.0));
        p.set(0.7);
        assert_eq!(p.value_at(3.0), 0.7, "an immediate set drops what was scheduled");
    }

    #[test]
    fn a_ramp_starts_from_the_live_value_at_its_start_not_from_the_base() {
        let mut p = Param::new(1.0);
        p.ramp(0.5, 0.0, 1.0); // 1.0 -> 0.5 over the first second
        p.ramp(0.0, 1.0, 1.0); // then from where that ended
        assert_eq!((p.value_at(1.0), p.value_at(1.5), p.value_at(2.0)), (0.5, 0.25, 0.0));
    }

    #[test]
    fn a_curve_interpolates_between_its_samples_and_holds_its_last_value() {
        let mut p = Param::new(1.0);
        p.curve(&[0.0, 1.0, 0.0], 10.0, 2.0);
        assert_eq!((p.value_at(9.0), p.value_at(10.0), p.value_at(10.5), p.value_at(11.0), p.value_at(11.5), p.value_at(12.0), p.value_at(50.0)), (1.0, 0.0, 0.5, 1.0, 0.5, 0.0, 0.0));
    }

    #[test]
    fn a_voice_plays_from_its_scheduled_frame_at_the_requested_offset() {
        let mut core = MixerCore::new(100);
        let id = core.add_voice(audio(100, 0.5, -0.5, 1000));
        core.schedule_start(id, 0.10, 0.0).unwrap();
        let out = render(&mut core, 20);
        assert_eq!(&out[..2], [0.0, 0.0], "silent before its start");
        assert_eq!(&out[2 * 9..2 * 9 + 2], [0.0, 0.0]);
        assert_eq!(&out[2 * 10..2 * 10 + 2], [0.5, -0.5], "sample-accurate start at frame 10");
        assert_eq!(core.now(), 0.2);
    }

    #[test]
    fn an_offset_skips_into_the_audio_and_mono_plays_on_both_sides() {
        let mut core = MixerCore::new(100);
        let mono = Arc::new(DecodedAudio { sample_rate: 100, channel_data: vec![(0..100).map(|i| i as f32).collect()] });
        let id = core.add_voice(mono);
        core.schedule_start(id, 0.0, 0.25).unwrap();
        let out = render(&mut core, 3);
        assert_eq!(out, [25.0, 25.0, 26.0, 26.0, 27.0, 27.0]);
    }

    #[test]
    fn a_start_in_the_past_begins_at_the_next_frame_and_a_bad_offset_is_rejected() {
        let mut core = MixerCore::new(100);
        render(&mut core, 50);
        let id = core.add_voice(audio(100, 1.0, 1.0, 10));
        core.schedule_start(id, 0.0, 0.0).unwrap();
        assert_eq!(render(&mut core, 1)[0], 1.0);
        assert!(core.schedule_start(id, f64::NAN, 0.0).is_err() && core.schedule_start(id, 0.0, f64::INFINITY).is_err() && core.schedule_start(id, 0.0, -1.0).is_err());
    }

    #[test]
    fn gain_is_applied_per_frame_including_an_equal_power_style_curve() {
        let mut core = MixerCore::new(100);
        let id = core.add_voice(audio(100, 1.0, 1.0, 1000));
        core.schedule_start(id, 0.0, 0.0).unwrap();
        core.set_gain(id, 0.5);
        core.ramp_gain(id, 0.0, 0.10, 0.10); // fade to silence over frames 10..20
        let out = render(&mut core, 30);
        assert_eq!(out[0], 0.5);
        assert!((out[2 * 15] - 0.25).abs() < 1e-6, "halfway down the ramp");
        assert_eq!(out[2 * 25], 0.0);
    }

    #[test]
    fn a_source_that_plays_to_its_end_reports_it_once_and_is_dropped() {
        let mut core = MixerCore::new(100);
        let id = core.add_voice(audio(100, 1.0, 1.0, 10));
        core.schedule_start(id, 0.0, 0.0).unwrap();
        let out = render(&mut core, 20);
        assert_eq!((out[2 * 9], out[2 * 10]), (1.0, 0.0), "the audio ends after 10 frames");
        assert_eq!(core.take_ended(), [id]);
        assert!(core.take_ended().is_empty());
        assert_eq!(core.active_voices(), 0);
    }

    #[test]
    fn a_scheduled_stop_cuts_the_voice_off_and_reports_it() {
        let mut core = MixerCore::new(100);
        let id = core.add_voice(audio(100, 1.0, 1.0, 1000));
        core.schedule_start(id, 0.0, 0.0).unwrap();
        core.stop(id, Some(0.05));
        let out = render(&mut core, 10);
        assert_eq!((out[2 * 4], out[2 * 5]), (1.0, 0.0));
        assert_eq!(core.take_ended(), [id]);
    }

    #[test]
    fn an_immediate_stop_removes_the_voice_and_still_reports_it_so_the_player_can_ignore_it() {
        let mut core = MixerCore::new(100);
        let id = core.add_voice(audio(100, 1.0, 1.0, 1000));
        core.schedule_start(id, 0.0, 0.0).unwrap();
        core.stop(id, None);
        assert_eq!(render(&mut core, 5)[0], 0.0);
        assert_eq!(core.take_ended(), [id]);
        core.stop(id, None); // stopping twice is harmless
        core.set_gain(id, 0.1);
        assert!(core.take_ended().is_empty());
    }

    #[test]
    fn a_source_at_another_sample_rate_is_resampled_to_the_output_rate() {
        let mut core = MixerCore::new(100);
        let slow = Arc::new(DecodedAudio { sample_rate: 50, channel_data: vec![vec![0.0, 1.0, 2.0, 3.0]] });
        let id = core.add_voice(slow);
        core.schedule_start(id, 0.0, 0.0).unwrap();
        let out = render(&mut core, 6);
        assert_eq!((out[0], out[2], out[4], out[6]), (0.0, 0.5, 1.0, 1.5), "linear interpolation: two output frames per source frame");
    }

    #[test]
    fn rate_changes_how_fast_the_audio_is_consumed() {
        let mut core = MixerCore::new(100);
        let id = core.add_voice(Arc::new(DecodedAudio { sample_rate: 100, channel_data: vec![(0..100).map(|i| i as f32).collect()] }));
        core.set_rate(id, 2.0);
        core.schedule_start(id, 0.0, 0.0).unwrap();
        let out = render(&mut core, 3);
        assert_eq!((out[0], out[2], out[4]), (0.0, 2.0, 4.0));
    }

    #[test]
    fn rendering_is_independent_of_the_buffer_size() {
        let make = || {
            let mut core = MixerCore::new(100);
            let a = core.add_voice(Arc::new(DecodedAudio { sample_rate: 100, channel_data: vec![(0..300).map(|i| (i as f32 * 0.01).sin()).collect(), vec![0.25; 300]] }));
            let b = core.add_voice(audio(100, 0.1, 0.2, 200));
            core.schedule_start(a, 0.0, 0.0).unwrap();
            core.schedule_start(b, 1.0, 0.0).unwrap();
            core.ramp_gain_curve(a, &[1.0, 0.0], 1.0, 1.0);
            core
        };
        let all = |chunk: usize| {
            let mut core = make();
            let mut v = Vec::new();
            for _ in 0..(400 / chunk) {
                v.extend(render(&mut core, chunk));
            }
            v
        };
        assert_eq!(all(1), all(40));
        assert_eq!(all(40), all(100));
    }

    #[test]
    fn mono_and_surround_outputs_are_handled() {
        let mut core = MixerCore::new(100);
        let id = core.add_voice(audio(100, 1.0, 0.0, 100));
        core.schedule_start(id, 0.0, 0.0).unwrap();
        let mut mono = vec![0.0; 2];
        core.render(&mut mono, 1);
        assert_eq!(mono, [0.5, 0.5]);
        let mut surround = vec![9.0; 6];
        core.render(&mut surround, 6);
        assert_eq!(surround, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    }
}
