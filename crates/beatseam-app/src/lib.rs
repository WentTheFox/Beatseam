//! Port of `packages/core/src/store`: the app's state and the actions that drive it, shared by every UI.
//!
//! The TypeScript used Redux Toolkit; here each slice is a plain struct whose methods are its reducers, [`Store`] holds the one
//! [`AppState`] and tells subscribers about every change, and the thunks become async functions over the services (file access,
//! the library store, the players, server mode). Nothing in a slice does I/O, so each is tested as plain data.

mod app;
mod library;
mod live_playback;
mod navigation;
mod notifications;
mod persistence;
mod playback;
mod restore;
mod settings;
mod store;

pub use app::{App, AppConfig, LibraryJobs, LyricsMatchCallbacks, POSITION_PERSIST_INTERVAL_MS, RAPID_PLAYBACK_PATCH_DEBOUNCE_MS};
pub use library::{LibraryRootData, LibraryState, RootWithLibrary};
pub use live_playback::LivePlayback;
pub use navigation::{CreatePlaylistTarget, EditingPlaylistFile, NavigationState, PlaylistContext, ScreenState};
pub use notifications::{NotificationActions, NotificationEntry, NotificationKind, NotificationProgress, NotificationStatus, NotificationsState, ProgressUpsert};
pub use persistence::{PersistenceState, PlaybackPatch, default_playback_state};
pub use playback::{CoarsePlayback, PlaybackSliceState};
pub use restore::{RESTORING_STEPS, RestoreState, RestoringStep};
pub use settings::*;
pub use store::Store;

/// Everything the UI reads.
#[derive(Clone, Debug, Default)]
pub struct AppState {
    pub settings: SettingsState,
    pub notifications: NotificationsState,
    pub library: LibraryState,
    pub navigation: NavigationState,
    pub playback: PlaybackSliceState,
    pub persistence: PersistenceState,
    pub restore: RestoreState,
}

#[cfg(test)]
mod app_tests;
