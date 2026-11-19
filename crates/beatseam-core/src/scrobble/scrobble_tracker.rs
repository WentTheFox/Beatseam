use super::lastfm::meets_scrobble_threshold;

#[derive(Clone, Debug, PartialEq)]
pub struct ScrobbleTrackInfo {
    pub file_id: String,
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration_seconds: f64,
}

/// What the tracker reports back. Both are called synchronously from [`ScrobbleTracker::update`].
pub struct ScrobbleTrackerCallbacks {
    /// Fired once per play instance, the first poll tick where it's actually audible.
    pub on_now_playing: Box<dyn FnMut(&ScrobbleTrackInfo)>,
    /// Fired once per play instance, the first poll tick that crosses the Last.fm scrobble threshold, with the unix
    /// second the play started.
    pub on_scrobble: Box<dyn FnMut(&ScrobbleTrackInfo, i64)>,
}

/// A position drop bigger than this (loop-one restarting, or the user replaying the same track from the start) is
/// treated as a new play instance rather than normal forward playback/a small seek-back.
const RESTART_DETECTION_SECONDS: f64 = 5.0;

/// Drives Last.fm's two calls (now-playing, then scrobble) off nothing but repeated `update()` polls carrying the
/// current playback snapshot - the same ~200 ms poll every caller already runs for the seek bar/persistence, not a
/// separate timer. Same file id is usually "still the same play", but loop-one/replaying-the-same-track-from-the-start
/// both keep the id unchanged while position visibly rewinds, so a backward jump past
/// [`RESTART_DETECTION_SECONDS`] is treated as a fresh play instance too - otherwise a looped track would only ever
/// scrobble once, no matter how many times it repeats.
pub struct ScrobbleTracker {
    callbacks: ScrobbleTrackerCallbacks,
    /// Wall clock in unix milliseconds (injected so tests and the app share one definition of "now").
    now_ms: Box<dyn Fn() -> i64>,
    current_file_id: Option<String>,
    last_position_seconds: f64,
    started_at_unix_seconds: i64,
    now_playing_sent: bool,
    scrobbled: bool,
}

impl ScrobbleTracker {
    pub fn new(callbacks: ScrobbleTrackerCallbacks, now_ms: impl Fn() -> i64 + 'static) -> Self {
        Self {
            callbacks,
            now_ms: Box::new(now_ms),
            current_file_id: None,
            last_position_seconds: 0.0,
            started_at_unix_seconds: 0,
            now_playing_sent: false,
            scrobbled: false,
        }
    }

