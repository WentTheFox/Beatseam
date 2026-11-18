use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use super::*;
use crate::audio_engine::fake::FakeEngine;
use crate::file_access::FileRef;
use crate::library_store::LoopMode;
use crate::tasks::Runtime;
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

type Player = PlaylistPlayer<FakeEngine, TestRuntime>;

struct Fixture {
    pool: Rc<Pool>,
    engine: Rc<FakeEngine>,
    errors: Rc<RefCell<Vec<(String, Option<String>)>>>,
    advances: Rc<Cell<usize>>,
    decoded: Rc<RefCell<Vec<String>>>,
    player: Player,
}

fn resolve_track() -> ResolveTrack {
    Rc::new(|id: &str| {
        let id = id.to_owned();
        Box::pin(async move { Ok(FileRef { id: id.clone(), name: id.clone(), relative_path: id, size_bytes: 0, last_modified_ms: 0.0 }) })
    })
}

fn fixture_with(configure: impl FnOnce(&mut PlaylistPlayerOptions)) -> Fixture {
    let pool = Rc::new(Pool::new());
    let runtime = Arc::new(TestRuntime { pool: pool.clone(), clock: Rc::new(Cell::new(1_000)) });
    let engine = Rc::new(FakeEngine::default());
    let (errors, advances, decoded): (Rc<RefCell<Vec<(String, Option<String>)>>>, Rc<Cell<usize>>, Rc<RefCell<Vec<String>>>) = Default::default();
    let (e, a, d) = (errors.clone(), advances.clone(), decoded.clone());
    let mut options = PlaylistPlayerOptions {
        on_error: Some(Rc::new(move |message: &str, file_id: Option<&str>| e.borrow_mut().push((message.to_owned(), file_id.map(str::to_owned))))),
        on_advance: Some(Rc::new(move || a.set(a.get() + 1))),
        on_decoded: Some(Rc::new(move |file, _audio| {
            let d = d.clone();
            Box::pin(async move { d.borrow_mut().push(file.id) })
        })),
        // A deterministic "random": always the middle.
        random: Some(Rc::new(|| 0.5)),
        ..Default::default()
    };
    configure(&mut options);
    let player = PlaylistPlayer::new(engine.clone(), runtime, resolve_track(), options);
    Fixture { pool, engine, errors, advances, decoded, player }
}

fn fixture() -> Fixture {
    fixture_with(|_| {})
}

fn ids(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

impl Fixture {
    fn settle(&self) {
        self.pool.run_until_stalled();
    }

    fn run<T: 'static>(&self, f: impl Future<Output = T> + 'static) -> T {
        let handle = self.pool.spawn(f);
        self.settle();
        handle.take().expect("finished")
    }

    fn set_playlist(&self, tracks: &[&str]) {
        let p = self.player.clone();
        let tracks = ids(tracks);
        self.run(async move { p.set_playlist(tracks, None, PlaylistOptions::default()).await });
    }

    fn set_playlist_from(&self, tracks: &[&str], start: &str) {
        let p = self.player.clone();
        let (tracks, start) = (ids(tracks), start.to_owned());
        self.run(async move { p.set_playlist(tracks, Some(&start), PlaylistOptions::default()).await });
    }

    fn next(&self, force: bool) {
        let p = self.player.clone();
        self.run(async move { p.next(force).await });
    }

    fn previous(&self, force: bool) {
        let p = self.player.clone();
        self.run(async move { p.previous(force).await });
    }

    fn current(&self) -> Option<String> {
        self.player.get_state().current_file_id
    }

    /// Plays the current source to its natural end.
    fn end_current_source(&self) {
        self.engine.fire_ended(&self.engine.last_source());
        self.settle();
    }

    fn status(&self) -> TrackPlayerStatus {
        self.player.get_state().track.status
    }
}

// ---------------------------------------------------------------- order, loop modes

#[test]
fn starts_on_the_first_track_in_sequential_order_and_can_start_on_a_specific_one() {
    let f = fixture();
    f.set_playlist(&["a", "b", "c"]);
    let state = f.player.get_state();
    assert_eq!((state.total_tracks, state.position, state.current_file_id.as_deref(), state.track.status), (3, 0, Some("a"), TrackPlayerStatus::Playing));
    f.set_playlist_from(&["a", "b", "c"], "c");
    assert_eq!(f.current().as_deref(), Some("c"));
    f.set_playlist_from(&["a", "b", "c"], "not-there");
    assert_eq!(f.current().as_deref(), Some("a"), "an unknown start id falls back to the first track");
}

