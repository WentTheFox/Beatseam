use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use beatseam_core::audio::DecodedAudio;
use beatseam_core::audio_engine::{AudioEngine, DecodeError, EngineError, OnEnded, RampSpec, SourceNode};
use beatseam_core::file_access::{CallOptions, FileAccess, FileRef};

use crate::decode::decode_audio;
use crate::mixer::MixerCore;

fn locked(core: &Mutex<MixerCore>) -> std::sync::MutexGuard<'_, MixerCore> {
    // A poisoned lock means a panic elsewhere while mixing; the mixer's state is still plain data, so keep going.
    core.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The [`AudioEngine`] on top of [`MixerCore`]: files are read through the app's file access and decoded with symphonia, and playback
/// is whatever the host pulls out of [`render`](Self::render) - an output device's callback, or an offline loop.
///
/// Threading: the mixer is shared behind a mutex so an output callback on another thread can render while the app thread schedules
/// and automates. Ended callbacks are not `Send`, so the engine keeps them on the app thread: the render side only records which
/// sources finished, and the app thread runs their callbacks when it calls [`poll_events`](Self::poll_events) (the playback ticker's
/// tick is a natural place).
pub struct MixerEngine<F> {
    core: Arc<Mutex<MixerCore>>,
    files: Arc<F>,
    callbacks: RefCell<HashMap<u64, OnEnded>>,
}

impl<F: FileAccess> MixerEngine<F> {
    /// `output_rate` is the sample rate the output device runs at.
    pub fn new(files: Arc<F>, output_rate: u32) -> Self {
        Self { core: Arc::new(Mutex::new(MixerCore::new(output_rate))), files, callbacks: RefCell::new(HashMap::new()) }
    }

    /// The shared mixer, for a host that renders from its own thread.
    pub fn core(&self) -> Arc<Mutex<MixerCore>> {
        self.core.clone()
    }

    /// Renders the next frames (interleaved, `channels` per frame) and advances the engine clock. Safe to call from an output
    /// callback: it never touches anything that is not `Send`.
    pub fn render(&self, out: &mut [f32], channels: usize) {
        locked(&self.core).render(out, channels);
    }

    /// Runs the ended callbacks of sources that finished (played to the end, reached a scheduled stop, or were stopped) since the last
    /// call. Returns how many ran.
    pub fn poll_events(&self) -> usize {
        let ended = locked(&self.core).take_ended();
        let mut ran = 0;
        for id in ended {
            // Taken out first: a callback may call back into the engine.
            let callback = self.callbacks.borrow_mut().remove(&id);
            if let Some(callback) = callback {
                callback();
                ran += 1;
            }
        }
        ran
    }
}

/// One playing source: a handle onto a voice in the shared mixer.
pub struct MixerSource {
    id: String,
    voice: u64,
    core: Arc<Mutex<MixerCore>>,
}

impl SourceNode for MixerSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn set_gain(&self, value: f64) {
        locked(&self.core).set_gain(self.voice, value);
    }

    fn ramp_gain(&self, ramp: RampSpec) -> Result<(), EngineError> {
        locked(&self.core).ramp_gain(self.voice, ramp.to_value, ramp.at_time_seconds, ramp.duration_seconds);
        Ok(())
    }

    fn ramp_gain_curve(&self, values: &[f64], at_time_seconds: f64, duration_seconds: f64) -> Result<(), EngineError> {
        locked(&self.core).ramp_gain_curve(self.voice, values, at_time_seconds, duration_seconds);
        Ok(())
    }

    fn set_rate(&self, value: f64) {
        locked(&self.core).set_rate(self.voice, value);
    }

    fn ramp_rate(&self, _ramp: RampSpec) -> Result<(), EngineError> {
        Err(EngineError("Rate automation isn't supported yet".into()))
    }

    fn stop(&self, when_seconds: Option<f64>) {
        locked(&self.core).stop(self.voice, when_seconds);
    }
}

impl<F: FileAccess> AudioEngine for MixerEngine<F> {
    type Source = MixerSource;

