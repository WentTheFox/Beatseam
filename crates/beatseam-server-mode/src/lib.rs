//! Port of `packages/core/src/server-mode`: what the app does when its library comes from a Beatseam server.

mod apply_sync;
mod device_playback;
mod mutation_queue;
mod random_id;

pub use apply_sync::{ApplyContext, SERVER_LYRICS_ASSIGNMENT, SYNCED_SETTING_KEYS, USER_PLAYLIST_ID_PREFIX, apply_change, apply_track};
pub use device_playback::{DEVICE_PLAYBACK_KEY, RemotePlayback, is_playing_now, list_device_playback, playback_payload, store_device_playback, to_remote_playback};
pub use mutation_queue::{MutationQueue, MutationQueueState, MutationSender, Rejection, Spawn};
pub use random_id::{DEFAULT_ID_LENGTH, random_id};
