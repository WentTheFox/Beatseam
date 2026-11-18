//! Port of `packages/core/src/store`: the app's state and the actions that drive it, shared by every UI.
//!
//! The TypeScript used Redux Toolkit; here each slice is a plain struct whose methods are its reducers, [`Store`] holds the one
//! [`AppState`] and tells subscribers about every change, and the thunks become async functions over the services (file access,
//! the library store, the players, server mode). Nothing in a slice does I/O, so each is tested as plain data.

mod library;
mod notifications;
mod persistence;
mod playback;
mod restore;
mod settings;
mod store;

pub use library::{LibraryRootData, LibraryState, RootWithLibrary};
pub use notifications::{NotificationActions, NotificationEntry, NotificationKind, NotificationProgress, NotificationStatus, NotificationsState, ProgressUpsert};
pub use persistence::{PersistenceState, PlaybackPatch, default_playback_state};
pub use playback::{CoarsePlayback, PlaybackSliceState};
pub use restore::{RESTORING_STEPS, RestoreState, RestoringStep};
pub use settings::*;
pub use store::Store;

/// Everything the UI reads. (The navigation slice joins it when that is ported.)
#[derive(Clone, Debug, Default)]
pub struct AppState {
    pub settings: SettingsState,
    pub notifications: NotificationsState,
    pub library: LibraryState,
    pub playback: PlaybackSliceState,
    pub persistence: PersistenceState,
    pub restore: RestoreState,
}
