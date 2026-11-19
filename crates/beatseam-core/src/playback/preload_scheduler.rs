use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::audio::DecodedAudio;
use crate::tasks::Runtime;

/// Decodes a track by file id - the playlist player wraps its track resolution plus the engine's decode into this.
pub type DecodeFn = Arc<dyn Fn(&str) -> Pin<Box<dyn Future<Output = Result<Arc<DecodedAudio>, String>>>>>;

pub struct PreloadTickInfo {
    /// Seconds remaining in the currently playing track.
    pub remaining_seconds: f64,
    /// The next N tracks' file ids, nearest first - the scheduler's lookahead window for this tick.
    pub upcoming_file_ids: Vec<String>,
}

pub struct PreloadSchedulerOptions {
    pub decode: DecodeFn,
    /// Called once the nearest upcoming track's preload has exhausted every retry threshold - "flag a playback error".
    /// Playback itself is unaffected: starting that track just decodes cold (its existing, already-correct fallback) when
    /// it gets there, since an empty preload cache is a no-op, not a special case.
    pub on_give_up: Option<Arc<dyn Fn(&str)>>,
    /// Seconds-remaining thresholds at which the nearest upcoming track is (re)attempted, in order.
    pub retry_thresholds_seconds: Vec<f64>,
}

impl PreloadSchedulerOptions {
    pub fn new(decode: DecodeFn) -> Self {
        Self { decode, on_give_up: None, retry_thresholds_seconds: DEFAULT_RETRY_THRESHOLDS_SECONDS.to_vec() }
    }
}

pub const DEFAULT_RETRY_THRESHOLDS_SECONDS: [f64; 4] = [60.0, 50.0, 40.0, 35.0];
/// How long to wait before retrying a failed deeper (non-nearest) slot - these aren't time-critical, so no need for the
/// timed retry sequence.
const DEEP_SLOT_RETRY_COOLDOWN_MS: u64 = 5000;

#[derive(Default)]
struct State {
    ready: HashMap<String, Arc<DecodedAudio>>,
    loading: HashSet<String>,
    deep_next_attempt_at_ms: HashMap<String, u64>,
    last_nearest: Option<String>,
    nearest_threshold_index: usize,
    nearest_gave_up_for: Option<String>,
    nearest_attempted: bool,
}

/// Decides when to start decoding upcoming tracks ahead of when they're needed. The *nearest* upcoming track is attempted
/// eagerly the moment it takes that slot (same as the deeper slots below) - a manual skip can reach it at any point during
/// the current track, not just near the end, so there's no safe time to defer that first attempt to. If that attempt fails,
/// retries follow an explicit backoff sequence (remaining seconds crossing each threshold in turn, e.g. 60/50/40/35 s
/// remaining) rather than hammering it repeatedly; once every threshold is exhausted, `on_give_up` fires and that track is
/// left to load cold when actually reached. Deeper lookahead slots (2nd, 3rd track ahead) aren't as urgent, so they're
/// attempted eagerly as soon as they enter the lookahead window and retried on a fixed cooldown on failure.
///
/// At most one decode is ever in flight at a time: every lookahead slot going eager at once meant every track change fired
/// 2-3 concurrent decodes, each one real, substantial work (a multi-MB file read + decode + a database analysis read/write),
/// which was measured on-device as compounding contention that made each individual decode take multiple seconds longer
/// than it does alone. Serializing them keeps each one fast and the rest of the app uncontended, at the cost of the
/// lookahead window filling in slightly slower - an easy trade since none of this is on the critical path once the nearest
/// slot itself is ready.
pub struct PreloadScheduler<R> {
    decode: DecodeFn,
    on_give_up: Option<Arc<dyn Fn(&str)>>,
    retry_thresholds_seconds: Vec<f64>,
    runtime: Arc<R>,
    state: Arc<Mutex<State>>,
}