#[test]
fn next_advances_sequentially_and_stops_at_the_end_when_loop_is_off() {
    let f = fixture();
    f.set_playlist(&["a", "b", "c"]);
    f.next(false);
    assert_eq!(f.current().as_deref(), Some("b"));
    f.next(false);
    f.next(false);
    assert_eq!(f.current().as_deref(), Some("c"));
}

#[test]
fn next_wraps_when_loop_is_all_and_previous_steps_back_and_wraps() {
    let f = fixture();
    f.player.set_loop_mode(LoopMode::All);
    f.set_playlist(&["a", "b", "c"]);
    f.previous(false);
    assert_eq!(f.current().as_deref(), Some("c"));
    f.next(false);
    assert_eq!(f.current().as_deref(), Some("a"));
    f.next(false);
    f.previous(false);
    assert_eq!(f.current().as_deref(), Some("a"));
}

#[test]
fn previous_clamps_at_the_start_when_loop_is_off() {
    let f = fixture();
    f.set_playlist(&["a", "b"]);
    f.previous(false);
    assert_eq!(f.current().as_deref(), Some("a"));
}

#[test]
fn a_track_ending_naturally_advances_to_the_next_one_and_reports_it() {
    let f = fixture();
    f.set_playlist(&["a", "b", "c"]);
    f.end_current_source();
    assert_eq!(f.current().as_deref(), Some("b"));
    assert_eq!(f.status(), TrackPlayerStatus::Playing);
    assert_eq!(f.advances.get(), 1);
}

#[test]
fn loop_one_replays_the_same_track_on_natural_end_and_makes_previous_restart_it() {
    let f = fixture();
    f.player.set_loop_mode(LoopMode::One);
    f.set_playlist(&["a", "b"]);
    f.end_current_source();
    assert_eq!(f.current().as_deref(), Some("a"));
    assert_eq!(f.status(), TrackPlayerStatus::Playing);
    f.engine.set_clock(3.0);
    f.previous(false);
    assert_eq!(f.current().as_deref(), Some("a"));
    assert_eq!(f.player.get_state().track.position_seconds, 0.0, "restarted rather than changed");
}

#[test]
fn loop_one_does_not_stop_a_manual_next_and_force_moves_tracks_even_so() {
    let f = fixture();
    f.player.set_loop_mode(LoopMode::One);
    f.set_playlist(&["a", "b", "c"]);
    f.next(false);
    assert_eq!(f.current().as_deref(), Some("b"));
    f.previous(true);
    assert_eq!(f.current().as_deref(), Some("a"));
}

#[test]
fn force_wraps_at_the_boundary_even_under_loop_off() {
    let f = fixture();
    f.set_playlist(&["a", "b", "c"]);
    f.previous(true);
    assert_eq!(f.current().as_deref(), Some("c"));
    f.next(true);
    assert_eq!(f.current().as_deref(), Some("a"));
}

#[test]
fn loop_all_wraps_on_natural_end_of_the_last_track_and_loop_off_stops() {
    let f = fixture();
    f.player.set_loop_mode(LoopMode::All);
    f.set_playlist_from(&["a", "b"], "b");
    f.end_current_source();
    assert_eq!(f.current().as_deref(), Some("a"));

    let g = fixture();
    g.set_playlist_from(&["a", "b"], "b");
    g.end_current_source();
    assert_eq!(g.current().as_deref(), Some("b"));
    assert_eq!(g.status(), TrackPlayerStatus::Stopped);
}

#[test]
fn overlapping_starts_do_not_race_only_the_latest_is_allowed_to_play() {
    let f = fixture();
    f.engine.gate_decode("a");
    let p = f.player.clone();
    let first = f.pool.spawn(async move { p.set_playlist(ids(&["a"]), None, PlaylistOptions::default()).await });
    f.settle();
    assert_eq!(f.status(), TrackPlayerStatus::Loading);
    f.set_playlist(&["b"]);
    assert_eq!(f.current().as_deref(), Some("b"));
    f.engine.release_decode("a");
    f.settle();
    assert!(first.is_done());
    assert_eq!(f.current().as_deref(), Some("b"));
    assert_eq!(f.engine.starts().len(), 1, "the superseded start never played");
}

