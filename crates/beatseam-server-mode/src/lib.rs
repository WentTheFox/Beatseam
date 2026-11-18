//! Port of `packages/core/src/server-mode`: what the app does when its library comes from a Beatseam server.

mod apply_sync;
mod device_playback;
mod lyrics_editing;
mod mutation_queue;
mod random_id;
mod server_covers;
mod server_file_access;
mod server_last_fm;
mod server_mode;
mod server_url;
mod user_playlists;

pub use apply_sync::{ApplyContext, SERVER_LYRICS_ASSIGNMENT, SYNCED_SETTING_KEYS, USER_PLAYLIST_ID_PREFIX, apply_change, apply_track};
pub use device_playback::{DEVICE_PLAYBACK_KEY, RemotePlayback, is_playing_now, list_device_playback, playback_payload, store_device_playback, to_remote_playback};
pub use mutation_queue::{MutationQueue, MutationQueueState, MutationSender, Rejection, Spawn};
pub use random_id::{DEFAULT_ID_LENGTH, random_id};
pub use lyrics_editing::{EditedLine, LyricsEditError, LyricsSource, ServerLyricsEditing};
pub use server_covers::{CoverError, CoverFetch, CoverFetchError, ServerCovers};
pub use server_file_access::{ModeAwareFileAccess, NoOffline, OfflineAccess, ServerFileAccess, ServerLink, unsupported};
pub use server_last_fm::{SERVER_SCROBBLE_QUEUE_KEY, ServerLastFmApi, ServerScrobbleSink};
pub use user_playlists::{UserPlaylistDeps, UserPlaylistError, is_user_playlist_id};
pub use server_mode::{NoSecretStore, SecretStore, ServerMode, ServerModeDeps, ServerModeError, ServerStatus, SyncKind, SyncOutcome, lyrics_from_dto};
pub use server_url::{is_private_host, normalize_server_url};
