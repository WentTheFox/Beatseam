//! Port of `store/playback/playbackTicker`: polls the player while something is moving.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use beatseam_core::audio_engine::AudioEngine;
use beatseam_core::playback::{PlaylistPlayer, PlaylistPlayerState, TrackPlayerStatus};
use beatseam_core::tasks::Runtime;

use crate::live_playback::LivePlayback;

/// How often playback state is re-read while something is active.
pub const PLAYBACK_TICK_MS: u64 = 200;
/// How often a platform's own timer drives the ticker while the app is in the background (see [`BackgroundTicks`]).
pub const BACKGROUND_TICK_MS: u64 = 500;

/// Whether the player has anything that moves on its own: a playing track, a decode in flight (which finishes asynchronously, with no
/// event to hear it by), or a crossfade. Paused/idle state only changes through an explicit command, and every command publishes -
/// so there's nothing to poll for.
pub fn is_playback_active(state: &PlaylistPlayerState) -> bool {
    matches!(state.track.status, TrackPlayerStatus::Playing | TrackPlayerStatus::Loading) || state.track.pending_incoming.is_some()
}

/// For platforms whose timers stop running while the app is in the background (Android): a timer that doesn't, to keep playback
/// state, preloading and everything driven by the ticker moving then.
pub trait BackgroundTicks {
    /// Whether ordinary timers can't be relied on right now.
    fn is_background(&self) -> bool;
    /// Calls back whenever that changes; returns the unsubscribe function.
    fn subscribe(&self, callback: Rc<dyn Fn(bool)>) -> Box<dyn FnOnce()>;
    /// Calls `callback` about every `interval_ms` from a timer that survives the background, until the returned function is called.
    fn start(&self, interval_ms: u64, callback: Rc<dyn Fn()>) -> Box<dyn FnOnce()>;
}

pub struct PlaybackTickerOptions<E: AudioEngine + 'static, R: Runtime + 'static> {
    pub live: Rc<LivePlayback>,
    pub player: PlaylistPlayer<E, R>,
    pub runtime: Arc<R>,
    /// Runs after each tick's publish, with the state it published (e.g. throttled position persistence).
    pub on_tick: Option<Rc<dyn Fn(&PlaylistPlayerState)>>,
    pub interval_ms: u64,
    /// Drives the ticker while the app is in the background, where ordinary timers are paused. Omit where timers just work.
    pub background: Option<Rc<dyn BackgroundTicks>>,
}

struct Inner<E: AudioEngine + 'static, R: Runtime + 'static> {
    options: PlaybackTickerOptions<E, R>,
    /// Bumped whenever the foreground timer is cancelled or replaced, so a sleeping loop knows it is stale.
    epoch: Cell<u64>,
    foreground_running: Cell<bool>,
    stop_background: RefCell<Option<Box<dyn FnOnce()>>>,
    subscriptions: RefCell<Vec<Box<dyn FnOnce()>>>,
}

/// The player has no event stream for position, so this polls it - but only while it's active (see [`is_playback_active`]), starting
/// whenever a published state is active and stopping on the first tick that finds it isn't. Each tick publishes the fresh state,
/// drives the preload/crossfade scheduler (which reuses this tick rather than running a second timer), then reports. A service, not
/// a screen effect: nothing in a UI owns it. [`stop`](Self::stop) ends it.
pub struct PlaybackTicker<E: AudioEngine + 'static, R: Runtime + 'static> {
    inner: Rc<Inner<E, R>>,
}

impl<E: AudioEngine + 'static, R: Runtime + 'static> Clone for PlaybackTicker<E, R> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