// ---------------------------------------------------------------- shuffle

#[test]
fn shuffle_pins_the_current_track_on_top_keeps_it_playing_and_off_restores_sequential_order() {
    let f = fixture();
    f.set_playlist_from(&["a", "b", "c", "d"], "c");
    assert_eq!(f.player.get_shuffle_order(), None);
    f.player.set_shuffle(true);
    let order = f.player.get_shuffle_order().unwrap();
    assert_eq!(order[0], "c", "the current song goes to the top");
    let mut sorted = order.clone();
    sorted.sort();
    assert_eq!(sorted, ["a", "b", "c", "d"]);
    let state = f.player.get_state();
    assert_eq!((state.position, state.current_file_id.as_deref(), state.track.status), (0, Some("c"), TrackPlayerStatus::Playing));

    f.player.set_shuffle(false);
    let state = f.player.get_state();
    assert_eq!((state.position, state.current_file_id.as_deref()), (2, Some("c")));
    assert_eq!(f.player.get_shuffle_order(), None);
}

#[test]
fn shuffled_playback_visits_every_track_exactly_once_per_cycle() {
    let f = fixture();
    f.player.set_shuffle(true);
    f.set_playlist(&["a", "b", "c", "d", "e"]);
    let mut seen = vec![f.current().unwrap()];
    for _ in 0..4 {
        f.next(false);
        seen.push(f.current().unwrap());
    }
    seen.sort();
    assert_eq!(seen, ["a", "b", "c", "d", "e"]);
}

#[test]
fn load_playlist_resumes_a_persisted_shuffle_order_and_leaves_the_track_paused() {
    let f = fixture();
    f.player.set_shuffle(true);
    let p = f.player.clone();
    f.run(async move { p.load_playlist(ids(&["a", "b", "c", "d"]), Some("b"), PlaylistOptions { shuffle_order: Some(ids(&["d", "b", "a", "c"])), playlist_id: Some("pl".into()) }).await });
    assert_eq!(f.player.get_shuffle_order().unwrap(), ["b", "d", "a", "c"], "the persisted order, with the start track pinned on top");
    let state = f.player.get_state();
    assert_eq!((state.track.status, state.is_loading_for_playback), (TrackPlayerStatus::Stopped, false));
    assert_eq!(f.player.get_current_playlist_id().as_deref(), Some("pl"));
    assert!(f.engine.starts().is_empty(), "nothing autoplayed");
}

#[test]
fn a_persisted_shuffle_order_drops_removed_tracks_and_shuffles_in_new_ones() {
    let f = fixture();
    f.player.set_shuffle(true);
    let p = f.player.clone();
    f.run(async move { p.load_playlist(ids(&["a", "b", "c", "x"]), None, PlaylistOptions { shuffle_order: Some(ids(&["c", "gone", "a", "b"])), playlist_id: None }).await });
    let order = f.player.get_shuffle_order().unwrap();
    assert_eq!(&order[..3], ["c", "a", "b"]);
    assert_eq!(order[3], "x");
}

#[test]
fn shuffle_with_loop_all_reshuffles_for_the_next_lap_with_the_last_track_pinned_first() {
    let f = fixture();
    f.player.set_loop_mode(LoopMode::All);
    f.player.set_shuffle(true);
    f.set_playlist(&["a", "b", "c", "d"]);
    let first_lap = f.player.get_shuffle_order().unwrap();
    for _ in 0..3 {
        f.next(false);
    }
    // Reaching the last track of the lap regenerated the order with that track at position 0.
    let last = first_lap[3].clone();
    let state = f.player.get_state();
    assert_eq!((state.position, state.current_file_id.as_deref()), (0, Some(last.as_str())));
    assert_eq!(f.player.get_shuffle_order().unwrap()[0], last);
    assert_ne!(f.player.get_next_file_id().as_deref(), Some(last.as_str()));
}

#[test]
fn set_shuffle_with_nothing_loaded_just_records_the_flag() {
    let f = fixture();
    f.player.set_shuffle(true);
    assert!(f.player.get_state().shuffle_enabled);
    assert_eq!(f.player.get_shuffle_order(), Some(vec![]));
}

// ---------------------------------------------------------------- reconcile