impl<R: Runtime + 'static> PreloadScheduler<R> {
    pub fn new(runtime: Arc<R>, options: PreloadSchedulerOptions) -> Self {
        Self { decode: options.decode, on_give_up: options.on_give_up, retry_thresholds_seconds: options.retry_thresholds_seconds, runtime, state: Arc::new(Mutex::new(State::default())) }
    }

    pub fn tick(&self, info: &PreloadTickInfo) {
        self.forget_stale(&info.upcoming_file_ids);
        let Some((nearest, deeper)) = info.upcoming_file_ids.split_first() else { return };
        self.tick_nearest(nearest, info.remaining_seconds);
        for file_id in deeper {
            self.tick_deep(file_id);
        }
    }

    /// Returns (and removes) a ready preloaded buffer for `file_id`, if there is one.
    pub fn take_preloaded(&self, file_id: &str) -> Option<Arc<DecodedAudio>> {
        self.state.lock().unwrap().ready.remove(file_id)
    }

    /// Checks readiness without consuming it - used to decide *whether* to start a crossfade before committing to it via
    /// [`take_preloaded`](Self::take_preloaded).
    pub fn has_preloaded(&self, file_id: &str) -> bool {
        self.state.lock().unwrap().ready.contains_key(file_id)
    }

    /// Drops any preloaded/in-progress state for tracks no longer in the current lookahead window, bounding memory to what's
    /// actually still relevant.
    fn forget_stale(&self, upcoming: &[String]) {
        let keep: HashSet<&str> = upcoming.iter().map(String::as_str).collect();
        let mut state = self.state.lock().unwrap();
        state.ready.retain(|id, _| keep.contains(id.as_str()));
        state.deep_next_attempt_at_ms.retain(|id, _| keep.contains(id.as_str()));
    }

    /// The nearest slot is attempted immediately, the moment a file id takes it - NOT gated on the remaining time crossing the
    /// first threshold. That gate made sense for the natural end-of-track crossfade (there's no rush), but a manual skip can
    /// reach this track at any point during the *current* track, at which point an ungated nearest slot is the only thing
    /// standing between the person and a fully cold decode blocking the UI for multiple seconds right in the middle of the
    /// switch animation. The threshold schedule still governs *retries* after a failed attempt.
    fn tick_nearest(&self, file_id: &str, remaining_seconds: f64) {
        {
            let mut state = self.state.lock().unwrap();
            // A different track has taken the nearest slot (natural advance, manual skip, or a shuffle re-order): that track's
            // retry history doesn't apply to this one.
            if state.last_nearest.as_deref() != Some(file_id) {
                state.last_nearest = Some(file_id.to_owned());
                state.nearest_threshold_index = 0;
                state.nearest_gave_up_for = None;
                state.nearest_attempted = false;
            }
            if state.ready.contains_key(file_id) || state.loading.contains(file_id) || state.nearest_gave_up_for.as_deref() == Some(file_id) {
                return;
            }
            // Only one scheduler-initiated decode runs at a time; a different slot's decode already in flight means this one
            // just waits for the next tick.
            if !state.loading.is_empty() {
                return;
            }
            if !state.nearest_attempted {
                state.nearest_attempted = true;
            } else {
                match self.retry_thresholds_seconds.get(state.nearest_threshold_index) {
                    Some(threshold) if remaining_seconds <= *threshold => {}
                    _ => return,
                }
            }
            state.loading.insert(file_id.to_owned());
        }
        self.start(file_id, true);
    }

    fn tick_deep(&self, file_id: &str) {
        {
            let mut state = self.state.lock().unwrap();
            if state.ready.contains_key(file_id) || state.loading.contains(file_id) || !state.loading.is_empty() {
                return;
            }
            if self.runtime.now_ms() < state.deep_next_attempt_at_ms.get(file_id).copied().unwrap_or(0) {
                return;
            }
            state.loading.insert(file_id.to_owned());
        }
        self.start(file_id, false);
    }

    /// Begins a decode (the call itself happens now; the wait for it happens in the background).
    fn start(&self, file_id: &str, nearest: bool) {
        let future = (self.decode)(file_id);
        let (state, runtime, give_up, thresholds, id) = (self.state.clone(), self.runtime.clone(), self.on_give_up.clone(), self.retry_thresholds_seconds.len(), file_id.to_owned());
        self.runtime.spawn(Box::pin(async move {
            let result = future.await;
            let mut gave_up = false;
            {
                let mut state = state.lock().unwrap();
                match result {
                    Ok(decoded) => {
                        state.ready.insert(id.clone(), decoded);
                    }
                    Err(_) if nearest => {
                        state.nearest_threshold_index += 1;
                        if state.nearest_threshold_index >= thresholds {
                            state.nearest_gave_up_for = Some(id.clone());
                            gave_up = true;
                        }
                    }
                    Err(_) => {
                        state.deep_next_attempt_at_ms.insert(id.clone(), runtime.now_ms() + DEEP_SLOT_RETRY_COOLDOWN_MS);
                    }
                }
                state.loading.remove(&id);
            }
            if gave_up
                && let Some(give_up) = give_up {
                    give_up(&id);
                }
        }));
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;

    use super::*;
    use crate::tasks::Gate;
    use crate::tasks::test_executor::Pool;

    #[derive(Clone)]
    struct TestRuntime {
        pool: Rc<Pool>,
        clock: Rc<Cell<u64>>,
    }

    impl Runtime for TestRuntime {
        fn spawn(&self, future: Pin<Box<dyn Future<Output = ()>>>) {
            self.pool.spawn(future);
        }
        fn sleep(&self, _millis: u64) -> Pin<Box<dyn Future<Output = ()>>> {
            Box::pin(async {})
        }
        fn now_ms(&self) -> u64 {
            self.clock.get()
        }
    }

    type Slot = (Gate, Rc<RefCell<Option<Result<Arc<DecodedAudio>, String>>>>);

    /// A controllable decode so tests can resolve/reject specific calls in a specific order.
    #[derive(Clone, Default)]
    struct ControllableDecoder {
        calls: Rc<RefCell<Vec<String>>>,
        pending: Rc<RefCell<VecDeque<Slot>>>,
    }

    impl ControllableDecoder {
        fn decode_fn(&self) -> DecodeFn {
            let this = self.clone();
            Arc::new(move |file_id: &str| {
                this.calls.borrow_mut().push(file_id.to_owned());
                let (gate, slot): Slot = (Gate::new(), Rc::default());
                this.pending.borrow_mut().push_back((gate.clone(), slot.clone()));
                Box::pin(async move {
                    gate.wait().await;
                    slot.borrow_mut().take().expect("settled")
                })
            })
        }

        fn settle_next(&self, result: Result<Arc<DecodedAudio>, String>) {
            let (gate, slot) = self.pending.borrow_mut().pop_front().expect("a decode in flight");
            *slot.borrow_mut() = Some(result);
            gate.open();
        }

        fn resolve_next(&self) {
            self.settle_next(Ok(Arc::new(DecodedAudio { sample_rate: 44100, channel_data: vec![] })));
        }

        fn reject_next(&self) {
            self.settle_next(Err("decode failed".into()));
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    struct Fixture {
        pool: Rc<Pool>,
        clock: Rc<Cell<u64>>,
        decoder: ControllableDecoder,
        gave_up: Rc<RefCell<Vec<String>>>,
        scheduler: PreloadScheduler<TestRuntime>,
    }

    fn fixture() -> Fixture {
        let pool = Rc::new(Pool::new());
        let clock = Rc::new(Cell::new(1_000));
        let decoder = ControllableDecoder::default();
        let gave_up: Rc<RefCell<Vec<String>>> = Rc::default();
        let g = gave_up.clone();
        let mut options = PreloadSchedulerOptions::new(decoder.decode_fn());
        options.on_give_up = Some(Arc::new(move |id: &str| g.borrow_mut().push(id.to_owned())));
        let scheduler = PreloadScheduler::new(Arc::new(TestRuntime { pool: pool.clone(), clock: clock.clone() }), options);
        Fixture { pool, clock, decoder, gave_up, scheduler }
    }

    fn tick(f: &Fixture, remaining: f64, ids: &[&str]) {
        f.scheduler.tick(&PreloadTickInfo { remaining_seconds: remaining, upcoming_file_ids: ids.iter().map(|s| s.to_string()).collect() });
    }

    fn calls(f: &Fixture) -> Vec<String> {
        f.decoder.calls()
    }

    #[test]
    fn attempts_the_nearest_track_immediately_not_gated_by_remaining_time() {
        let f = fixture();
        // Plenty of time left on the current track - a manual skip could still reach this track at any moment.
        tick(&f, 200.0, &["a"]);
        assert_eq!(calls(&f), ["a"]);
    }

    #[test]
    fn retries_at_each_subsequent_threshold_after_a_failure_then_gives_up() {
        let f = fixture();
        tick(&f, 200.0, &["a"]);
        assert_eq!(calls(&f), ["a"]);
        f.decoder.reject_next();
        f.pool.run_until_stalled();

        for (remaining, expected_calls) in [(50.0, 2), (40.0, 3), (35.0, 4)] {
            tick(&f, remaining, &["a"]);
            assert_eq!(calls(&f).len(), expected_calls, "at {remaining}s remaining");
            if expected_calls < 4 {
                f.decoder.reject_next();
                f.pool.run_until_stalled();
            }
        }
        assert!(f.gave_up.borrow().is_empty());
        f.decoder.reject_next();
        f.pool.run_until_stalled();
        assert_eq!(*f.gave_up.borrow(), ["a"]);

        // No further attempts once given up, even at a later (lower) threshold check.
        tick(&f, 10.0, &["a"]);
        assert_eq!(calls(&f).len(), 4);
    }

    #[test]
    fn a_retry_waits_for_its_threshold() {
        let f = fixture();
        tick(&f, 200.0, &["a"]);
        f.decoder.reject_next();
        f.pool.run_until_stalled();
        tick(&f, 51.0, &["a"]);
        assert_eq!(calls(&f).len(), 1, "the failed first attempt used up the 60 s slot, so the next is 50 s");
        tick(&f, 50.0, &["a"]);
        assert_eq!(calls(&f).len(), 2);
    }

    #[test]
    fn makes_the_decoded_buffer_available_once_it_succeeds_and_only_once() {
        let f = fixture();
        tick(&f, 60.0, &["a"]);
        assert!(f.scheduler.take_preloaded("a").is_none());
        f.decoder.resolve_next();
        f.pool.run_until_stalled();
        assert!(f.scheduler.has_preloaded("a"));
        assert!(f.scheduler.take_preloaded("a").is_some());
        assert!(f.scheduler.take_preloaded("a").is_none(), "consumed");
    }

    #[test]
    fn does_not_start_a_second_concurrent_decode_for_a_track_already_in_flight() {
        let f = fixture();
        tick(&f, 60.0, &["a"]);
        tick(&f, 55.0, &["a"]);
        assert_eq!(calls(&f), ["a"]);
    }

    #[test]
    fn resets_retry_progress_when_a_different_track_takes_the_nearest_slot() {
        let f = fixture();
        tick(&f, 200.0, &["a"]);
        f.decoder.reject_next();
        f.pool.run_until_stalled();
        // A different track becomes nearest (a manual skip): attempted immediately, same as 'a' was.
        tick(&f, 200.0, &["b"]);
        assert_eq!(calls(&f), ["a", "b"]);
    }

    #[test]
    fn eagerly_attempts_deeper_slots_but_only_once_the_nearest_slot_is_settled() {
        let f = fixture();
        tick(&f, 200.0, &["a", "b"]);
        assert_eq!(calls(&f), ["a"], "only one decode runs at a time");
        f.decoder.resolve_next();
        f.pool.run_until_stalled();
        tick(&f, 200.0, &["a", "b"]);
        assert_eq!(calls(&f), ["a", "b"]);
    }

    #[test]
    fn retries_a_failed_deep_slot_after_its_cooldown_not_on_every_tick() {
        let f = fixture();
        tick(&f, 200.0, &["a", "b"]);
        f.decoder.resolve_next();
        f.pool.run_until_stalled();
        tick(&f, 200.0, &["a", "b"]);
        assert_eq!(calls(&f), ["a", "b"]);
        f.decoder.reject_next(); // 'b' (deep) fails
        f.pool.run_until_stalled();

        tick(&f, 200.0, &["a", "b"]);
        assert_eq!(calls(&f), ["a", "b"], "'b' is still within its cooldown, 'a' is already ready");
        f.clock.set(f.clock.get() + 6_000);
        tick(&f, 200.0, &["a", "b"]);
        assert_eq!(calls(&f), ["a", "b", "b"]);
    }

    #[test]
    fn drops_preloaded_state_for_tracks_that_fall_out_of_the_lookahead_window() {
        let f = fixture();
        tick(&f, 200.0, &["a", "b"]);
        f.decoder.resolve_next(); // 'a'
        f.pool.run_until_stalled();
        tick(&f, 200.0, &["a", "b"]);
        f.decoder.resolve_next(); // 'b'
        f.pool.run_until_stalled();
        assert!(f.scheduler.has_preloaded("b"));

        tick(&f, 200.0, &["a", "c"]);
        assert!(f.scheduler.take_preloaded("b").is_none());
    }

    #[test]
    fn an_empty_window_does_nothing() {
        let f = fixture();
        tick(&f, 10.0, &[]);
        assert!(calls(&f).is_empty());
    }
}