    pub fn update(&mut self, info: Option<&ScrobbleTrackInfo>, is_playing: bool, position_seconds: f64) {
        let Some(info) = info else {
            self.current_file_id = None;
            return;
        };

        let is_new_instance = self.current_file_id.as_deref() != Some(info.file_id.as_str())
            || position_seconds < self.last_position_seconds - RESTART_DETECTION_SECONDS;
        if is_new_instance {
            self.current_file_id = Some(info.file_id.clone());
            self.now_playing_sent = false;
            self.scrobbled = false;
            self.started_at_unix_seconds = (self.now_ms)().div_euclid(1000);
        }
        self.last_position_seconds = position_seconds;

        if is_playing && !self.now_playing_sent {
            self.now_playing_sent = true;
            (self.callbacks.on_now_playing)(info);
        }
        if !self.scrobbled && meets_scrobble_threshold(info.duration_seconds, position_seconds) {
            self.scrobbled = true;
            (self.callbacks.on_scrobble)(info, self.started_at_unix_seconds);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    fn track(id: &str, duration: f64) -> ScrobbleTrackInfo {
        ScrobbleTrackInfo { file_id: id.into(), artist: format!("Artist {id}"), title: format!("Track {id}"), album: None, duration_seconds: duration }
    }

    struct Harness {
        tracker: ScrobbleTracker,
        now_playing: Rc<RefCell<Vec<ScrobbleTrackInfo>>>,
        scrobbles: Rc<RefCell<Vec<(ScrobbleTrackInfo, i64)>>>,
    }

    fn harness(now_seconds: i64) -> Harness {
        let now_playing = Rc::new(RefCell::new(Vec::new()));
        let scrobbles = Rc::new(RefCell::new(Vec::new()));
        let (np, sc) = (now_playing.clone(), scrobbles.clone());
        let tracker = ScrobbleTracker::new(
            ScrobbleTrackerCallbacks {
                on_now_playing: Box::new(move |t| np.borrow_mut().push(t.clone())),
                on_scrobble: Box::new(move |t, at| sc.borrow_mut().push((t.clone(), at))),
            },
            move || now_seconds * 1000,
        );
        Harness { tracker, now_playing, scrobbles }
    }

    #[test]
    fn fires_on_now_playing_once_at_the_start_of_playback_not_on_every_poll() {
        let mut h = harness(1_000);
        let a = track("a", 200.0);
        for p in [0.0, 1.0, 2.0] {
            h.tracker.update(Some(&a), true, p);
        }
        assert_eq!(*h.now_playing.borrow(), vec![a]);
    }

    #[test]
    fn does_not_fire_on_now_playing_while_paused_but_does_once_playback_starts() {
        let mut h = harness(1_000);
        let a = track("a", 200.0);
        h.tracker.update(Some(&a), false, 0.0);
        assert!(h.now_playing.borrow().is_empty());
        h.tracker.update(Some(&a), true, 0.0);
        assert_eq!(h.now_playing.borrow().len(), 1);
    }

    #[test]
    fn scrobbles_once_the_threshold_is_crossed_exactly_once() {
        let mut h = harness(5_000);
        let a = track("a", 200.0);
        h.tracker.update(Some(&a), true, 0.0);
        h.tracker.update(Some(&a), true, 90.0);
        assert!(h.scrobbles.borrow().is_empty());
        h.tracker.update(Some(&a), true, 100.0); // 50% of 200 s
        assert_eq!(*h.scrobbles.borrow(), vec![(a.clone(), 5_000)]);
        h.tracker.update(Some(&a), true, 150.0);
        assert_eq!(h.scrobbles.borrow().len(), 1);
    }

    #[test]
    fn never_scrobbles_a_track_under_the_30s_minimum() {
        let mut h = harness(1_000);
        h.tracker.update(Some(&track("b", 20.0)), true, 19.0);
        assert!(h.scrobbles.borrow().is_empty());
    }

    #[test]
    fn resets_and_refires_both_callbacks_when_the_track_changes() {
        let mut h = harness(1_000);
        let (a, c) = (track("a", 200.0), track("c", 60.0));
        h.tracker.update(Some(&a), true, 100.0);
        assert_eq!((h.now_playing.borrow().len(), h.scrobbles.borrow().len()), (1, 1));
        h.tracker.update(Some(&a), true, 190.0);
        h.tracker.update(Some(&c), true, 0.0);
        assert_eq!(h.now_playing.borrow().len(), 2);
        h.tracker.update(Some(&c), true, 30.0); // 50% of 60 s
        assert_eq!(h.scrobbles.borrow().len(), 2);
    }

    #[test]
    fn treats_a_large_backward_position_jump_on_the_same_track_as_a_new_play_instance() {
        let mut h = harness(1_000);
        let a = track("a", 200.0);
        h.tracker.update(Some(&a), true, 190.0);
        assert_eq!(h.scrobbles.borrow().len(), 1);
        h.tracker.update(Some(&a), true, 0.0); // looped back to the start
        assert_eq!(h.now_playing.borrow().len(), 2);
        h.tracker.update(Some(&a), true, 100.0);
        assert_eq!(h.scrobbles.borrow().len(), 2);
    }

    #[test]
    fn does_not_treat_a_small_seek_back_as_a_new_instance() {
        let mut h = harness(1_000);
        let a = track("a", 200.0);
        h.tracker.update(Some(&a), true, 50.0);
        h.tracker.update(Some(&a), true, 47.0);
        assert_eq!(h.now_playing.borrow().len(), 1);
    }

    #[test]
    fn clears_current_track_state_when_playback_stops_entirely() {
        let mut h = harness(1_000);
        let a = track("a", 200.0);
        h.tracker.update(Some(&a), true, 0.0);
        h.tracker.update(None, false, 0.0);
        h.tracker.update(Some(&a), true, 0.0);
        assert_eq!(h.now_playing.borrow().len(), 2);
    }

    #[test]
    fn the_start_time_is_whole_unix_seconds_from_the_clock_at_the_start_of_the_play() {
        let now = Rc::new(RefCell::new(1_500_999i64));
        let scrobbles = Rc::new(RefCell::new(Vec::new()));
        let (sc, clock) = (scrobbles.clone(), now.clone());
        let mut tracker = ScrobbleTracker::new(
            ScrobbleTrackerCallbacks { on_now_playing: Box::new(|_| {}), on_scrobble: Box::new(move |_, at| sc.borrow_mut().push(at)) },
            move || *clock.borrow(),
        );
        let a = track("a", 100.0);
        tracker.update(Some(&a), true, 0.0);
        *now.borrow_mut() = 1_700_000_000; // the clock moves on, the recorded start must not
        tracker.update(Some(&a), true, 60.0);
        assert_eq!(*scrobbles.borrow(), vec![1_500]);
    }
}
