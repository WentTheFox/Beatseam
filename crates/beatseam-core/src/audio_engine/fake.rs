//! A scripted [`AudioEngine`] for tests: records every call, keeps a controllable clock, and can reproduce the awkward behaviours
//! of real native engines (an ended callback fired synchronously from `stop`, a rejected start, a scheduling conflict).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use super::{AudioEngine, DecodeError, EngineError, OnEnded, RampSpec, SourceNode};
use crate::audio::DecodedAudio;
use crate::file_access::FileRef;

#[derive(Clone, Debug, PartialEq)]
pub struct GainCurve {
    pub values: Vec<f64>,
    pub at_time_seconds: f64,
    pub duration_seconds: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScheduledStart {
    pub source_id: String,
    pub when_seconds: f64,
    pub offset_seconds: f64,
}

#[derive(Default)]
pub struct FakeState {
    pub clock: Cell<f64>,
    pub stopped_source_ids: RefCell<Vec<String>>,
    /// The time passed to a scheduled (future) `stop`, by source id; `None` for an immediate stop.
    pub stop_when_by_source_id: RefCell<HashMap<String, Option<f64>>>,
    pub gain_by_source_id: RefCell<HashMap<String, f64>>,
    pub gain_ramps_by_source_id: RefCell<HashMap<String, Vec<RampSpec>>>,
    pub rate_ramps_by_source_id: RefCell<HashMap<String, Vec<RampSpec>>>,
    pub gain_curves_by_source_id: RefCell<HashMap<String, Vec<GainCurve>>>,
    pub set_rate_calls_by_source_id: RefCell<HashMap<String, Vec<f64>>>,
    pub schedule_start_calls: RefCell<Vec<ScheduledStart>>,
    /// Some real native engines invoke a source's ended callback SYNCHRONOUSLY from within `stop` - unlike the browser, where
    /// it is always async. Enable this to reproduce that.
    pub fire_ended_synchronously_on_stop: Cell<bool>,
    /// Simulates the engine rejecting a start (e.g. native throwing on a non-finite offset).
    pub fail_schedule_start: Cell<bool>,
    /// Simulates a scheduling conflict on the *incoming* source's gain curve, after the outgoing source's own succeeded.
    pub fail_incoming_ramp_gain_curve: Cell<bool>,
    pub decode_call_count: Cell<usize>,
    /// File ids in the order `decode_file` was called with them.
    pub decoded_file_ids: RefCell<Vec<String>>,
    /// Per-file-id duration override for `decode_file` (the default is `decode_duration_seconds`).
    pub duration_by_file_id: RefCell<HashMap<String, f64>>,
    /// Simulates a scheduling conflict on ANY source's gain curve (the outgoing one is scheduled first).
    pub fail_ramp_gain_curve: Cell<bool>,
    /// The id of the most recently created source.
    pub last_source_id: RefCell<Option<String>>,
    decode_gates: RefCell<HashMap<String, crate::tasks::Gate>>,
    /// Duration of what `decode_file` returns, in seconds (at 100 Hz, so tests stay tiny).
    pub decode_duration_seconds: Cell<f64>,
    next_id: Cell<usize>,
    ended_callbacks: RefCell<HashMap<String, OnEnded>>,
    decode_failures: RefCell<Vec<String>>,
}

/// Shares its state between clones, so a test keeps one handle while the player owns another.
#[derive(Clone)]
pub struct FakeEngine(pub Rc<FakeState>);

pub const FAKE_SAMPLE_RATE: u32 = 100;

/// A decoded buffer of the given length at [`FAKE_SAMPLE_RATE`].
pub fn fake_audio(duration_seconds: f64) -> Arc<DecodedAudio> {
    Arc::new(DecodedAudio { sample_rate: FAKE_SAMPLE_RATE, channel_data: vec![vec![0.0; (duration_seconds * f64::from(FAKE_SAMPLE_RATE)).round() as usize]; 2] })
}

impl Default for FakeEngine {
    fn default() -> Self {
        let state = FakeState::default();
        state.decode_duration_seconds.set(10.0);
        Self(Rc::new(state))
    }
}

impl FakeEngine {
    pub fn set_clock(&self, seconds: f64) {
        self.0.clock.set(seconds);
    }

    /// Holds every decode of `file_id` until [`release_decode`](Self::release_decode) - lets a test build a real overlapping-decode window.
    pub fn gate_decode(&self, file_id: &str) {
        self.0.decode_gates.borrow_mut().insert(file_id.to_owned(), crate::tasks::Gate::new());
    }

    pub fn release_decode(&self, file_id: &str) {
        if let Some(gate) = self.0.decode_gates.borrow().get(file_id) {
            gate.open();
        }
    }

    pub fn set_duration(&self, file_id: &str, seconds: f64) {
        self.0.duration_by_file_id.borrow_mut().insert(file_id.to_owned(), seconds);
    }

    pub fn decode_count(&self, file_id: &str) -> usize {
        self.0.decoded_file_ids.borrow().iter().filter(|id| *id == file_id).count()
    }