    async fn decode_file(&self, file: &FileRef) -> Result<Arc<DecodedAudio>, DecodeError> {
        let bytes = self.files.read_file_bytes(file, CallOptions::default()).await.map_err(|e| DecodeError(e.to_string()))?;
        let extension = file.name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
        decode_audio(bytes, extension.as_deref()).map(Arc::new).map_err(DecodeError)
    }

    fn create_source(&self, audio: &Arc<DecodedAudio>, on_ended: Option<OnEnded>) -> MixerSource {
        let voice = locked(&self.core).add_voice(audio.clone());
        if let Some(callback) = on_ended {
            self.callbacks.borrow_mut().insert(voice, callback);
        }
        MixerSource { id: format!("source-{voice}"), voice, core: self.core.clone() }
    }

    fn schedule_start(&self, source: &MixerSource, when_seconds: f64, offset_seconds: f64) -> Result<(), EngineError> {
        locked(&self.core).schedule_start(source.voice, when_seconds, offset_seconds).map_err(EngineError)
    }

    fn now(&self) -> f64 {
        locked(&self.core).now()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::future::Future;
    use std::io::Cursor;
    use std::pin::Pin;
    use std::rc::Rc;

    use beatseam_core::file_access::{DirectoryEntry, FileAccessError, GrantedRoot, RootKind};
    use beatseam_core::playback::{PlaylistOptions, PlaylistPlayer, PlaylistPlayerOptions, ResolveTrack, TrackPlayer, TrackPlayerCallbacks, TrackPlayerStatus};
    use beatseam_core::tasks::Runtime;
    use beatseam_core::tasks::test_executor::Pool;

    use super::*;

    /// Files that are generated WAVs: `name` -> (seconds, amplitude), at 8 kHz stereo.
    struct WavFiles(HashMap<String, (f32, f32)>);

    const RATE: u32 = 8000;

    fn wav(seconds: f32, amplitude: f32) -> Vec<u8> {
        let mut bytes = Vec::new();
        let spec = hound::WavSpec { channels: 2, sample_rate: RATE, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        {
            let mut writer = hound::WavWriter::new(Cursor::new(&mut bytes), spec).unwrap();
            for i in 0..(seconds * RATE as f32) as usize {
                let sample = ((i as f32 * 0.3).sin() * amplitude * i16::MAX as f32) as i16;
                writer.write_sample(sample).unwrap();
                writer.write_sample(sample).unwrap();
            }
            writer.finalize().unwrap();
        }
        bytes
    }

    impl FileAccess for WavFiles {
        async fn request_root(&self, _k: RootKind) -> Result<Option<GrantedRoot>, FileAccessError> {
            Ok(None)
        }
        async fn list_granted_roots(&self) -> Result<Vec<GrantedRoot>, FileAccessError> {
            Ok(vec![])
        }
        async fn revoke_root(&self, _r: &str) -> Result<(), FileAccessError> {
            Ok(())
        }
        async fn list_directory(&self, _r: &str, _p: Option<&str>, _o: CallOptions) -> Result<Vec<DirectoryEntry>, FileAccessError> {
            Ok(vec![])
        }
        async fn read_file_bytes(&self, file: &FileRef, _o: CallOptions) -> Result<Vec<u8>, FileAccessError> {
            let (seconds, amplitude) = self.0.get(&file.id).copied().ok_or_else(|| FileAccessError::NotFound(file.id.clone()))?;
            Ok(wav(seconds, amplitude))
        }
        async fn read_file_text(&self, _f: &FileRef, _o: CallOptions) -> Result<String, FileAccessError> {
            Ok(String::new())
        }
        async fn write_file_text(&self, _r: &str, _p: &str, _c: &str) -> Result<(), FileAccessError> {
            Ok(())
        }
    }

    fn engine(files: &[(&str, f32, f32)]) -> Rc<MixerEngine<WavFiles>> {
        Rc::new(MixerEngine::new(Arc::new(WavFiles(files.iter().map(|(n, s, a)| (n.to_string(), (*s, *a))).collect())), RATE))
    }

    fn file(id: &str) -> FileRef {
        FileRef { id: id.into(), name: format!("{id}.wav"), relative_path: format!("{id}.wav"), size_bytes: 0, last_modified_ms: 0.0 }
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
    }

    #[test]
    fn a_decoded_file_plays_through_the_track_player_and_the_end_is_reported_on_the_app_thread() {
        let engine = engine(&[("a", 2.0, 0.5)]);
        let ended = Rc::new(Cell::new(0));
        let e = ended.clone();
        let player = TrackPlayer::new(engine.clone(), TrackPlayerCallbacks { on_ended: Some(Rc::new(move || e.set(e.get() + 1))), ..Default::default() });
        beatseam_core::http::block_on(player.load(&file("a"))).unwrap();
        assert_eq!(player.get_state().duration_seconds, 2.0);
        player.set_volume(1.0);
        player.play();

        let mut out = vec![0.0; 2 * RATE as usize]; // one second
        engine.render(&mut out, 2);
        assert!(rms(&out) > 0.2, "the file is audible");
        assert!((player.get_state().position_seconds - 1.0).abs() < 1e-9, "the player's position follows the rendered audio");
        assert_eq!(engine.poll_events(), 0);
        assert_eq!(ended.get(), 0);

        engine.render(&mut out, 2);
        assert_eq!(ended.get(), 0, "the end is only reported when the app thread polls");
        assert_eq!(engine.poll_events(), 1);
        assert_eq!((ended.get(), player.get_state().status), (1, TrackPlayerStatus::Stopped));
    }

    #[test]
    fn pausing_fades_out_and_a_resume_continues_from_the_same_place() {
        let engine = engine(&[("a", 4.0, 0.5)]);
        let player = TrackPlayer::new(engine.clone(), TrackPlayerCallbacks::default());
        beatseam_core::http::block_on(player.load(&file("a"))).unwrap();
        player.set_volume(1.0);
        player.play();
        let mut second = vec![0.0; 2 * RATE as usize];
        engine.render(&mut second, 2);
        player.pause();
        engine.render(&mut second, 2); // the half-second fade-out, then silence
        assert!(rms(&second[2 * RATE as usize - 2 * 400..]) < 1e-3, "silent once paused");
        assert!((player.get_state().position_seconds - 1.0).abs() < 1e-9, "the position froze where it was paused");
        player.play();
        assert_eq!(player.get_state().status, TrackPlayerStatus::Playing);
        engine.render(&mut second, 2);
        assert!(rms(&second) > 0.1);
    }

    #[test]
    fn a_crossfade_moves_from_one_track_to_the_next_without_a_gap_and_completes_when_the_outgoing_source_stops() {
        let engine = engine(&[("a", 6.0, 0.5), ("b", 6.0, 0.5)]);
        let completed = Rc::new(Cell::new(0));
        let c = completed.clone();
        let player = TrackPlayer::new(engine.clone(), TrackPlayerCallbacks { on_crossfade_completed: Some(Rc::new(move || c.set(c.get() + 1))), ..Default::default() });
        beatseam_core::http::block_on(player.load(&file("a"))).unwrap();
        player.set_volume(1.0);
        player.play();
        let mut chunk = vec![0.0; 2 * (RATE / 10) as usize]; // 100 ms
        for _ in 0..20 {
            engine.render(&mut chunk, 2); // 2 s in
        }
        let b = beatseam_core::http::block_on(engine.decode_file(&file("b"))).unwrap();
        let plan = beatseam_core::crossfade::TransitionPlan { fade_start_seconds: 2.0, fade_duration_seconds: 2.0, incoming_start_seconds: 0.0 };
        assert_eq!(player.crossfade_to(b, &plan, 1.0), Ok(true));
        let mut energy = Vec::new();
        for _ in 0..40 {
            engine.render(&mut chunk, 2);
            energy.push(rms(&chunk));
            engine.poll_events();
        }
        assert!(energy.iter().all(|e| *e > 0.15), "never a gap or a dip: {energy:?}");
        assert_eq!(completed.get(), 1, "the swap was reported once, after the outgoing source stopped");
        let state = player.get_state();
        assert_eq!((state.status, state.duration_seconds), (TrackPlayerStatus::Playing, 6.0));
        assert!((state.position_seconds - 4.0).abs() < 0.3, "now on the incoming track, about 4 s in: {}", state.position_seconds);
    }

    // ---- the whole stack: files -> decode -> playlist player -> mixer, with real timing

    #[derive(Clone)]
    struct TestRuntime {
        pool: Rc<Pool>,
    }

    impl Runtime for TestRuntime {
        fn spawn(&self, future: Pin<Box<dyn Future<Output = ()>>>) {
            self.pool.spawn(future);
        }
        fn sleep(&self, _millis: u64) -> Pin<Box<dyn Future<Output = ()>>> {
            Box::pin(std::future::pending())
        }
        fn now_ms(&self) -> u64 {
            0
        }
    }

    #[test]
    fn a_playlist_plays_through_with_natural_crossfades_decoding_real_files_and_never_going_silent() {
        let engine = engine(&[("a", 4.0, 0.5), ("b", 4.0, 0.5), ("c", 4.0, 0.5)]);
        let pool = Rc::new(Pool::new());
        let resolve: ResolveTrack = Rc::new(|id: &str| {
            let f = file(id);
            Box::pin(async move { Ok(f) })
        });
        let advanced = Rc::new(RefCell::new(Vec::new()));
        let adv = advanced.clone();
        let player = PlaylistPlayer::new(
            engine.clone(),
            Arc::new(TestRuntime { pool: pool.clone() }),
            resolve,
            PlaylistPlayerOptions { crossfade_seconds: Some(1.0), on_advance: Some(Rc::new(move || adv.borrow_mut().push(()))), ..Default::default() },
        );
        player.set_volume(1.0);
        let p = player.clone();
        pool.spawn(async move { p.set_playlist(vec!["a".into(), "b".into(), "c".into()], None, PlaylistOptions::default()).await });
        pool.run_until_stalled();
        assert_eq!(player.get_state().current_file_id.as_deref(), Some("a"));

        let mut windows = Vec::new();
        let mut chunk = vec![0.0; 2 * (RATE / 10) as usize];
        let mut visited = vec!["a".to_owned()];
        for _ in 0..105 {
            engine.render(&mut chunk, 2);
            windows.push(rms(&chunk));
            engine.poll_events();
            player.check_preload();
            pool.run_until_stalled();
            if let Some(current) = player.get_state().current_file_id {
                if visited.last() != Some(&current) {
                    visited.push(current);
                }
            }
        }
        assert_eq!(visited, ["a", "b", "c"], "advanced through the playlist by itself");
        assert!(advanced.borrow().len() >= 2);
        // Each crossfade starts as soon as the track is within the crossfade time plus the 1 s lead of its end (so ~1 s early, as in the
        // TypeScript): three 4 s tracks play for about 4 + 3 + 1... in all 8 s, with nothing silent until the last one ends.
        let audible = windows.iter().take_while(|w| **w > 0.1).count();
        assert!((79..=81).contains(&audible), "{audible} audible 100 ms windows: {windows:?}");
        assert!(windows[audible..].iter().all(|w| *w < 1e-6), "silent after the last track");
        assert!(windows[20..30].iter().any(|w| *w > 0.4) && windows[40..50].iter().any(|w| *w > 0.4), "the two crossfades are visible as equal-power bumps (correlated signals add)");
        assert_eq!(player.get_state().track.status, TrackPlayerStatus::Stopped, "the last track ended on its own");
    }

    #[test]
    fn decoding_a_missing_or_broken_file_fails_cleanly() {
        let engine = engine(&[]);
        let error = beatseam_core::http::block_on(engine.decode_file(&file("nope"))).unwrap_err();
        assert!(error.0.contains("nope"));
    }

    #[test]
    fn rate_automation_is_declined_rather_than_ignored() {
        let engine = engine(&[("a", 1.0, 0.5)]);
        let audio = beatseam_core::http::block_on(engine.decode_file(&file("a"))).unwrap();
        let source = engine.create_source(&audio, None);
        assert!(source.ramp_rate(RampSpec { to_value: 1.1, at_time_seconds: 0.0, duration_seconds: 1.0 }).is_err());
        assert_eq!(source.id(), format!("source-{}", 1));
    }
}