impl<E: AudioEngine + 'static, R: Runtime + 'static> PlaybackTicker<E, R> {
    pub fn start(options: PlaybackTickerOptions<E, R>) -> Self {
        let ticker = Self { inner: Rc::new(Inner { options, epoch: Cell::new(0), foreground_running: Cell::new(false), stop_background: RefCell::new(None), subscriptions: RefCell::new(Vec::new()) }) };
        let (live, background) = (ticker.inner.options.live.clone(), ticker.inner.options.background.clone());

        let this = ticker.clone();
        let id = live.subscribe(move |state| this.ensure_running(state));
        let unsubscribe_live = {
            let live = live.clone();
            Box::new(move || live.unsubscribe(id)) as Box<dyn FnOnce()>
        };
        ticker.inner.subscriptions.borrow_mut().push(unsubscribe_live);

        // Changing between foreground and background swaps the timer, and catches up at once on whatever was missed.
        if let Some(background) = &background {
            let this = ticker.clone();
            let unsubscribe = background.subscribe(Rc::new(move |_| {
                this.stop_timers();
                let snapshot = this.inner.options.live.snapshot();
                if is_playback_active(&snapshot) {
                    this.tick();
                    this.ensure_running(&this.inner.options.live.snapshot());
                }
            }));
            ticker.inner.subscriptions.borrow_mut().push(unsubscribe);
        }
        ticker.ensure_running(&live.snapshot());
        ticker
    }

    pub fn stop(&self) {
        for unsubscribe in self.inner.subscriptions.borrow_mut().drain(..) {
            unsubscribe();
        }
        self.stop_timers();
    }

    fn stop_timers(&self) {
        self.inner.epoch.set(self.inner.epoch.get() + 1);
        self.inner.foreground_running.set(false);
        let stop = self.inner.stop_background.borrow_mut().take();
        if let Some(stop) = stop {
            stop();
        }
    }

    fn tick(&self) {
        let options = &self.inner.options;
        let state = options.live.publish(options.player.get_state());
        options.player.check_preload();
        if let Some(on_tick) = &options.on_tick {
            on_tick(&state);
        }
        if !is_playback_active(&state) {
            self.stop_timers();
        }
    }