    pub fn last_source(&self) -> String {
        self.0.last_source_id.borrow().clone().expect("a source was created")
    }

    /// Makes decodes of a file whose id or name is in the list fail.
    pub fn fail_decoding(&self, file_name: &str) {
        self.0.decode_failures.borrow_mut().push(file_name.to_owned());
    }

    /// Test helper: simulate a source naturally reaching the end of its buffer (or a scheduled stop firing).
    pub fn fire_ended(&self, source_id: &str) {
        let callback = self.0.ended_callbacks.borrow_mut().remove(source_id);
        if let Some(callback) = callback {
            callback();
        }
    }

    pub fn stopped(&self) -> Vec<String> {
        self.0.stopped_source_ids.borrow().clone()
    }

    pub fn starts(&self) -> Vec<ScheduledStart> {
        self.0.schedule_start_calls.borrow().clone()
    }

    pub fn gain_of(&self, source_id: &str) -> Option<f64> {
        self.0.gain_by_source_id.borrow().get(source_id).copied()
    }
}

#[derive(Clone)]
pub struct FakeSource {
    id: String,
    state: Rc<FakeState>,
}

impl SourceNode for FakeSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn set_gain(&self, value: f64) {
        self.state.gain_by_source_id.borrow_mut().insert(self.id.clone(), value);
    }

    fn ramp_gain(&self, ramp: RampSpec) -> Result<(), EngineError> {
        self.state.gain_ramps_by_source_id.borrow_mut().entry(self.id.clone()).or_default().push(ramp);
        Ok(())
    }

    fn ramp_gain_curve(&self, values: &[f64], at_time_seconds: f64, duration_seconds: f64) -> Result<(), EngineError> {
        if self.state.fail_ramp_gain_curve.get() || (self.state.fail_incoming_ramp_gain_curve.get() && self.id != "source-0") {
            return Err(EngineError("NotSupportedError: Cannot schedule event of type SetValueAtTime because it conflicts with an existing curve event".into()));
        }
        self.state.gain_curves_by_source_id.borrow_mut().entry(self.id.clone()).or_default().push(GainCurve { values: values.to_vec(), at_time_seconds, duration_seconds });
        Ok(())
    }

    fn set_rate(&self, value: f64) {
        self.state.set_rate_calls_by_source_id.borrow_mut().entry(self.id.clone()).or_default().push(value);
    }

    fn ramp_rate(&self, ramp: RampSpec) -> Result<(), EngineError> {
        self.state.rate_ramps_by_source_id.borrow_mut().entry(self.id.clone()).or_default().push(ramp);
        Ok(())
    }

    fn stop(&self, when_seconds: Option<f64>) {
        self.state.stopped_source_ids.borrow_mut().push(self.id.clone());
        self.state.stop_when_by_source_id.borrow_mut().insert(self.id.clone(), when_seconds);
        if self.state.fire_ended_synchronously_on_stop.get() {
            let callback = self.state.ended_callbacks.borrow_mut().remove(&self.id);
            if let Some(callback) = callback {
                callback();
            }
        }
    }
}

impl AudioEngine for FakeEngine {
    type Source = FakeSource;

    async fn decode_file(&self, file: &FileRef) -> Result<Arc<DecodedAudio>, DecodeError> {
        self.0.decode_call_count.set(self.0.decode_call_count.get() + 1);
        self.0.decoded_file_ids.borrow_mut().push(file.id.clone());
        if self.0.decode_failures.borrow().iter().any(|f| *f == file.name || *f == file.id) {
            return Err(DecodeError(format!("NotFoundError: no such file \"{}\"", file.id)));
        }
        let gate = self.0.decode_gates.borrow().get(&file.id).cloned();
        if let Some(gate) = gate {
            gate.wait().await;
        }
        let seconds = self.0.duration_by_file_id.borrow().get(&file.id).copied().unwrap_or_else(|| self.0.decode_duration_seconds.get());
        Ok(fake_audio(seconds))
    }

    fn create_source(&self, _audio: &Arc<DecodedAudio>, on_ended: Option<OnEnded>) -> FakeSource {
        let id = format!("source-{}", self.0.next_id.get());
        self.0.next_id.set(self.0.next_id.get() + 1);
        *self.0.last_source_id.borrow_mut() = Some(id.clone());
        if let Some(callback) = on_ended {
            self.0.ended_callbacks.borrow_mut().insert(id.clone(), callback);
        }
        FakeSource { id, state: self.0.clone() }
    }

    fn schedule_start(&self, source: &FakeSource, when_seconds: f64, offset_seconds: f64) -> Result<(), EngineError> {
        if self.0.fail_schedule_start.get() {
            return Err(EngineError("The provided double value is non-finite.".into()));
        }
        self.0.schedule_start_calls.borrow_mut().push(ScheduledStart { source_id: source.id.clone(), when_seconds, offset_seconds });
        Ok(())
    }

    fn now(&self) -> f64 {
        self.0.clock.get()
    }
}