fn order_of(f: &Fixture) -> Vec<String> {
    let state = f.player.get_state();
    let mut out = Vec::new();
    for position in 0..state.total_tracks as i64 {
        if let Some(id) = f.player.file_id_at_for_test(position) {
            out.push(id);
        }
    }
    out
}

impl<E: crate::audio_engine::AudioEngine + 'static, R: Runtime + 'static> PlaylistPlayer<E, R> {
    fn file_id_at_for_test(&self, position: i64) -> Option<String> {
        self.file_id_at(position)
    }
}

#[test]
fn reconcile_is_a_no_op_when_nothing_is_loaded() {
    let f = fixture();
    f.player.reconcile_playlist(ids(&["a"]));
    assert_eq!(f.player.get_state().total_tracks, 0);
}

#[test]
fn reconcile_sequential_appends_new_tracks_drops_removed_ones_and_keeps_the_current_playing() {
    let f = fixture();
    f.set_playlist_from(&["a", "b", "c"], "b");
    f.player.reconcile_playlist(ids(&["a", "b", "d", "c"]));
    assert_eq!(order_of(&f), ["a", "b", "d", "c"]);
    assert_eq!((f.player.get_state().position, f.current().as_deref()), (1, Some("b")));
    f.player.reconcile_playlist(ids(&["b", "c"]));
    assert_eq!(order_of(&f), ["b", "c"]);
    assert_eq!(f.current().as_deref(), Some("b"));
}

#[test]
fn reconcile_keeps_a_removed_current_track_alive_at_the_end() {
    let f = fixture();
    f.set_playlist_from(&["a", "b", "c"], "b");
    f.player.reconcile_playlist(ids(&["a", "c"]));
    assert_eq!(order_of(&f), ["a", "c", "b"]);
    assert_eq!(f.current().as_deref(), Some("b"));
    assert_eq!(f.player.get_state().total_tracks, 3);
}

#[test]
fn reconcile_shuffled_splices_new_tracks_in_without_disturbing_survivors_or_the_current_track() {
    let f = fixture();
    f.player.set_shuffle(true);
    f.set_playlist_from(&["a", "b", "c", "d"], "c");
    let before = f.player.get_shuffle_order().unwrap();
    f.player.reconcile_playlist(ids(&["a", "b", "c", "d", "e"]));
    let after = f.player.get_shuffle_order().unwrap();
    assert_eq!(after.iter().filter(|i| *i != "e").cloned().collect::<Vec<_>>(), before, "survivors keep their relative order");
    assert!(after.contains(&"e".to_string()));
    assert_eq!(f.current().as_deref(), Some("c"));

    f.player.reconcile_playlist(ids(&["a", "c", "d", "e"]));
    assert!(!f.player.get_shuffle_order().unwrap().contains(&"b".to_string()));
    assert_eq!(f.current().as_deref(), Some("c"));
}

// ---------------------------------------------------------------- state

#[test]
fn is_loading_for_playback_is_true_only_for_a_user_facing_load() {
    let f = fixture();
    f.engine.gate_decode("a");
    let p = f.player.clone();
    f.pool.spawn(async move { p.set_playlist(ids(&["a"]), None, PlaylistOptions::default()).await });
    f.settle();
    let state = f.player.get_state();
    assert_eq!((state.track.status, state.is_loading_for_playback), (TrackPlayerStatus::Loading, true));
    f.engine.release_decode("a");
    f.settle();

    let g = fixture();
    g.engine.gate_decode("a");
    let p = g.player.clone();
    g.pool.spawn(async move { p.load_playlist(ids(&["a"]), None, PlaylistOptions::default()).await });
    g.settle();
    let state = g.player.get_state();
    assert_eq!((state.track.status, state.is_loading_for_playback), (TrackPlayerStatus::Loading, false), "a silent restore must not show a spinner");
}

#[test]
fn next_file_id_follows_loop_mode() {
    let f = fixture();
    f.set_playlist(&["a", "b"]);
    assert_eq!(f.player.get_next_file_id().as_deref(), Some("b"));
    f.next(false);
    assert_eq!(f.player.get_next_file_id(), None, "the end with loop off");
    f.player.set_loop_mode(LoopMode::All);
    assert_eq!(f.player.get_next_file_id().as_deref(), Some("a"));
    f.player.set_loop_mode(LoopMode::One);
    assert_eq!(f.player.get_next_file_id(), None, "looping the current track");
}

