//! Port of `packages/core/src/audio-engine/types.ts`: what the playback code needs of a platform's audio output.
//!
//! The shapes mirror the Web Audio automation the TypeScript was written against (one-shot sources, scheduled starts,
//! gain ramps and curves), so the playback logic ports 1:1 and every platform's engine (the Rust mixer on desktop, the
//! AudioWorklet on web, the Android engine) implements the same small surface.

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use crate::audio::DecodedAudio;
use crate::file_access::FileRef;

/// A linear ramp automation, mirroring the Web Audio `AudioParam` shape the playback code actually needs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RampSpec {
    pub to_value: f64,
    pub at_time_seconds: f64,
    pub duration_seconds: f64,
}

/// One playing (or scheduled) source. One-shot: once stopped it can't be restarted, so pause/seek/resume are done by the
/// caller (see the track player) stopping the current source and creating a new one at the desired offset.
pub trait SourceNode {
    fn id(&self) -> &str;
    fn set_gain(&self, value: f64);
    fn ramp_gain(&self, ramp: RampSpec);
    /// Schedules an arbitrary gain curve (evenly-spaced sample values) over `[at_time_seconds, at_time_seconds +
    /// duration_seconds)`, via the engine's native curve automation - used for the crossfade's equal-power fade, since a
    /// single linear ramp can't express that shape (and several back-to-back ramps don't work either: each call anchors
    /// itself to the *current* live gain value at call time, not to where an in-flight scheduled ramp would be by then).
    fn ramp_gain_curve(&self, values: &[f64], at_time_seconds: f64, duration_seconds: f64);
    fn set_rate(&self, value: f64);
    fn ramp_rate(&self, ramp: RampSpec);
    fn stop(&self, when_seconds: Option<f64>);
}

/// Structurally identical to the analysis module's `TrackAnalysis` - duplicated so the engine doesn't depend on analysis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EngineTrackAnalysis {
    pub normalization_gain: f64,
}

/// Why a file could not be decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DecodeError {}

/// Called when a source plays to its end (not when it is stopped).
pub type OnEnded = Box<dyn FnOnce()>;

pub trait AudioEngine {
    type Source: SourceNode;

    /// Decodes an entire file to PCM. Callers slice the first/last-30s windows themselves.
    fn decode_file(&self, file: &FileRef) -> impl Future<Output = Result<Arc<DecodedAudio>, DecodeError>>;

    /// Optional: resolves once `audio` holds real sample data usable for analysis, if it might not yet when
    /// [`decode_file`](Self::decode_file) resolves. Exists for engines (Windows) whose decode returns as soon as playback is
    /// possible - which never reads the samples, it plays from a native-cached buffer - and fill the samples in afterwards in
    /// the background, since that transfer alone can take several seconds on a multi-minute track and playback shouldn't have
    /// to wait for it. Callers that care about analysis correctness must await it before reading samples.
    fn await_analysis_ready(&self, _audio: &DecodedAudio) -> impl Future<Output = ()> {
        async {}
    }

    /// Optional: an engine-native implementation of BPM/loudness analysis, for platforms where running that analysis in the
    /// app's own code visibly stutters the UI (`None` = not provided: use the shared analysis code).
    fn analyze_track(&self, _audio: &DecodedAudio) -> impl Future<Output = Option<EngineTrackAnalysis>> {
        async { None }
    }

    /// Optional: does whatever expensive one-time work [`create_source`](Self::create_source) would otherwise do lazily on
    /// first use, ahead of time (for Web Audio-style engines, building the native buffer). Left undone, a decode that finished
    /// well ahead of time via preload still stalls for that copy at the moment a track needs to start - exactly when a
    /// crossfade animation is running.
    fn prepare_buffer(&self, _audio: &Arc<DecodedAudio>) {}

    fn create_source(&self, audio: &Arc<DecodedAudio>, on_ended: Option<OnEnded>) -> Self::Source;

    /// Schedules playback to start at an engine-clock time (seconds), beginning at `offset_seconds` into the decoded buffer -
    /// for sample-accurate overlap and for resuming/seeking mid-track.
    fn schedule_start(&self, source: &Self::Source, when_seconds: f64, offset_seconds: f64);

    /// The engine's current transport clock, in seconds.
    fn now(&self) -> f64;
}
