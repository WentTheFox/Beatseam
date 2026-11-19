use std::cell::{Cell, RefCell};
use std::rc::Rc;

use beatseam_core::playback::PlaylistPlayerState;

/// The player's full state, including the fast-moving parts (position, crossfade progress), as a subscribable snapshot - the single
/// place the UI reads it from. Deliberately NOT part of the store's state: positions change ~5 times a second, and routing every
/// tick through the store would re-run every selector for nothing. The store instead mirrors only the coarse fields (see
/// [`PlaybackSliceState`](crate::PlaybackSliceState)) that change when something meaningful happens.
pub struct LivePlayback {
    snapshot: RefCell<PlaylistPlayerState>,
    listeners: RefCell<Vec<(u64, Rc<dyn Fn(&PlaylistPlayerState)>)>>,
    next_listener: Cell<u64>,
}

impl LivePlayback {
    pub fn new(initial: PlaylistPlayerState) -> Self {
        Self { snapshot: RefCell::new(initial), listeners: RefCell::new(Vec::new()), next_listener: Cell::new(0) }
    }

    pub fn snapshot(&self) -> PlaylistPlayerState {
        self.snapshot.borrow().clone()
    }

    /// Calls `listener` on every publish. Returns an id for [`unsubscribe`](Self::unsubscribe).
    pub fn subscribe(&self, listener: impl Fn(&PlaylistPlayerState) + 'static) -> u64 {
        let id = self.next_listener.get() + 1;
        self.next_listener.set(id);
        self.listeners.borrow_mut().push((id, Rc::new(listener)));
        id
    }

    pub fn unsubscribe(&self, id: u64) {
        self.listeners.borrow_mut().retain(|(i, _)| *i != id);
    }

    /// Takes a fresh state and notifies subscribers. Call after anything that changed the player, or on the ticker.
    pub fn publish(&self, state: PlaylistPlayerState) -> PlaylistPlayerState {
        *self.snapshot.borrow_mut() = state.clone();
        let listeners: Vec<_> = self.listeners.borrow().iter().map(|(_, l)| l.clone()).collect();
        for listener in listeners {
            listener(&state);
        }
        state
    }
}

#[cfg(test)]
mod tests {
    use beatseam_core::library_store::LoopMode;
    use beatseam_core::playback::{TrackPlayerState, TrackPlayerStatus};

    use super::*;

    fn state(position: f64) -> PlaylistPlayerState {
        PlaylistPlayerState {
            total_tracks: 1,
            position: 0,
            current_file_id: Some("a".into()),
            loop_mode: LoopMode::Off,
            shuffle_enabled: false,
            track: TrackPlayerState { status: TrackPlayerStatus::Playing, is_audible: true, position_seconds: position, duration_seconds: 10.0, pending_incoming: None },
            pending_crossfade_file_ids: None,
            is_loading_for_playback: false,
        }
    }

    #[test]
    fn publishing_updates_the_snapshot_and_tells_subscribers_until_they_unsubscribe() {
        let live = LivePlayback::new(state(0.0));
        let seen = Rc::new(RefCell::new(Vec::new()));
        let s = seen.clone();
        let id = live.subscribe(move |st| s.borrow_mut().push(st.track.position_seconds));
        assert_eq!(live.publish(state(1.5)).track.position_seconds, 1.5);
        live.publish(state(2.5));
        assert_eq!(live.snapshot().track.position_seconds, 2.5);
        live.unsubscribe(id);
        live.publish(state(9.0));
        assert_eq!(*seen.borrow(), [1.5, 2.5]);
    }
}