#[test]
fn nothing_loaded_means_play_next_previous_do_nothing_and_settings_apply_immediately() {
    let f = fixture();
    f.player.play();
    f.next(false);
    f.previous(false);
    f.player.set_volume(0.6);
    f.player.set_loop_mode(LoopMode::All);
    f.player.check_preload();
    assert_eq!(f.player.get_volume(), 0.6);
    assert_eq!(f.player.get_state().loop_mode, LoopMode::All);
    assert_eq!(f.player.get_state().position, -1);
    assert!(f.engine.starts().is_empty() && f.errors.borrow().is_empty());
}

// ---------------------------------------------------------------- gain

#[test]
fn applies_the_resolved_gain_when_a_track_starts_and_falls_back_to_one() {
    let f = fixture_with(|o| {
        o.resolve_gain = Some(Rc::new(|id: &str| {
            let id = id.to_owned();
            Box::pin(async move { if id == "bad" { Err("lookup failed".to_owned()) } else { Ok(0.5) } })
        }));
    });
    f.player.set_volume(1.0);
    f.set_playlist(&["a", "bad"]);
    assert_eq!(f.engine.gain_of("source-0"), Some(0.5));
    f.next(false); // a manual skip crossfades: the incoming source starts silent and fades to the fallback gain
    let curves = f.engine.0.gain_curves_by_source_id.borrow().get("source-1").cloned().unwrap();
    assert!((curves[0].values.last().unwrap() - 1.0).abs() < 1e-6, "falls back to a gain of 1 instead of failing playback");
    assert!(f.errors.borrow().is_empty());
}

#[test]
fn defaults_to_a_gain_of_one_without_a_lookup() {
    let f = fixture();
    f.player.set_volume(1.0);
    f.set_playlist(&["a"]);
    assert_eq!(f.engine.gain_of("source-0"), Some(1.0));
}

// ---------------------------------------------------------------- preload

#[test]
fn check_preload_decodes_upcoming_tracks_ahead_and_advancing_to_one_does_not_decode_it_again() {
    let f = fixture();
    f.set_playlist(&["a", "b", "c", "d"]);
    f.player.check_preload();
    f.settle();
    assert_eq!(f.engine.decode_count("b"), 1);
    f.player.check_preload(); // one decode at a time: the second slot starts once the first settled
    f.settle();
    assert_eq!(f.engine.decode_count("c"), 1);
    f.end_current_source(); // natural end -> b (preloaded)
    assert_eq!(f.current().as_deref(), Some("b"));
    assert_eq!(f.engine.decode_count("b"), 1, "no second decode");
}

#[test]
fn check_preload_does_nothing_when_idle_or_looping_the_current_track() {
    let f = fixture();
    f.set_playlist(&["a", "b"]);
    f.player.pause();
    f.player.seek(1.0);
    f.player.set_loop_mode(LoopMode::One);
    f.player.check_preload();
    f.settle();
    assert_eq!(f.engine.decode_count("b"), 0);

    let g = fixture();
    let p = g.player.clone();
    g.run(async move { p.load_playlist(ids(&["a", "b"]), None, PlaylistOptions::default()).await });
    g.player.check_preload();
    g.settle();
    assert_eq!(g.engine.decode_count("b"), 0, "stopped, not playing or paused");
}

#[test]
fn on_decoded_fires_for_the_current_track_and_for_preloaded_ones_without_blocking_playback() {
    let f = fixture();
    f.set_playlist(&["a", "b", "c"]);
    assert_eq!(f.status(), TrackPlayerStatus::Playing);
    assert_eq!(*f.decoded.borrow(), ["a"]);
    f.player.check_preload();
    f.settle();
    assert_eq!(*f.decoded.borrow(), ["a", "b"]);
}

#[test]
fn a_decode_failure_resets_the_status_off_loading_and_reports_the_file() {
    let f = fixture();
    f.engine.fail_decoding("a");
    f.set_playlist(&["a", "b"]);
    assert_eq!(f.status(), TrackPlayerStatus::Idle, "not stuck on a spinner forever");
    let errors = f.errors.borrow();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].1.as_deref(), Some("a"));
    assert!(errors[0].0.contains("no such file"));
}

