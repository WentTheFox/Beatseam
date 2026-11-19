use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::Arc;

use crate::audio::DecodedAudio;
use crate::audio_engine::{AudioEngine, DecodeError, EngineError, RampSpec, SourceNode};
use crate::crossfade::{TransitionPlan, sample_equal_power_gain_curve};
use crate::file_access::FileRef;

/// How many points to sample the equal-power gain curve at for crossfades - the engine interpolates between them, so this
/// just needs to be smooth enough to not sound stepped.
const GAIN_CURVE_SAMPLE_COUNT: usize = 32;
/// Extra real seconds the outgoing source is left playing (already silent, per the gain curve) after its fade curve's own
/// nominal duration, before its stop is actually scheduled - a cushion against the physical cutoff landing a hair before the
/// ramp automation has genuinely finished (native scheduling/thread timing isn't perfectly exact), which would otherwise be
/// audible as an abrupt cutoff rather than the fade actually reaching silence on its own.
const STOP_TAIL_SECONDS: f64 = 0.2;
/// Cap for the fade-out on pause and fade-in on resume - avoids an audible pop/click from an instant gain cut or jump, while
/// staying quick enough that it doesn't read as a real crossfade.
const PAUSE_RESUME_FADE_SECONDS: f64 = 0.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackPlayerStatus {
    Idle,
    Loading,
    Playing,
    Paused,
    Stopped,
}

/// The incoming side of a crossfade already scheduled by [`TrackPlayer::crossfade_to`], live-tracked the same way the
/// outgoing track's position is. A caller that switches its own display (track name, seek bar) to this the instant it is
/// present, rather than waiting for the completion callback, avoids an abrupt seek-bar jump: without it, the bar keeps
/// climbing toward the OUTGOING track's own duration for the entire audible fade, then snaps to a small fraction of the
/// (usually much longer) incoming track's duration the instant the swap actually completes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PendingIncoming {
    pub position_seconds: f64,
    pub duration_seconds: f64,
    pub fade_duration_seconds: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackPlayerState {
    pub status: TrackPlayerStatus,
    /// True whenever a real source is actually generating sound right now - independent of `status`, which describes the
    /// CURRENT track's own lifecycle (a fresh play request sets status to `Loading` for the track it's decoding, but never
    /// touches whatever source is already playing). Lets a caller tell "genuinely silent" apart from "the next track is
    /// loading while the previous one is still audible", which `status` alone can't distinguish - used to decide whether the
    /// play/pause control should show/act as pause even while status reads `Loading`.
    pub is_audible: bool,
    pub position_seconds: f64,
    pub duration_seconds: f64,
    pub pending_incoming: Option<PendingIncoming>,
}

#[derive(Default)]
pub struct TrackPlayerCallbacks {
    /// Fired when the track finishes playing on its own - never on an explicit stop/pause/seek, and never for a track that
    /// ended via a crossfade (see `on_crossfade_completed`).
    pub on_ended: Option<Rc<dyn Fn()>>,
    /// Fired when a transition scheduled by [`TrackPlayer::crossfade_to`] actually completes and the incoming track becomes
    /// "current" - distinct from `on_ended`, since the transition already started the next track's audio itself; callers
    /// should just advance their own position bookkeeping, not start anything again.
    pub on_crossfade_completed: Option<Rc<dyn Fn()>>,
}

struct PendingCrossfade<S> {
    source: Rc<S>,
    decoded: Arc<DecodedAudio>,
    started_at_engine_time: f64,
    start_offset_seconds: f64,
    gain: f64,
    rate: f64,
    fade_duration_seconds: f64,
}

struct State<S> {
    decoded: Option<Arc<DecodedAudio>>,
    source: Option<Rc<S>>,
    status: TrackPlayerStatus,
    started_at_engine_time: f64,
    start_offset_seconds: f64,
    load_token: u64,
    current_gain: f64,
    /// The rate the current source is actually playing at - always 1 for now (no rate/speed manipulation this round). Kept,
    /// rather than assumed to always be 1, so position tracking stays correct if a rate change comes back in a later round;
    /// re-applied to every new source the same way the gain is (seek/pause/resume all tear down and recreate the source).
    current_rate: f64,
    /// User-facing master volume [0,1], independent of `current_gain` (per-track normalization) - the two multiply together
    /// into whatever's actually sent to the engine. Persists across tracks (unlike `current_gain`, which is per-track).
    master_volume: f64,
    pending_crossfade: Option<PendingCrossfade<S>>,
    /// Set by `pause` when it's called during `Loading` - a real, distinct case (the previous track can still be genuinely
    /// audible for the whole decode window). Consumed by `play` once the pending decode actually lands, so the play request's
    /// own autoplay intent doesn't un-pause a track the person explicitly silenced while it was still loading. Reset by
    /// `mark_loading` too, so a stale pause from a superseded load can't leak onto the new one.
    paused_while_loading: bool,
}

struct Inner<E: AudioEngine> {
    engine: Rc<E>,
    callbacks: TrackPlayerCallbacks,
    state: RefCell<State<E::Source>>,
}

/// Play/pause/seek/stop on top of the engine's one-shot source primitive. A source can't be paused or restarted once stopped,
/// so this stops the current source and creates a new one (at the resume/seek offset) whenever playback needs to continue
/// from somewhere other than "where it just was".
///
/// Engine calls are never made while the internal state is borrowed: some native engines invoke a source's ended callback
/// synchronously from within `stop`, which then re-enters the player.
pub struct TrackPlayer<E: AudioEngine + 'static> {
    inner: Rc<Inner<E>>,
}

