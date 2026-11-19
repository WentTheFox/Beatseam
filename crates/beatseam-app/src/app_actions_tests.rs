//! Tests of the playlist screens' actions, continuing another device, notifications and the task mirror (the fixture lives in
//! `app_tests`).

use super::*;

fn callbacks() -> (PlaylistActionCallbacks, Rc<RefCell<Vec<String>>>, Rc<RefCell<Vec<String>>>) {
    let (opened, rescanned): (Rc<RefCell<Vec<String>>>, Rc<RefCell<Vec<String>>>) = Default::default();
    let (o, r) = (opened.clone(), rescanned.clone());
    (PlaylistActionCallbacks { rescan: Some(Rc::new(move |root| r.borrow_mut().push(root.to_owned()))), on_opened: Some(Rc::new(move |playlist, root| o.borrow_mut().push(format!("{}@{root}", playlist.id)))) }, opened, rescanned)
}

fn ids(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// A restored library with "Mix" playing.
fn with_open_mix() -> Fixture {
    let f = fixture();
    f.restore(Some(stored_state()));
    f
}

fn open_playlist_ids(f: &Fixture) -> Vec<String> {
    match f.read(|s| s.navigation.screen.clone()) {
        ScreenState::Playlist(c) => c.playlist.track_file_ids,
        ScreenState::Library => panic!("expected a playlist screen"),
    }
}

#[test]
fn now_playing_mirrors_what_is_playing_in_file_order() {
    let f = with_open_mix();
    let (cb, opened, _) = callbacks();
    f.app.show_now_playing(&cb);
    match f.read(|s| s.navigation.screen.clone()) {
        ScreenState::Playlist(c) => assert_eq!((c.playlist.id.as_str(), c.playlist.name.as_str(), c.playlist.track_file_ids.clone()), ("virtual:root-1:now-playing", "Now Playing", ids(&["Track A.mp3", "Track B.mp3"]))),
        ScreenState::Library => panic!("expected a playlist"),
    }
    assert_eq!(*opened.borrow(), ["virtual:root-1:now-playing@root-1"]);

    let blank = fixture();
    blank.app.show_now_playing(&cb);
    assert_eq!(blank.read(|s| s.navigation.screen.clone()), ScreenState::Library, "nothing playing: nothing to show");
}

#[test]
fn now_playing_follows_the_live_shuffle_order_when_shuffle_is_on() {
    let f = with_open_mix();
    f.app.publish_live();
    f.pool.run_until_stalled();
    f.app.cfg().player.set_shuffle(true);
    let live_order = f.app.cfg().player.get_shuffle_order().expect("shuffled");
    f.app.show_now_playing(&PlaylistActionCallbacks::default());
    assert_eq!(open_playlist_ids(&f), live_order);
    assert_eq!(live_order[0], "Track B.mp3", "the current track is pinned on top");
}

#[test]
fn unplaylisted_opens_at_once_then_fills_in_with_the_tracks_no_playlist_references() {
    let f = fixture_with(&[("Mix.m3u8", "Track A.mp3"), ("Track A.mp3", "a"), ("Loose One.mp3", "x"), ("Loose Two.mp3", "y")], vec![root()], &[]);
    f.restore(None);
    let (cb, opened, _) = callbacks();
    let app = f.app.clone();
    f.pool.spawn(async move { app.show_unplaylisted("root-1", &cb).await });
    f.settle();
    match f.read(|s| s.navigation.screen.clone()) {
        ScreenState::Playlist(c) => {
            assert_eq!((c.playlist.id.as_str(), c.playlist.name.as_str()), ("virtual:root-1:unplaylisted", "Songs not in a playlist"));
            let mut found = c.playlist.track_file_ids.clone();
            found.sort();
            assert_eq!(found, ["Loose One.mp3", "Loose Two.mp3"]);
            assert_eq!(c.tracks.len(), 2);
        }
        ScreenState::Library => panic!("expected the unplaylisted screen"),
    }
    assert!(!f.read(|s| s.navigation.is_finding_unplaylisted));
    assert_eq!(*opened.borrow(), ["virtual:root-1:unplaylisted@root-1"]);
}

#[test]
fn asking_for_unplaylisted_again_while_viewing_it_does_not_walk_again_and_an_unknown_root_is_ignored() {
    let f = fixture_with(&[("Loose.mp3", "x")], vec![root()], &[]);
    f.restore(None);
    let (cb, opened, _) = callbacks();
    for root_id in ["root-1", "root-1", "nope"] {
        let (app, cb) = (f.app.clone(), cb.clone());
        f.pool.spawn(async move { app.show_unplaylisted(root_id, &cb).await });
        f.settle();
    }
    assert_eq!(opened.borrow().len(), 1);
}

#[test]
fn reordering_a_playlist_file_rewrites_it_updates_the_open_screen_at_once_and_rescans() {
    let f = with_open_mix();
    let (cb, _, rescanned) = callbacks();
    let context = f.read(|s| s.navigation.playing_context.clone()).unwrap();
    f.app.open_playlist(context, &cb);
    let app = f.app.clone();
    f.pool.spawn(async move { app.move_open_playlist_tracks(&ids(&["Track B.mp3"]), "Track A.mp3", beatseam_core::playlist::Position::Before, &cb).await });
    f.settle();
    assert_eq!(f.file_access.contents()["Mix.m3u8"], "#EXTM3U\nTrack B.mp3\nTrack A.mp3\n");
    assert_eq!(open_playlist_ids(&f), ["Track B.mp3", "Track A.mp3"]);
    assert_eq!(*rescanned.borrow(), ["root-1"]);
}

#[test]
fn removing_tracks_from_a_playlist_file_and_adding_the_pending_one_to_another() {
    let f = with_open_mix();
    let (cb, _, rescanned) = callbacks();
    let context = f.read(|s| s.navigation.playing_context.clone()).unwrap();
    let other = f.read(|s| s.library.roots[0].playlists.iter().find(|p| p.id == "Other.m3u8").cloned()).unwrap();
    f.app.open_playlist(context, &cb);

    let track_a = f.read(|s| s.library.roots[0].tracks.iter().find(|t| t.file_id == "Track A.mp3").cloned()).unwrap();
    f.app.store().dispatch(move |s| s.navigation.add_to_playlist_track_set(Some(track_a)));
    let (app, cb2, other2) = (f.app.clone(), cb.clone(), other.clone());
    f.pool.spawn(async move { app.add_pending_track_to_playlist(&other2, false, &cb2).await });
    f.settle();
    assert!(f.file_access.contents()["Other.m3u8"].contains("Track A.mp3"), "appended to the chosen playlist's file");
    assert!(f.read(|s| s.navigation.add_to_playlist_track.is_none()), "the offer is cleared");

    let (app, cb3) = (f.app.clone(), cb.clone());
    f.pool.spawn(async move { app.remove_open_playlist_tracks(&ids(&["Track A.mp3"]), &cb3).await });
    f.settle();
    assert_eq!(f.file_access.contents()["Mix.m3u8"], "#EXTM3U\nTrack B.mp3\n");
    assert_eq!(open_playlist_ids(&f), ["Track B.mp3"]);
    assert_eq!(rescanned.borrow().len(), 2);
}

#[test]
fn a_failed_playlist_edit_is_reported_as_a_notification_and_leaves_the_screen_alone() {
    let f = with_open_mix();
    let (cb, _, rescanned) = callbacks();
    let context = f.read(|s| s.navigation.playing_context.clone()).unwrap();
    f.app.open_playlist(context, &cb);
    f.file_access.remove_file("Mix.m3u8"); // the playlist file vanished from disk
    let (app, cb2) = (f.app.clone(), cb.clone());
    f.pool.spawn(async move { app.remove_open_playlist_tracks(&ids(&["Track A.mp3"]), &cb2).await });
    f.settle();
    let errors = f.read(|s| s.notifications.items.clone());
    assert_eq!(errors.len(), 1);
    assert!(errors[0].title.starts_with("Couldn't remove tracks from \"Mix\""), "{}", errors[0].title);
    assert!(errors[0].detail.is_some());
    assert!(rescanned.borrow().is_empty());
    assert_eq!(open_playlist_ids(&f).len(), 2, "unchanged");
}

#[test]
fn a_persons_own_playlist_is_edited_through_the_server_not_by_rewriting_a_file() {
    let f = with_open_mix();
    f.log.borrow_mut().user_playlists_active = true;
    let (cb, _, rescanned) = callbacks();
    let own = PlaylistContext {
        root: root(),
        playlist: beatseam_core::library_store::PlaylistRecord { id: "user:x".into(), root_id: "root-1".into(), file_id: "user:x".into(), name: "Mine".into(), track_file_ids: ids(&["Track A.mp3", "Track B.mp3"]) },
        tracks: vec![],
    };
    f.app.open_playlist(own, &cb);
    let (app, cb2) = (f.app.clone(), cb.clone());
    f.pool.spawn(async move {
        app.move_open_playlist_tracks(&ids(&["Track B.mp3"]), "Track A.mp3", beatseam_core::playlist::Position::Before, &cb2).await;
        app.remove_open_playlist_tracks(&ids(&["Track A.mp3"]), &cb2).await;
        app.rename_open_user_playlist(" Renamed ").await;
    });
    f.settle();
    assert_eq!(f.log.borrow().user_ops, ["set user:x Track B.mp3,Track A.mp3", "remove user:x Track A.mp3", "rename user:x  Renamed "]);
    assert!(f.file_access.contents()["Mix.m3u8"].starts_with("Track A.mp3"), "no file was rewritten");
    assert!(rescanned.borrow().is_empty(), "no folder rescan either: the library reloads from the store");
    match f.read(|s| s.navigation.screen.clone()) {
        ScreenState::Playlist(c) => assert_eq!((c.playlist.name.as_str(), c.playlist.track_file_ids.clone()), ("Renamed", ids(&["Track B.mp3"]))),
        ScreenState::Library => unreachable!(),
    }
    let app = f.app.clone();
    f.pool.spawn(async move { app.delete_open_user_playlist().await });
    f.settle();
    assert_eq!(f.read(|s| s.navigation.screen.clone()), ScreenState::Library, "back to the library after a delete");
    assert_eq!(f.log.borrow().user_ops.last().map(String::as_str), Some("delete user:x"));
}

#[test]
fn creating_a_users_own_playlist_opens_it_and_a_file_playlist_is_never_renamed_this_way() {
    let f = with_open_mix();
    let (cb, opened, _) = callbacks();
    let app = f.app.clone();
    f.pool.spawn(async move { app.create_user_playlist_and_open("Road trip", &ids(&["Track A.mp3"]), &cb).await });
    f.settle();
    assert!(f.log.borrow().user_ops.is_empty() && opened.borrow().is_empty(), "not in server mode: nothing happens");

    f.log.borrow_mut().user_playlists_active = true;
    let (cb, opened2, _) = callbacks();
    let app = f.app.clone();
    f.pool.spawn(async move { app.create_user_playlist_and_open("Road trip", &ids(&["Track A.mp3"]), &cb).await });
    f.settle();
    assert_eq!(f.log.borrow().user_ops, ["create Road trip Track A.mp3"]);
    assert!(matches!(f.read(|s| s.navigation.screen.clone()), ScreenState::Playlist(c) if c.playlist.id == "user:new"));
    assert_eq!(*opened2.borrow(), ["user:new@root-1"]);

    let file_playlist = f.read(|s| s.library.roots[0].playlists.iter().find(|p| p.id == "Mix.m3u8").cloned()).unwrap();
    f.app.open_playlist(PlaylistContext { root: root(), playlist: file_playlist, tracks: vec![] }, &PlaylistActionCallbacks::default());
    let app = f.app.clone();
    f.pool.spawn(async move { app.rename_open_user_playlist("x").await });
    f.settle();
    assert_eq!(f.log.borrow().user_ops.len(), 1, "a playlist file is renamed by editing the file, not through the server");
}

fn remote(playlist: &str, track: &str) -> beatseam_server_mode::RemotePlayback {
    beatseam_server_mode::RemotePlayback {
        device_id: "other".into(),
        device_name: "Pixel".into(),
        updated_at_ms: 1.0,
        playlist_id: playlist.into(),
        root_id: Some("root-1".into()),
        current_track_file_id: track.into(),
        position_seconds: 42.0,
        loop_mode: LoopMode::One,
        shuffle_enabled: false,
        shuffle_order: None,
        playing: true,
    }
}

#[test]
fn continuing_another_devices_playback_starts_the_same_playlist_track_and_position_here() {
    let f = fixture();
    f.restore(None); // loads the library
    let app = f.app.clone();
    let result = f.pool.spawn(async move { app.continue_playback(&remote("Mix.m3u8", "Track B.mp3")).await });
    f.settle();
    assert_eq!(result.take(), Some(Ok(true)));
    let player = f.app.publish_live();
    assert_eq!((player.current_file_id.as_deref(), player.track.status, player.track.position_seconds, player.loop_mode), (Some("Track B.mp3"), TrackPlayerStatus::Playing, 42.0, LoopMode::One));
    assert_eq!(f.read(|s| s.navigation.playing_context.as_ref().map(|c| c.playlist.id.clone())).as_deref(), Some("Mix.m3u8"));
    let persisted = f.read(|s| s.persistence.state.clone());
    assert_eq!((persisted.playlist_id.as_deref(), persisted.current_track_file_id.as_deref(), persisted.position_seconds, persisted.loop_mode), (Some("Mix.m3u8"), Some("Track B.mp3"), 42.0, LoopMode::One));
}

#[test]
fn continuing_something_this_device_does_not_have_touches_nothing() {
    let f = fixture();
    f.restore(None);
    for r in [remote("Gone.m3u8", "Track A.mp3"), remote("Mix.m3u8", "Gone.mp3")] {
        let app = f.app.clone();
        let result = f.pool.spawn(async move { app.continue_playback(&r).await });
        f.settle();
        assert_eq!(result.take(), Some(Ok(false)));
    }
    assert_eq!(f.app.publish_live().track.status, TrackPlayerStatus::Idle);
    assert!(f.read(|s| s.navigation.playing_context.is_none()));
}

#[test]
fn notifications_can_be_added_updated_acted_on_dismissed_and_cleared() {
    let f = fixture();
    f.runtime.clock.set(500);
    f.app.add_error("Something broke", Some("raw detail"));
    f.app.add_error("Another", None);
    let items = f.read(|s| s.notifications.items.clone());
    assert_eq!((items[0].title.as_str(), items[1].title.as_str()), ("Another", "Something broke"));
    assert_ne!(items[0].id, items[1].id, "ids are unique");

    let pressed = Rc::new(Cell::new(0));
    let p = pressed.clone();
    let entry = |current| ProgressUpsert { id: "scan".into(), title: "Scanning".into(), detail: None, progress: Some(NotificationProgress { current, total: 10, done: false }), action_label: None, status: None };
    f.app.upsert_notification(ProgressNotification { entry: entry(1), action: Some(("Cancel".into(), Rc::new(move || p.set(p.get() + 1)))) });
    f.app.run_notification_action("scan");
    assert_eq!(pressed.get(), 1);
    f.runtime.clock.set(900);
    f.app.upsert_notification(ProgressNotification { entry: entry(5), action: Some(("Cancel".into(), Rc::new(|| {}))) });
    let scan = f.read(|s| s.notifications.items.iter().find(|n| n.id == "scan").cloned()).unwrap();
    assert_eq!((scan.action_label.as_deref(), scan.created_at, scan.progress.map(|p| p.current)), (Some("Cancel"), 500, Some(5)));
    f.app.run_notification_action("scan"); // the second upsert replaced the first callback
    assert_eq!(pressed.get(), 1);

    f.app.dismiss_notification("scan");
    assert!(f.read(|s| s.notifications.items.iter().all(|n| n.id != "scan")));
    f.app.clear_all_notifications();
    assert!(f.read(|s| s.notifications.items.is_empty()));
}

#[test]
fn the_task_queue_is_mirrored_into_notifications_with_cancel_and_a_brief_done_state() {
    use beatseam_core::tasks::{TaskOptions, TaskPriority, TaskProgress, TaskQueue};
    let f = fixture();
    let queue = TaskQueue::new(|| 0);
    let sync = TaskNotificationSync::start(f.app.clone(), queue.clone(), Arc::new(|| {}));
    assert!(f.read(|s| s.notifications.items.is_empty()));

    let gate = Gate::new();
    let g = gate.clone();
    let finished = Rc::new(Cell::new(false));
    let fin = finished.clone();
    let options = TaskOptions::new("scanning-r", "Scanning \"Music\"", TaskPriority::Foreground).cancel(|| {});
    let q = queue.clone();
    f.pool.spawn(async move {
        q.run_task(options, |context| async move {
            context.report_progress(TaskProgress { detail: "5 files in 2 folders".into(), current: 5, total: 10 });
            g.wait().await;
        })
        .await;
        fin.set(true);
    });
    f.settle();
    sync.process();
    let entry = f.read(|s| s.notifications.items.iter().find(|n| n.id == "scanning-r").cloned()).unwrap();
    assert_eq!(entry.title, "Scanning \"Music\" - 5 files in 2 folders");
    assert_eq!(entry.progress, Some(NotificationProgress { current: 5, total: 10, done: false }));
    assert_eq!((entry.action_label.as_deref(), entry.status), (Some("Cancel"), None));

    gate.open();
    f.settle();
    sync.process();
    assert!(finished.get());
    let done = f.read(|s| s.notifications.items.iter().find(|n| n.id == "scanning-r").cloned()).unwrap();
    assert_eq!(done.progress, Some(NotificationProgress { current: 10, total: 10, done: true }), "left on screen as done for a moment");
    f.settle(); // the auto-dismiss timer registers its sleep
    let gates: Vec<Gate> = f.runtime.sleeps.borrow().iter().map(|(_, g)| g.clone()).collect();
    for gate in gates {
        gate.open();
    }
    f.settle();
    assert!(f.read(|s| s.notifications.items.iter().all(|n| n.id != "scanning-r")), "dismissed on its own after a few seconds");
    sync.stop();
}

#[test]
fn a_finished_task_with_no_known_total_just_disappears() {
    use beatseam_core::tasks::{TaskOptions, TaskPriority, TaskQueue};
    let f = fixture();
    let queue = TaskQueue::new(|| 0);
    let sync = TaskNotificationSync::start(f.app.clone(), queue.clone(), Arc::new(|| {}));
    let gate = Gate::new();
    let g = gate.clone();
    let q = queue.clone();
    f.pool.spawn(async move { q.run_task(TaskOptions::new("quick", "Quick", TaskPriority::Background), |_| async move { g.wait().await }).await });
    f.settle();
    sync.process();
    assert!(f.read(|s| s.notifications.items.iter().any(|n| n.id == "quick")));
    gate.open();
    f.settle();
    sync.process();
    assert!(f.read(|s| s.notifications.items.is_empty()));
}