// ---------------------------------------------------------------- the natural crossfade

/// Plays "a" with "b"/"c" preloaded and the clock near the end of "a" (10 s long, 5 s crossfade).
fn near_the_end() -> Fixture {
    let f = fixture();
    f.set_playlist(&["a", "b", "c"]);
    f.player.check_preload();
    f.settle();
    f.player.check_preload();
    f.settle();
    f.engine.set_clock(5.0);
    f
}

#[test]
fn schedules_a_real_transition_near_the_end_but_advances_the_position_only_when_it_completes() {
    let f = near_the_end();
    f.player.check_preload();
    f.settle();
    let state = f.player.get_state();
    assert!(state.track.pending_incoming.is_some());
    assert_eq!(state.pending_crossfade_file_ids, Some(CrossfadeFileIds { outgoing: "a".into(), incoming: "b".into() }));
    assert_eq!((state.position, state.current_file_id.as_deref()), (0, Some("a")), "still the outgoing track");
    let curves = f.engine.0.gain_curves_by_source_id.borrow();
    assert!(curves.contains_key("source-0") && curves.contains_key("source-1"));
    assert!(f.engine.0.rate_ramps_by_source_id.borrow().is_empty(), "no rate change this round");
    drop(curves);
    assert_eq!(f.advances.get(), 0);

    f.engine.fire_ended("source-0"); // the outgoing source's scheduled stop
    let state = f.player.get_state();
    assert_eq!((state.position, state.current_file_id.as_deref(), state.pending_crossfade_file_ids), (1, Some("b"), None));
    assert_eq!(f.advances.get(), 1);
    assert_eq!(f.engine.decode_count("b"), 1);
}

#[test]
fn does_not_crossfade_before_the_next_track_has_finished_preloading_or_before_the_window() {
    let f = fixture();
    f.set_playlist(&["a", "b"]);
    f.engine.set_clock(5.0);
    f.engine.gate_decode("b");
    f.player.check_preload(); // starts decoding b, which is held
    f.settle();
    f.player.check_preload();
    f.settle();
    assert!(f.player.get_state().track.pending_incoming.is_none(), "b isn't ready");

    let g = fixture();
    g.set_playlist(&["a", "b"]);
    g.player.check_preload();
    g.settle();
    g.engine.set_clock(3.0); // 7 s remaining: outside crossfade + lead
    g.player.check_preload();
    g.settle();
    assert!(g.player.get_state().track.pending_incoming.is_none());
}

#[test]
fn never_double_triggers_a_crossfade_for_the_same_track_across_repeated_ticks() {
    let f = near_the_end();
    for _ in 0..5 {
        f.player.check_preload();
        f.settle();
    }
    assert_eq!(f.engine.starts().len(), 2, "the original start plus exactly one incoming");
}

#[test]
fn set_crossfade_seconds_changes_the_duration_of_the_next_transition() {
    let f = fixture();
    f.player.set_crossfade_seconds(2.0);
    assert_eq!(f.player.get_crossfade_seconds(), 2.0);
    f.set_playlist(&["a", "b"]);
    f.player.check_preload();
    f.settle();
    f.engine.set_clock(5.0); // 5 s left: outside 2 s + 1 s lead
    f.player.check_preload();
    f.settle();
    assert!(f.player.get_state().track.pending_incoming.is_none());
    f.engine.set_clock(7.5);
    f.player.check_preload();
    f.settle();
    assert_eq!(f.player.get_state().track.pending_incoming.map(|p| p.fade_duration_seconds), Some(2.0));
}

#[test]
fn does_not_retry_storm_a_repeatable_scheduling_failure() {
    let f = near_the_end();
    f.engine.0.fail_ramp_gain_curve.set(true);
    for _ in 0..6 {
        f.player.check_preload();
        f.settle();
    }
    assert_eq!(f.errors.borrow().len(), 1, "one report, not one per tick");
    assert_eq!(f.engine.starts().len(), 1, "nothing half-scheduled kept piling up");
    assert!(f.player.get_state().track.pending_incoming.is_none());
}