    /// One timer at a time: an ordinary one in the foreground, the platform's own while backgrounded.
    fn ensure_running(&self, state: &PlaylistPlayerState) {
        if !is_playback_active(state) {
            return;
        }
        let options = &self.inner.options;
        if options.background.as_ref().is_some_and(|b| b.is_background()) {
            if self.inner.stop_background.borrow().is_none() {
                self.inner.epoch.set(self.inner.epoch.get() + 1); // ends a foreground loop that was running
                self.inner.foreground_running.set(false);
                let this = self.clone();
                let background = options.background.clone().expect("checked above");
                *self.inner.stop_background.borrow_mut() = Some(background.start(BACKGROUND_TICK_MS, Rc::new(move || this.tick())));
            }
        } else if !self.inner.foreground_running.get() {
            self.inner.foreground_running.set(true);
            let epoch = self.inner.epoch.get() + 1;
            self.inner.epoch.set(epoch);
            let this = self.clone();
            options.runtime.spawn(Box::pin(async move {
                loop {
                    this.inner.options.runtime.sleep(this.inner.options.interval_ms).await;
                    if this.inner.epoch.get() != epoch {
                        return;
                    }
                    this.tick();
                    // The tick may have stopped the ticker: don't schedule one more sleep just to find that out.
                    if this.inner.epoch.get() != epoch {
                        return;
                    }
                }
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;

    use beatseam_core::audio_engine::fake::FakeEngine;
    use beatseam_core::file_access::FileRef;
    use beatseam_core::library_store::LoopMode;
    use beatseam_core::playback::{PlaylistOptions, PlaylistPlayerOptions, ResolveTrack, TrackPlayerState};
    use beatseam_core::tasks::Gate;
    use beatseam_core::tasks::test_executor::Pool;

    use super::*;

    #[derive(Clone)]
    struct TestRuntime {
        pool: Rc<Pool>,
        sleeps: Rc<RefCell<Vec<(u64, Gate)>>>,
    }

    impl Runtime for TestRuntime {
        fn spawn(&self, future: Pin<Box<dyn Future<Output = ()>>>) {
            self.pool.spawn(future);
        }
        fn sleep(&self, millis: u64) -> Pin<Box<dyn Future<Output = ()>>> {
            let gate = Gate::new();
            self.sleeps.borrow_mut().push((millis, gate.clone()));
            Box::pin(async move { gate.wait().await })
        }
        fn now_ms(&self) -> u64 {
            0
        }
    }

    struct Fixture {
        pool: Rc<Pool>,
        runtime: TestRuntime,
        engine: Rc<FakeEngine>,
        player: PlaylistPlayer<FakeEngine, TestRuntime>,
        live: Rc<LivePlayback>,
        ticks: Rc<Cell<usize>>,
    }

    fn fixture() -> Fixture {
        let pool = Rc::new(Pool::new());
        let runtime = TestRuntime { pool: pool.clone(), sleeps: Rc::default() };
        let engine = Rc::new(FakeEngine::default());
        let resolve: ResolveTrack = Rc::new(|id: &str| {
            let id = id.to_owned();
            Box::pin(async move { Ok(FileRef { id: id.clone(), name: id.clone(), relative_path: id, size_bytes: 1, last_modified_ms: 0.0 }) })
        });
        let player = PlaylistPlayer::new(engine.clone(), Arc::new(runtime.clone()), resolve, PlaylistPlayerOptions { random: Some(Rc::new(|| 0.5)), ..Default::default() });
        let live = Rc::new(LivePlayback::new(player.get_state()));
        Fixture { pool, runtime, engine, player, live, ticks: Rc::default() }
    }

    impl Fixture {
        fn start(&self, background: Option<Rc<dyn BackgroundTicks>>) -> PlaybackTicker<FakeEngine, TestRuntime> {
            let ticks = self.ticks.clone();
            PlaybackTicker::start(PlaybackTickerOptions { live: self.live.clone(), player: self.player.clone(), runtime: Arc::new(self.runtime.clone()), on_tick: Some(Rc::new(move |_| ticks.set(ticks.get() + 1))), interval_ms: PLAYBACK_TICK_MS, background })
        }

        fn play(&self, ids: &[&str]) {
            let p = self.player.clone();
            let ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
            self.pool.spawn(async move { p.set_playlist(ids, None, PlaylistOptions::default()).await });
            self.pool.run_until_stalled();
            self.live.publish(self.player.get_state());
            self.pool.run_until_stalled();
        }

        /// Fires the pending foreground timer, if any, and lets what it started run.
        fn fire_timer(&self) -> bool {
            let next = {
                let mut sleeps = self.runtime.sleeps.borrow_mut();
                if sleeps.is_empty() { None } else { Some(sleeps.remove(0)) }
            };
            let Some((_, gate)) = next else { return false };
            gate.open();
            self.pool.run_until_stalled();
            true
        }
    }

    fn idle_state() -> PlaylistPlayerState {
        PlaylistPlayerState {
            total_tracks: 0,
            position: -1,
            current_file_id: None,
            loop_mode: LoopMode::Off,
            shuffle_enabled: false,
            track: TrackPlayerState { status: TrackPlayerStatus::Idle, is_audible: false, position_seconds: 0.0, duration_seconds: 0.0, pending_incoming: None },
            pending_crossfade_file_ids: None,
            is_loading_for_playback: false,
        }
    }

    #[test]
    fn only_a_playing_loading_or_crossfading_player_counts_as_active() {
        let mut state = idle_state();
        assert!(!is_playback_active(&state));
        for (status, active) in [(TrackPlayerStatus::Playing, true), (TrackPlayerStatus::Loading, true), (TrackPlayerStatus::Paused, false), (TrackPlayerStatus::Stopped, false)] {
            state.track.status = status;
            assert_eq!(is_playback_active(&state), active, "{status:?}");
        }
        state.track.status = TrackPlayerStatus::Paused;
        state.track.pending_incoming = Some(beatseam_core::playback::PendingIncoming { position_seconds: 0.0, duration_seconds: 1.0, fade_duration_seconds: 1.0 });
        assert!(is_playback_active(&state));
    }

    #[test]
    fn it_does_nothing_while_idle_and_starts_polling_once_something_plays() {
        let f = fixture();
        let _ticker = f.start(None);
        f.pool.run_until_stalled();
        assert!(f.runtime.sleeps.borrow().is_empty(), "nothing to poll for");
        f.play(&["a", "b"]);
        assert_eq!(f.runtime.sleeps.borrow().len(), 1, "one timer, started by the first active publish");
        assert_eq!(f.runtime.sleeps.borrow()[0].0, 200);
    }

    #[test]
    fn each_tick_publishes_fresh_state_drives_the_preload_and_reports() {
        let f = fixture();
        let _ticker = f.start(None);
        f.play(&["a", "b", "c"]);
        f.engine.set_clock(3.0);
        assert!(f.fire_timer());
        assert_eq!(f.live.snapshot().track.position_seconds, 3.0, "the published state moved with the clock");
        assert_eq!(f.ticks.get(), 1);
        assert_eq!(f.engine.decode_count("b"), 1, "the tick ran the preload scheduler for the next track");
        assert_eq!(f.runtime.sleeps.borrow().len(), 1, "and the loop goes on");
    }

    #[test]
    fn it_stops_on_the_first_tick_that_finds_the_player_inactive_and_restarts_when_it_plays_again() {
        let f = fixture();
        let _ticker = f.start(None);
        f.play(&["a", "b"]);
        f.player.pause();
        f.fire_timer();
        assert_eq!(f.ticks.get(), 1);
        assert!(!f.fire_timer(), "no timer left to fire");
        f.player.play();
        f.live.publish(f.player.get_state());
        f.pool.run_until_stalled();
        assert_eq!(f.runtime.sleeps.borrow().len(), 1, "playing again starts polling again");
    }

    #[test]
    fn publishing_while_already_running_never_starts_a_second_timer() {
        let f = fixture();
        let _ticker = f.start(None);
        f.play(&["a"]);
        f.live.publish(f.player.get_state());
        f.live.publish(f.player.get_state());
        assert_eq!(f.runtime.sleeps.borrow().len(), 1);
    }

    #[test]
    fn stopping_ends_the_timer_and_the_subscription() {
        let f = fixture();
        let ticker = f.start(None);
        f.play(&["a"]);
        ticker.stop();
        f.fire_timer();
        assert_eq!(f.ticks.get(), 0, "the stale loop gave up instead of ticking");
        f.live.publish(f.player.get_state());
        assert!(f.runtime.sleeps.borrow().is_empty(), "no longer listening");
    }

    struct Background {
        background: Cell<bool>,
        callbacks: RefCell<Vec<Rc<dyn Fn(bool)>>>,
        timer: RefCell<Option<Rc<dyn Fn()>>>,
        stops: Rc<Cell<usize>>,
    }

    impl BackgroundTicks for Rc<Background> {
        fn is_background(&self) -> bool {
            self.background.get()
        }
        fn subscribe(&self, callback: Rc<dyn Fn(bool)>) -> Box<dyn FnOnce()> {
            self.callbacks.borrow_mut().push(callback);
            Box::new(|| {})
        }
        fn start(&self, interval_ms: u64, callback: Rc<dyn Fn()>) -> Box<dyn FnOnce()> {
            assert_eq!(interval_ms, 500);
            *self.timer.borrow_mut() = Some(callback);
            let (this, stops) = (self.clone(), self.stops.clone());
            Box::new(move || {
                this.timer.borrow_mut().take();
                stops.set(stops.get() + 1);
            })
        }
    }

    #[test]
    fn in_the_background_the_platforms_own_timer_drives_the_ticks_instead_of_the_foreground_one() {
        let f = fixture();
        let bg = Rc::new(Background { background: Cell::new(false), callbacks: RefCell::default(), timer: RefCell::default(), stops: Rc::default() });
        let _ticker = f.start(Some(Rc::new(bg.clone())));
        f.play(&["a", "b"]);
        assert_eq!(f.runtime.sleeps.borrow().len(), 1, "foreground timer while visible");

        // Going to the background swaps the timer and catches up at once.
        bg.background.set(true);
        f.engine.set_clock(2.0);
        let callbacks = bg.callbacks.borrow().clone();
        for callback in callbacks {
            callback(true);
        }
        f.pool.run_until_stalled();
        assert_eq!(f.live.snapshot().track.position_seconds, 2.0, "caught up immediately");
        assert!(bg.timer.borrow().is_some(), "the platform timer is running");
        f.fire_timer(); // the old foreground loop is stale and does not tick
        let ticks_before = f.ticks.get();
        let tick = bg.timer.borrow().clone().unwrap();
        tick();
        assert_eq!(f.ticks.get(), ticks_before + 1);

        // Back to the foreground: the platform timer stops and an ordinary one takes over.
        bg.background.set(false);
        let callbacks = bg.callbacks.borrow().clone();
        for callback in callbacks {
            callback(false);
        }
        f.pool.run_until_stalled();
        assert_eq!(bg.stops.get(), 1);
        assert_eq!(f.runtime.sleeps.borrow().len(), 1);
    }
}