impl<E: AudioEngine + 'static> TrackPlayer<E> {
    pub fn new(engine: Rc<E>, callbacks: TrackPlayerCallbacks) -> Self {
        let state = State {
            decoded: None,
            source: None,
            status: TrackPlayerStatus::Idle,
            started_at_engine_time: 0.0,
            start_offset_seconds: 0.0,
            load_token: 0,
            current_gain: 1.0,
            current_rate: 1.0,
            master_volume: 0.25,
            pending_crossfade: None,
            paused_while_loading: false,
        };
        Self { inner: Rc::new(Inner { engine, callbacks, state: RefCell::new(state) }) }
    }

    /// Decodes `file` and loads it. Stops whatever is loaded first.
    pub async fn load(&self, file: &FileRef) -> Result<(), DecodeError> {
        self.stop();
        let token = {
            let mut state = self.inner.state.borrow_mut();
            state.status = TrackPlayerStatus::Loading;
            // If a newer load supersedes this one before the decode resolves, this call must not touch state on completion: a
            // slow superseded decode finishing after a newer, faster one has already started playing would otherwise
            // silently overwrite the decoded buffer and force status back to `Stopped` out from under the track that's
            // actually audible right now.
            state.load_token += 1;
            state.load_token
        };
        let decoded = self.inner.engine.decode_file(file).await?;
        if self.inner.state.borrow().load_token != token {
            return Ok(());
        }
        self.apply_decoded(decoded);
        Ok(())
    }

    /// Marks status `Loading` immediately - for callers that decode outside of [`load`](Self::load) (e.g. to run the decode
    /// and gain lookup concurrently) but still want [`get_state`](Self::get_state) to reflect "loading" for the whole decode
    /// window.
    pub fn mark_loading(&self) {
        let mut state = self.inner.state.borrow_mut();
        state.status = TrackPlayerStatus::Loading;
        // A fresh load supersedes whatever pause intent applied to a PREVIOUS pending load (the person skipped again before
        // that one's decode even finished) - without this, a stale flag could wrongly land THIS unrelated one paused instead
        // of autoplaying it as actually requested.
        state.paused_while_loading = false;
    }

    /// Counterpart to [`mark_loading`](Self::mark_loading) for when the decode it was covering never arrives. Without it, a
    /// failed decode left status stuck on `Loading` forever: the error is reported as a one-shot event, while the status is
    /// polled continuously by the UI, so the spinner just ran with no way to tell the track had actually failed. Resets to
    /// `Idle`, this player's own initial state - there is no decoded buffer to fall back to.
    pub fn mark_load_failed(&self) {
        self.inner.state.borrow_mut().status = TrackPlayerStatus::Idle;
    }

    /// Loads an already-decoded buffer synchronously - no decode round trip. Makes an advance to a track the preload
    /// scheduler already finished decoding ahead of time effectively instant.
    pub fn load_decoded(&self, decoded: Arc<DecodedAudio>) {
        self.stop();
        self.inner.state.borrow_mut().load_token += 1; // invalidates any in-flight async load a caller superseded with this
        self.apply_decoded(decoded);
    }

    fn apply_decoded(&self, decoded: Arc<DecodedAudio>) {
        let mut state = self.inner.state.borrow_mut();
        state.decoded = Some(decoded);
        state.start_offset_seconds = 0.0;
        state.status = TrackPlayerStatus::Stopped;
        // A fresh load always starts at normal speed - only an in-progress crossfade can set this to anything else, and that
        // shouldn't leak into an unrelated track loaded afterward.
        state.current_rate = 1.0;
    }

    pub fn play(&self) {
        // A no-op, not an error: a manual play landing while a load is still in flight has nothing to do yet - callers that
        // want auto-play once ready call play again after the load resolves.
        let from = {
            let mut state = self.inner.state.borrow_mut();
            if state.decoded.is_none() || matches!(state.status, TrackPlayerStatus::Loading | TrackPlayerStatus::Playing) {
                return;
            }
            if state.paused_while_loading {
                // This decode's own pending autoplay is what's calling play right now - the person explicitly paused while it
                // was still in flight, so land here paused instead of autoplaying out from under that.
                state.paused_while_loading = false;
                state.status = TrackPlayerStatus::Paused;
                return;
            }
            // Only a genuine resume-from-pause fades in - a fresh track start should still begin at full volume immediately.
            (state.start_offset_seconds, if state.status == TrackPlayerStatus::Paused { PAUSE_RESUME_FADE_SECONDS } else { 0.0 })
        };
        self.start_playback_from(from.0, from.1);
    }

    pub fn pause(&self) {
        let (source, loading) = {
            let mut state = self.inner.state.borrow_mut();
            match state.status {
                TrackPlayerStatus::Loading => {
                    // A new track is decoding in the background while the previous one may still be genuinely audible right
                    // now - `mark_loading` only flips status, it never touches the source, so there's real audio to silence
                    // here. The pending decode itself has nothing to pause yet; `paused_while_loading` stops its own
                    // eventual autoplay from undoing this the instant it lands.
                    state.paused_while_loading = true;
                    (None, true)
                }
                TrackPlayerStatus::Playing => (Some(()), false),
                _ => return,
            }
        };
        let _ = source;
        let position = self.get_position_seconds();
        // Cancel any in-flight crossfade first, then fade the source out and schedule its stop rather than cutting it
        // immediately - an instant gain cut on pause is an audible pop/click. The source is taken out of the state before
        // scheduling the fade/stop (not after), because some native engines invoke a source's ended callback synchronously
        // from within `stop`, and the staleness guard needs the state to already be without it.
        let taken = {
            if loading && self.inner.state.borrow().source.is_none() {
                return;
            }
            self.cancel_pending_crossfade();
            let mut state = self.inner.state.borrow_mut();
            let source = state.source.take();
            state.start_offset_seconds = position;
            if !loading {
                state.status = TrackPlayerStatus::Paused;
            }
            source
        };
        if let Some(source) = taken {
            self.fade_out_and_stop(&source);
        }
    }

    /// Fades a still-audible source out and schedules its stop rather than cutting it immediately.
    fn fade_out_and_stop(&self, source: &E::Source) {
        let when = self.inner.engine.now();
        let scheduled = source.ramp_gain(RampSpec { to_value: 0.0, at_time_seconds: when, duration_seconds: PAUSE_RESUME_FADE_SECONDS });
        if scheduled.is_ok() {
            source.stop(Some(when + PAUSE_RESUME_FADE_SECONDS + STOP_TAIL_SECONDS));
        } else {
            // A scheduling conflict or bad engine state - fall back to an immediate stop rather than leaving the source
            // playing forever.
            source.stop(None);
        }
    }

    pub fn seek(&self, position_seconds: f64) {
        if !position_seconds.is_finite() {
            // Guards against bad input anywhere upstream (e.g. a UI coordinate calculation gone wrong) turning into a
            // non-finite value the engine has to reject.
            return;
        }
        let (clamped, was_playing) = {
            let state = self.inner.state.borrow();
            let duration = state.decoded.as_ref().map_or(0.0, |d| d.duration_seconds());
            (position_seconds.min(duration).max(0.0), state.status == TrackPlayerStatus::Playing)
        };
        self.stop_current_source();
        self.inner.state.borrow_mut().start_offset_seconds = clamped;
        if was_playing {
            self.start_playback_from(clamped, 0.0);
        } else {
            let mut state = self.inner.state.borrow_mut();
            if !matches!(state.status, TrackPlayerStatus::Idle | TrackPlayerStatus::Loading) {
                state.status = TrackPlayerStatus::Paused;
            }
        }
    }

    pub fn stop(&self) {
        self.stop_current_source();
        let mut state = self.inner.state.borrow_mut();
        state.start_offset_seconds = 0.0;
        if !matches!(state.status, TrackPlayerStatus::Idle | TrackPlayerStatus::Loading) {
            state.status = TrackPlayerStatus::Stopped;
        }
    }

    /// Sets the gain multiplier applied to this track (normalization, relative to the fixed reference loudness target
    /// computed during analysis). Takes effect on the currently playing source immediately, and on every source created
    /// afterward (seek/resume/pause all tear down and recreate the source, so this has to be remembered and re-applied each
    /// time, not just set once).
    pub fn set_gain(&self, value: f64) {
        let (source, volume) = {
            let mut state = self.inner.state.borrow_mut();
            state.current_gain = value;
            (state.source.clone(), state.master_volume)
        };
        if let Some(source) = source {
            source.set_gain(value * volume);
        }
    }

    /// Sets the user-facing master volume [0,1] (e.g. a UI slider) - takes effect on the currently playing source
    /// immediately, and on every source created afterward, same as [`set_gain`](Self::set_gain). Independent of the
    /// per-track normalization gain; the two multiply together.
    pub fn set_volume(&self, value: f64) {
        let (source, gain, volume) = {
            let mut state = self.inner.state.borrow_mut();
            state.master_volume = value.clamp(0.0, 1.0);
            (state.source.clone(), state.current_gain, state.master_volume)
        };
        if let Some(source) = source {
            source.set_gain(gain * volume);
        }
    }

    pub fn get_volume(&self) -> f64 {
        self.inner.state.borrow().master_volume
    }

    /// Takes the source out of the state BEFORE calling `stop` on it, not after: some native engines invoke the source's ended
    /// callback synchronously from within `stop`, and the "is this a stale/superseded source" guard would otherwise see them
    /// as equal and treat an intentional stop as a natural end, potentially cascading into overlapping play requests under
    /// rapid seeking.
    fn stop_current_source(&self) {
        self.cancel_pending_crossfade();
        let old = self.inner.state.borrow_mut().source.take();
        if let Some(old) = old {
            old.stop(None);
        }
    }

    /// Any explicit pause/seek/stop/reload during a pending crossfade cancels the whole transition, not just the outgoing
    /// source - without this, the incoming source scheduled by the crossfade would keep playing/ramping in the background with
    /// nothing tracking it, and could resurface later (e.g. resuming playback on the wrong track).
    fn cancel_pending_crossfade(&self) {
        let pending = self.inner.state.borrow_mut().pending_crossfade.take();
        if let Some(pending) = pending {
            pending.source.stop(None);
        }
    }

    /// Schedules a volume-only crossfade into `next` per `plan`, on top of the track already playing: a new source for `next`
    /// starts at `plan.incoming_start_seconds`, at rate 1, essentially immediately ("little to no delay" - no ramp/wait
    /// phase this round), while the outgoing source's gain fades out - both at rate 1 throughout, no tempo matching.
    ///
    /// Returns `Ok(false)` (no-op) if there's nothing currently playing to transition from - callers should fall back to a
    /// hard cut in that case, same as an exhausted preload. Returns an error (after cleaning up) if the engine rejects a
    /// scheduling call partway through.
    ///
    /// Doesn't swap the current track over immediately - that happens once the outgoing source's scheduled stop actually
    /// fires, so [`get_state`](Self::get_state) keeps reporting the outgoing track's own position for as long as it's still
    /// the audible "current" track.
    pub fn crossfade_to(&self, next: Arc<DecodedAudio>, plan: &TransitionPlan, next_gain: f64) -> Result<bool, EngineError> {
        {
            let state = self.inner.state.borrow();
            if state.status != TrackPlayerStatus::Playing || state.source.is_none() || state.decoded.is_none() {
                return Ok(false);
            }
        }
        // A second crossfade request while one's already in flight (e.g. rapid manual next/previous presses each scheduling
        // their own short crossfade) - cancel the stale pending incoming source first, rather than layering a second
        // scheduled gain automation on top of the first on the same still-live outgoing source, which native engines can
        // reject as a scheduling conflict.
        self.cancel_pending_crossfade();
        let (old_source, current_gain, master_volume) = {
            let state = self.inner.state.borrow();
            (state.source.clone().expect("checked above"), state.current_gain, state.master_volume)
        };
        let fade_when = self.inner.engine.now();

        // If an engine scheduling call fails partway through (e.g. a real conflict on a parameter that already has something
        // scheduled on it), the caller must get a clean error back, not a half-scheduled outgoing source paired with a
        // success. A caller that retried on the very next tick after that would schedule *another* conflicting automation on
        // top of whatever this attempt already managed to apply - repeatedly, since the same conflict would just recur -
        // which reads as random, escalating gain-jump glitches rather than a single clean failure.
        //
        // Equal-power, not a straight linear ramp: a linear gain fade spends much of its duration well under its target value
        // (a 20 s linear fade-in is still under 10% of target 2 s in), which reads as "barely playing". The outgoing curve is
        // scaled by its own normalization gain (without it the automation's first sample jumps straight to the raw start of
        // the curve instead of continuing from wherever the gain actually was, an audible volume jump right as the fade
        // begins); both are scaled by the master volume, like every other gain applied to a source.
        let outgoing: Vec<f64> = sample_equal_power_gain_curve(GAIN_CURVE_SAMPLE_COUNT, true, plan.fade_duration_seconds).iter().map(|v| v * current_gain * master_volume).collect();
        let incoming: Vec<f64> = sample_equal_power_gain_curve(GAIN_CURVE_SAMPLE_COUNT, false, plan.fade_duration_seconds).iter().map(|v| v * next_gain * master_volume).collect();

        old_source.ramp_gain_curve(&outgoing, fade_when, plan.fade_duration_seconds)?;
        old_source.stop(Some(fade_when + plan.fade_duration_seconds + STOP_TAIL_SECONDS));

        let id_cell: Rc<RefCell<Option<String>>> = Rc::default();
        let incoming_source = Rc::new(self.inner.engine.create_source(&next, Some(self.ended_callback(&id_cell))));
        *id_cell.borrow_mut() = Some(incoming_source.id().to_owned());
        incoming_source.set_gain(0.0);
        let scheduled = self.inner.engine.schedule_start(&incoming_source, fade_when, plan.incoming_start_seconds).and_then(|()| incoming_source.ramp_gain_curve(&incoming, fade_when, plan.fade_duration_seconds));
        if let Err(error) = scheduled {
            // Best-effort cleanup - don't leave an orphaned, already-started incoming source with nothing tracking it.
            incoming_source.stop(None);
            return Err(error);
        }

        self.inner.state.borrow_mut().pending_crossfade = Some(PendingCrossfade {
            source: incoming_source,
            decoded: next,
            started_at_engine_time: fade_when,
            start_offset_seconds: plan.incoming_start_seconds,
            gain: next_gain,
            rate: 1.0,
            fade_duration_seconds: plan.fade_duration_seconds,
        });
        Ok(true)
    }

    pub fn get_state(&self) -> TrackPlayerState {
        let state = self.inner.state.borrow();
        TrackPlayerState {
            status: state.status,
            is_audible: state.source.is_some(),
            position_seconds: self.position_of(&state),
            duration_seconds: state.decoded.as_ref().map_or(0.0, |d| d.duration_seconds()),
            pending_incoming: state.pending_crossfade.as_ref().map(|pending| PendingIncoming {
                // Clamped at 0: the incoming source is scheduled at the crossfade's start, essentially "now" but not
                // necessarily audible yet the very instant the crossfade call returns and a caller reads the state - avoids
                // a fleeting negative position.
                position_seconds: (pending.start_offset_seconds + (self.inner.engine.now() - pending.started_at_engine_time) * pending.rate).max(0.0),
                duration_seconds: pending.decoded.duration_seconds(),
                fade_duration_seconds: pending.fade_duration_seconds,
            }),
        }
    }

    fn get_position_seconds(&self) -> f64 {
        self.position_of(&self.inner.state.borrow())
    }

    /// Keyed on the source (see `is_audible`), not `status` - a fresh play request sets status to `Loading` for the track it's
    /// decoding without touching whatever source is already playing, so gating on status alone would freeze the reported
    /// position of a still-genuinely-playing previous track for the whole background-load window instead of continuing to
    /// advance it.
    fn position_of(&self, state: &State<E::Source>) -> f64 {
        if state.source.is_none() {
            return state.start_offset_seconds;
        }
        state.start_offset_seconds + (self.inner.engine.now() - state.started_at_engine_time) * state.current_rate
    }

    /// The ended callback for a source whose id isn't known until the engine has created it.
    fn ended_callback(&self, id: &Rc<RefCell<Option<String>>>) -> Box<dyn FnOnce()> {
        let (weak, id): (Weak<Inner<E>>, _) = (Rc::downgrade(&self.inner), id.clone());
        Box::new(move || {
            // Fired from inside the engine's own creation call (before the id was recorded): nothing to match it against.
            let Some(id) = id.borrow().clone() else { return };
            if let Some(inner) = weak.upgrade() {
                TrackPlayer { inner }.handle_ended(&id);
            }
        })
    }

    /// `fade_in_seconds` > 0 starts the source silent and ramps it up to its real gain instead of jumping straight there -
    /// used for a resume so it doesn't pop back in at full volume; a fresh seek while already playing passes 0 (no fade,
    /// matches the instant jump the UI shows).
    fn start_playback_from(&self, offset_seconds: f64, fade_in_seconds: f64) {
        let Some(decoded) = self.inner.state.borrow().decoded.clone() else { return };
        let duration = decoded.duration_seconds();
        if offset_seconds >= duration {
            // Nothing left to play - don't hand the engine a zero-length source.
            let mut state = self.inner.state.borrow_mut();
            state.source = None;
            state.start_offset_seconds = duration;
            state.status = TrackPlayerStatus::Stopped;
            return;
        }

        let when = self.inner.engine.now();
        let id_cell: Rc<RefCell<Option<String>>> = Rc::default();
        let source = Rc::new(self.inner.engine.create_source(&decoded, Some(self.ended_callback(&id_cell))));
        *id_cell.borrow_mut() = Some(source.id().to_owned());
        // Recorded before the start is scheduled (not after): if the engine invokes the ended callback synchronously from
        // within it - some native engines do, for a source that turns out to have nothing playable - the staleness guard
        // needs the state to already hold the new source, so it processes a genuine immediate end correctly instead of
        // silently dropping it.
        let (target_gain, rate) = {
            let mut state = self.inner.state.borrow_mut();
            state.source = Some(source.clone());
            state.started_at_engine_time = when;
            state.start_offset_seconds = offset_seconds;
            state.status = TrackPlayerStatus::Playing;
            (state.current_gain * state.master_volume, state.current_rate)
        };
        if fade_in_seconds > 0.0 {
            source.set_gain(0.0);
            let _ = source.ramp_gain(RampSpec { to_value: target_gain, at_time_seconds: when, duration_seconds: fade_in_seconds });
        } else {
            source.set_gain(target_gain);
        }
        source.set_rate(rate);
        if self.inner.engine.schedule_start(&source, when, offset_seconds).is_err() {
            // The engine rejected this start (e.g. native throws synchronously on a non-finite offset). Without this, state
            // above already claims `Playing` with a source that was never actually started - nothing will ever fire its ended
            // callback, so the player would be stuck forever ("playback just stops and never advances"). Treat it as an
            // immediate natural end instead, so a playlist can move on to the next track.
            self.handle_ended(source.id());
        }
    }

    fn handle_ended(&self, source_id: &str) {
        enum Outcome {
            Ignored,
            CrossfadeCompleted,
            Ended,
        }
        let outcome = {
            let mut state = self.inner.state.borrow_mut();
            let is_current = state.source.as_ref().is_some_and(|s| s.id() == source_id);
            // The outgoing source's scheduled stop (from a crossfade) firing - this IS the trustworthy "the transition has
            // completed" signal (more precise than polling the engine clock against a separately-tracked completion time), so
            // swap over to the incoming source that's already been playing/ramped-in throughout the transition, instead of
            // treating this as a natural end.
            if is_current && state.pending_crossfade.is_some() {
                let pending = state.pending_crossfade.take().expect("checked");
                state.source = Some(pending.source);
                state.decoded = Some(pending.decoded);
                state.started_at_engine_time = pending.started_at_engine_time;
                state.start_offset_seconds = pending.start_offset_seconds;
                state.current_gain = pending.gain;
                state.current_rate = pending.rate;
                Outcome::CrossfadeCompleted
            } else if !is_current {
                // A callback from a source we've already moved past (stopped early for pause/seek).
                Outcome::Ignored
            } else {
                state.source = None;
                state.start_offset_seconds = state.decoded.as_ref().map_or(0.0, |d| d.duration_seconds());
                state.status = TrackPlayerStatus::Stopped;
                Outcome::Ended
            }
        };
        match outcome {
            Outcome::Ignored => {}
            Outcome::CrossfadeCompleted => {
                if let Some(callback) = self.inner.callbacks.on_crossfade_completed.clone() {
                    callback();
                }
            }
            Outcome::Ended => {
                if let Some(callback) = self.inner.callbacks.on_ended.clone() {
                    callback();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::audio_engine::fake::{FakeEngine, fake_audio};
    use crate::http::block_on;

    fn file() -> FileRef {
        FileRef { id: "f1".into(), name: "track.mp3".into(), relative_path: "track.mp3".into(), size_bytes: 0, last_modified_ms: 0.0 }
    }

    fn setup() -> (Rc<FakeEngine>, TrackPlayer<FakeEngine>) {
        let engine = Rc::new(FakeEngine::default());
        let player = TrackPlayer::new(engine.clone(), TrackPlayerCallbacks::default());
        block_on(player.load(&file())).unwrap();
        (engine, player)
    }

    fn state(status: TrackPlayerStatus, audible: bool, position: f64, duration: f64) -> TrackPlayerState {
        TrackPlayerState { status, is_audible: audible, position_seconds: position, duration_seconds: duration, pending_incoming: None }
    }

    #[test]
    fn starts_stopped_with_the_decoded_duration_and_zero_position() {
        let (_, player) = setup();
        assert_eq!(player.get_state(), state(TrackPlayerStatus::Stopped, false, 0.0, 10.0));
    }

    #[test]
    fn play_advances_position_with_the_engine_clock() {
        let (engine, player) = setup();
        player.play();
        engine.set_clock(3.0);
        assert_eq!(player.get_state(), state(TrackPlayerStatus::Playing, true, 3.0, 10.0));
    }

    #[test]
    fn pause_freezes_position_and_stops_the_underlying_source() {
        let (engine, player) = setup();
        player.play();
        engine.set_clock(4.0);
        player.pause();
        engine.set_clock(10.0); // no further effect while paused
        assert_eq!(player.get_state(), state(TrackPlayerStatus::Paused, false, 4.0, 10.0));
        assert_eq!(engine.stopped(), ["source-0"]);
    }

    #[test]
    fn play_after_pause_resumes_from_the_paused_position() {
        let (engine, player) = setup();
        player.play();
        engine.set_clock(4.0);
        player.pause();
        engine.set_clock(5.0);
        player.play();
        engine.set_clock(7.0);
        assert_eq!(player.get_state().position_seconds, 6.0); // 4 + (7 - 5)
    }

    #[test]
    fn pause_fades_the_outgoing_source_out_and_schedules_its_stop_rather_than_cutting_it_immediately() {
        let (engine, player) = setup();
        player.play();
        engine.set_clock(4.0);
        player.pause();
        let ramps = engine.0.gain_ramps_by_source_id.borrow().get("source-0").cloned().unwrap();
        assert_eq!(ramps, [RampSpec { to_value: 0.0, at_time_seconds: 4.0, duration_seconds: 0.5 }]);
        assert_eq!(engine.0.stop_when_by_source_id.borrow().get("source-0"), Some(&Some(4.0 + 0.5 + 0.2)));
    }

    #[test]
    fn play_after_pause_fades_the_resumed_source_back_in_but_a_fresh_play_does_not() {
        let (engine, player) = setup();
        player.play(); // source-0: fresh
        assert!(engine.0.gain_ramps_by_source_id.borrow().get("source-0").is_none());
        engine.set_clock(4.0);
        player.pause();
        engine.set_clock(5.0);
        player.play(); // source-1: a resume
        assert_eq!(engine.gain_of("source-1"), Some(0.0));
        let ramps = engine.0.gain_ramps_by_source_id.borrow().get("source-1").cloned().unwrap();
        assert_eq!(ramps, [RampSpec { to_value: 0.25, at_time_seconds: 5.0, duration_seconds: 0.5 }], "to the default master volume");
    }

    #[test]
    fn seek_while_playing_restarts_playback_from_the_new_offset() {
        let (engine, player) = setup();
        player.play();
        engine.set_clock(2.0);
        player.seek(6.0);
        engine.set_clock(3.0);
        assert_eq!(player.get_state(), state(TrackPlayerStatus::Playing, true, 7.0, 10.0));
        assert_eq!(engine.starts().last().map(|s| s.offset_seconds), Some(6.0));
        assert_eq!(engine.stopped(), ["source-0"]);
    }

    #[test]
    fn seek_while_paused_updates_position_without_starting_playback_and_is_clamped() {
        let (engine, player) = setup();
        player.seek(4.0);
        assert_eq!(player.get_state(), state(TrackPlayerStatus::Paused, false, 4.0, 10.0));
        assert!(engine.starts().is_empty());
        player.seek(99.0);
        assert_eq!(player.get_state().position_seconds, 10.0);
        player.seek(-5.0);
        assert_eq!(player.get_state().position_seconds, 0.0);
    }

    #[test]
    fn stop_resets_position_to_zero() {
        let (engine, player) = setup();
        player.play();
        engine.set_clock(5.0);
        player.stop();
        assert_eq!(player.get_state(), state(TrackPlayerStatus::Stopped, false, 0.0, 10.0));
    }

    #[test]
    fn transitions_to_stopped_when_the_source_reports_it_ended_naturally() {
        let (engine, player) = setup();
        player.play();
        engine.fire_ended("source-0");
        assert_eq!(player.get_state(), state(TrackPlayerStatus::Stopped, false, 10.0, 10.0));
    }

    #[test]
    fn ignores_an_ended_callback_from_a_source_already_superseded_by_seek_or_pause() {
        let (engine, player) = setup();
        player.play();
        player.seek(5.0); // source-0 stopped, source-1 playing
        engine.fire_ended("source-0");
        assert_eq!(player.get_state().status, TrackPlayerStatus::Playing);
    }

    #[test]
    fn fires_on_ended_when_the_track_finishes_naturally_but_not_on_explicit_stop_pause_or_seek() {
        let engine = Rc::new(FakeEngine::default());
        let ended = Rc::new(Cell::new(0));
        let e = ended.clone();
        let player = TrackPlayer::new(engine.clone(), TrackPlayerCallbacks { on_ended: Some(Rc::new(move || e.set(e.get() + 1))), ..Default::default() });
        block_on(player.load(&file())).unwrap();
        player.play();
        player.pause();
        player.play();
        player.seek(2.0);
        player.stop();
        assert_eq!(ended.get(), 0);
        player.play(); // source-3
        engine.fire_ended("source-3");
        assert_eq!(ended.get(), 1);
    }

    #[test]
    fn an_engine_that_fires_ended_synchronously_from_stop_is_not_mistaken_for_a_natural_end() {
        let engine = Rc::new(FakeEngine::default());
        engine.0.fire_ended_synchronously_on_stop.set(true);
        let ended = Rc::new(Cell::new(0));
        let e = ended.clone();
        let player = TrackPlayer::new(engine.clone(), TrackPlayerCallbacks { on_ended: Some(Rc::new(move || e.set(e.get() + 1))), ..Default::default() });
        block_on(player.load(&file())).unwrap();
        player.play();
        player.seek(5.0);
        player.pause();
        player.play();
        player.stop();
        assert_eq!(ended.get(), 0);
    }

    #[test]
    fn seek_ignores_non_finite_input() {
        let (engine, player) = setup();
        player.play();
        engine.set_clock(2.0);
        player.seek(f64::NAN);
        player.seek(f64::INFINITY);
        assert_eq!(player.get_state(), state(TrackPlayerStatus::Playing, true, 2.0, 10.0));
        assert_eq!(engine.starts().len(), 1);
    }

    mod pause_while_loading {
        use super::*;

        #[test]
        fn silences_the_still_playing_previous_source_and_freezes_its_position() {
            let (engine, player) = setup();
            player.play();
            engine.set_clock(3.0);
            player.mark_loading();
            engine.set_clock(4.0);
            player.pause();
            engine.set_clock(9.0);
            assert_eq!(player.get_state(), state(TrackPlayerStatus::Loading, false, 4.0, 10.0));
            assert_eq!(engine.0.stop_when_by_source_id.borrow().get("source-0"), Some(&Some(4.0 + 0.5 + 0.2)));
        }

        #[test]
        fn is_a_harmless_no_op_if_nothing_was_actually_playing_yet() {
            let engine = Rc::new(FakeEngine::default());
            let player = TrackPlayer::new(engine.clone(), TrackPlayerCallbacks::default());
            player.mark_loading();
            player.pause();
            assert_eq!(player.get_state().status, TrackPlayerStatus::Loading);
            assert!(engine.stopped().is_empty());
        }

        #[test]
        fn lands_the_pending_track_paused_instead_of_autoplaying_once_its_decode_completes() {
            let (engine, player) = setup();
            player.play();
            player.mark_loading();
            player.pause();
            player.load_decoded(fake_audio(8.0));
            player.play(); // the pending decode's own autoplay
            assert_eq!(player.get_state().status, TrackPlayerStatus::Paused);
            assert_eq!(engine.starts().len(), 1, "no second source was started");
            player.play(); // a deliberate resume
            assert_eq!(player.get_state().status, TrackPlayerStatus::Playing);
        }

        #[test]
        fn a_fresh_mark_loading_discards_the_stale_pause_so_that_track_still_autoplays() {
            let (_, player) = setup();
            player.play();
            player.mark_loading();
            player.pause();
            player.mark_loading(); // skipped again before the first load finished
            player.load_decoded(fake_audio(8.0));
            player.play();
            assert_eq!(player.get_state().status, TrackPlayerStatus::Playing);
        }
    }

    #[test]
    fn recovers_instead_of_getting_stuck_if_the_engine_rejects_a_start() {
        let engine = Rc::new(FakeEngine::default());
        let ended = Rc::new(Cell::new(0));
        let e = ended.clone();
        let player = TrackPlayer::new(engine.clone(), TrackPlayerCallbacks { on_ended: Some(Rc::new(move || e.set(e.get() + 1))), ..Default::default() });
        block_on(player.load(&file())).unwrap();
        engine.0.fail_schedule_start.set(true);
        player.play();
        assert_eq!(ended.get(), 1, "treated as an immediate natural end so a playlist can move on");
        assert_eq!(player.get_state().status, TrackPlayerStatus::Stopped);
    }

    #[test]
    fn a_stale_load_resolving_after_a_newer_one_must_not_corrupt_the_now_playing_state() {
        use crate::tasks::Gate;
        use crate::tasks::test_executor::Pool;

        /// An engine whose first decode waits at a gate, so a second load can overtake it.
        struct Slow {
            inner: FakeEngine,
            gate: Gate,
            first: Cell<bool>,
        }
        impl AudioEngine for Slow {
            type Source = <FakeEngine as AudioEngine>::Source;
            async fn decode_file(&self, file: &FileRef) -> Result<Arc<DecodedAudio>, DecodeError> {
                let slow = self.first.replace(false);
                if slow {
                    self.gate.wait().await;
                    return Ok(fake_audio(99.0));
                }
                self.inner.decode_file(file).await
            }
            fn create_source(&self, audio: &Arc<DecodedAudio>, on_ended: Option<crate::audio_engine::OnEnded>) -> Self::Source {
                self.inner.create_source(audio, on_ended)
            }
            fn schedule_start(&self, source: &Self::Source, when: f64, offset: f64) -> Result<(), EngineError> {
                self.inner.schedule_start(source, when, offset)
            }
            fn now(&self) -> f64 {
                self.inner.now()
            }
        }

        let gate = Gate::new();
        let engine = Rc::new(Slow { inner: FakeEngine::default(), gate: gate.clone(), first: Cell::new(true) });
        let player = Rc::new(TrackPlayer::new(engine, TrackPlayerCallbacks::default()));
        let pool = Pool::new();
        let (p1, p2) = (player.clone(), player.clone());
        let slow = pool.spawn(async move { p1.load(&file()).await });
        pool.run_until_stalled();
        let fast = pool.spawn(async move { p2.load(&file()).await });
        pool.run_until_stalled();
        assert!(fast.is_done() && !slow.is_done());
        player.play();
        gate.open();
        pool.run_until_stalled();
        assert!(slow.is_done());
        assert_eq!(player.get_state(), state(TrackPlayerStatus::Playing, true, 0.0, 10.0), "the superseded 99 s decode did not overwrite anything");
    }

    #[test]
    fn set_gain_applies_immediately_and_carries_over_to_later_sources() {
        let (engine, player) = setup();
        player.set_volume(1.0);
        player.play();
        assert_eq!(engine.gain_of("source-0"), Some(1.0), "defaults to a gain of 1");
        player.set_gain(0.5);
        assert_eq!(engine.gain_of("source-0"), Some(0.5));
        player.seek(2.0);
        assert_eq!(engine.gain_of("source-1"), Some(0.5));
    }

    #[test]
    fn set_volume_multiplies_with_set_gain_is_applied_immediately_carries_over_and_clamps() {
        let (engine, player) = setup();
        player.set_gain(0.5);
        player.play();
        player.set_volume(0.4);
        assert_eq!(engine.gain_of("source-0"), Some(0.2));
        player.seek(1.0);
        assert_eq!(engine.gain_of("source-1"), Some(0.2));
        player.set_volume(7.0);
        assert_eq!(player.get_volume(), 1.0);
        player.set_volume(-3.0);
        assert_eq!(player.get_volume(), 0.0);
    }

    #[test]
    fn load_decoded_makes_a_buffer_playable_without_a_decode_and_stops_whatever_was_playing() {
        let engine = Rc::new(FakeEngine::default());
        let player = TrackPlayer::new(engine.clone(), TrackPlayerCallbacks::default());
        player.load_decoded(fake_audio(7.0));
        assert_eq!(player.get_state(), state(TrackPlayerStatus::Stopped, false, 0.0, 7.0));
        assert_eq!(engine.0.decode_call_count.get(), 0);
        player.play();
        player.load_decoded(fake_audio(5.0));
        assert_eq!(engine.stopped(), ["source-0"]);
        assert_eq!(player.get_state().duration_seconds, 5.0);
    }

    mod crossfade_to {
        use super::*;

        fn plan() -> TransitionPlan {
            TransitionPlan { fade_start_seconds: 5.0, incoming_start_seconds: 1.0, fade_duration_seconds: 3.0 }
        }

        #[test]
        fn is_a_no_op_if_nothing_is_currently_playing() {
            let (engine, player) = setup();
            assert_eq!(player.crossfade_to(fake_audio(20.0), &plan(), 1.0), Ok(false));
            assert!(engine.starts().is_empty());
        }

        #[test]
        fn starts_the_fade_immediately_with_equal_power_curves_and_a_silent_incoming_source_at_rate_one() {
            let (engine, player) = setup();
            player.set_volume(1.0);
            player.play();
            engine.set_clock(5.0);
            let before = engine.starts().len();
            assert_eq!(player.crossfade_to(fake_audio(20.0), &plan(), 0.8), Ok(true));

            assert!(engine.0.rate_ramps_by_source_id.borrow().get("source-0").is_none(), "no rate change this round");
            let curves = engine.0.gain_curves_by_source_id.borrow();
            let outgoing = &curves["source-0"][0];
            assert_eq!((outgoing.at_time_seconds, outgoing.duration_seconds), (5.0, 3.0));
            assert!((outgoing.values[0] - 1.0).abs() < 1e-6 && outgoing.values.last().unwrap().abs() < 1e-6);
            assert!((engine.0.stop_when_by_source_id.borrow()["source-0"].unwrap() - (5.0 + 3.0 + 0.2)).abs() < 1e-9);

            let new_starts = &engine.starts()[before..];
            assert_eq!(new_starts.len(), 1);
            assert_eq!((new_starts[0].source_id.as_str(), new_starts[0].when_seconds, new_starts[0].offset_seconds), ("source-1", 5.0, 1.0));
            assert_eq!(engine.gain_of("source-1"), Some(0.0));
            assert!(engine.0.set_rate_calls_by_source_id.borrow().get("source-1").is_none());
            let incoming = &curves["source-1"][0];
            assert!((incoming.values.last().unwrap() - 0.8).abs() < 1e-6);
        }

        #[test]
        fn scales_the_outgoing_curve_by_its_own_current_gain() {
            let (engine, player) = setup();
            player.set_volume(1.0);
            player.play();
            player.set_gain(0.57);
            engine.set_clock(5.0);
            player.crossfade_to(fake_audio(20.0), &plan(), 1.0).unwrap();
            assert!((engine.0.gain_curves_by_source_id.borrow()["source-0"][0].values[0] - 0.57).abs() < 1e-6);
        }

        #[test]
        fn keeps_reporting_the_outgoing_track_until_its_stop_fires_then_swaps_and_reports_completion_not_an_end() {
            let engine = Rc::new(FakeEngine::default());
            let (ended, completed) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
            let (e, c) = (ended.clone(), completed.clone());
            let player = TrackPlayer::new(engine.clone(), TrackPlayerCallbacks { on_ended: Some(Rc::new(move || e.set(e.get() + 1))), on_crossfade_completed: Some(Rc::new(move || c.set(c.get() + 1))) });
            block_on(player.load(&file())).unwrap();
            player.play();
            engine.set_clock(5.0);
            player.crossfade_to(fake_audio(20.0), &plan(), 0.8).unwrap();

            engine.set_clock(6.0);
            assert_eq!(
                player.get_state(),
                TrackPlayerState { status: TrackPlayerStatus::Playing, is_audible: true, position_seconds: 6.0, duration_seconds: 10.0, pending_incoming: Some(PendingIncoming { position_seconds: 2.0, duration_seconds: 20.0, fade_duration_seconds: 3.0 }) }
            );
            assert_eq!((ended.get(), completed.get()), (0, 0));

            engine.fire_ended("source-0"); // the outgoing source's scheduled stop
            assert_eq!((ended.get(), completed.get()), (0, 1), "not a natural end");
            assert_eq!(player.get_state(), state(TrackPlayerStatus::Playing, true, 2.0, 20.0));
            engine.set_clock(8.0);
            assert!((player.get_state().position_seconds - 4.0).abs() < 1e-9);
        }

        #[test]
        fn cleans_up_the_incoming_source_and_reports_the_error_if_a_scheduling_call_fails_partway() {
            let (engine, player) = setup();
            player.play();
            engine.set_clock(5.0);
            engine.0.fail_incoming_ramp_gain_curve.set(true);
            let error = player.crossfade_to(fake_audio(20.0), &plan(), 1.0).unwrap_err();
            assert!(error.0.contains("conflicts with an existing curve event"));
            assert!(engine.stopped().contains(&"source-1".to_string()), "not left dangling");
            // No pending crossfade was left behind: source-0's real natural end is a real end, not a swap.
            engine.fire_ended("source-0");
            assert_eq!(player.get_state().status, TrackPlayerStatus::Stopped);
        }

        #[test]
        fn an_explicit_stop_before_it_completes_cancels_the_incoming_source_too() {
            let (engine, player) = setup();
            player.play();
            engine.set_clock(5.0);
            player.crossfade_to(fake_audio(20.0), &plan(), 1.0).unwrap();
            player.stop();
            assert!(engine.stopped().contains(&"source-1".to_string()));
            assert_eq!(player.get_state().status, TrackPlayerStatus::Stopped);
            engine.fire_ended("source-0");
            assert_eq!(player.get_state().status, TrackPlayerStatus::Stopped);
        }

        #[test]
        fn a_second_request_cancels_the_still_pending_first_rather_than_layering_two_on_one_source() {
            let (engine, player) = setup();
            player.play();
            engine.set_clock(5.0);
            player.crossfade_to(fake_audio(20.0), &plan(), 1.0).unwrap();
            player.crossfade_to(fake_audio(15.0), &plan(), 1.0).unwrap();
            assert!(engine.stopped().contains(&"source-1".to_string()), "the first, now-stale, incoming source");
            assert_eq!(player.get_state().pending_incoming.map(|p| p.duration_seconds), Some(15.0));
            engine.fire_ended("source-0");
            assert_eq!(player.get_state().duration_seconds, 15.0);
        }
    }
}