#[test]
fn a_playlist_change_resets_the_retry_guard_so_a_since_fixed_failure_can_retry() {
    let f = near_the_end();
    f.engine.0.fail_ramp_gain_curve.set(true);
    f.player.check_preload();
    f.settle();
    assert_eq!(f.errors.borrow().len(), 1);
    f.engine.0.fail_ramp_gain_curve.set(false);
    f.player.check_preload();
    f.settle();
    assert!(f.player.get_state().track.pending_incoming.is_none(), "still suppressed for this position");
    f.player.reconcile_playlist(ids(&["a", "b", "c", "d"]));
    f.player.check_preload();
    f.settle();
    f.player.check_preload();
    f.settle();
    assert!(f.player.get_state().track.pending_incoming.is_some(), "retried after the playlist changed");
}

#[test]
fn reconcile_is_a_no_op_while_a_crossfade_is_in_flight() {
    let f = near_the_end();
    f.player.check_preload();
    f.settle();
    assert!(f.player.get_state().track.pending_incoming.is_some());
    f.player.reconcile_playlist(ids(&["a", "x", "b", "c"]));
    assert_eq!(f.player.get_state().total_tracks, 3, "untouched");
    f.engine.fire_ended("source-0");
    assert_eq!(f.current().as_deref(), Some("b"), "the pending transition still completes to the original next track");
}

// ---------------------------------------------------------------- manual skips

#[test]
fn a_manual_skip_crossfades_briefly_and_moves_the_position_immediately() {
    let f = fixture();
    f.set_playlist(&["a", "b", "c"]);
    f.engine.set_clock(2.0);
    f.next(false);
    let state = f.player.get_state();
    assert_eq!((state.position, state.current_file_id.as_deref()), (1, Some("b")), "snappy: no waiting for the fade");
    assert_eq!(state.track.pending_incoming.map(|p| p.fade_duration_seconds), Some(1.0), "the short manual-skip crossfade");
    assert_eq!(state.pending_crossfade_file_ids, Some(CrossfadeFileIds { outgoing: "a".into(), incoming: "b".into() }));
    assert_eq!(f.player.get_next_file_id().as_deref(), Some("c"), "'next' already means the one after the incoming track");

    f.engine.fire_ended("source-0");
    let state = f.player.get_state();
    assert_eq!((state.position, state.current_file_id.as_deref(), state.pending_crossfade_file_ids), (1, Some("b"), None), "completing does not advance again");
    assert_eq!(f.advances.get(), 1);
}

#[test]
fn a_manual_skip_falls_back_to_a_hard_cut_when_nothing_is_playing() {
    let f = fixture();
    f.set_playlist(&["a", "b"]);
    f.player.pause();
    f.next(false);
    assert_eq!(f.current().as_deref(), Some("b"));
    assert_eq!(f.status(), TrackPlayerStatus::Playing, "a plain hard cut, as a play request does");
    assert!(f.player.get_state().track.pending_incoming.is_none());
}

#[test]
fn a_manual_skip_reuses_a_decode_the_preload_scheduler_has_in_flight() {
    let f = fixture();
    f.set_playlist(&["a", "b", "c"]);
    f.engine.gate_decode("b");
    f.player.check_preload(); // the scheduler begins decoding b, held at the gate
    f.settle();
    assert_eq!(f.engine.decode_count("b"), 1);
    let p = f.player.clone();
    let skip = f.pool.spawn(async move { p.next(false).await });
    f.settle();
    assert_eq!(f.engine.decode_count("b"), 1, "the skip joined the in-flight decode instead of starting a second one");
    f.engine.release_decode("b");
    f.settle();
    assert!(skip.is_done());
    assert_eq!(f.current().as_deref(), Some("b"));
    assert!(f.player.get_state().track.pending_incoming.is_some());
}

#[test]
fn the_first_start_waits_for_the_persisted_volume_so_it_never_starts_at_the_wrong_one() {
    use crate::tasks::Gate;
    let f = fixture();
    let gate = Gate::new();
    let g = gate.clone();
    f.player.await_initial_volume(async move {
        g.wait().await;
        0.8
    });
    let p = f.player.clone();
    let start = f.pool.spawn(async move { p.set_playlist(ids(&["a"]), None, PlaylistOptions::default()).await });
    f.settle();
    assert!(!start.is_done());
    assert!(f.engine.starts().is_empty(), "nothing started before the real volume is known");
    gate.open();
    f.settle();
    assert!(start.is_done());
    assert_eq!(f.engine.gain_of("source-0"), Some(0.8));
    assert_eq!(f.player.get_volume(), 0.8);
}
