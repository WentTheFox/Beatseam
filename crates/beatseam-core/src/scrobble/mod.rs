//! Port of `packages/core/src/scrobble`: Last.fm signing and calls, the offline scrobble queue, the play tracker.

mod connection_status;
mod lastfm;
mod md5;
mod scrobble_queue;
mod scrobble_sink;
mod scrobble_tracker;

pub use connection_status::{LastFmAttemptStatus, LastFmConnectionStatus, derive_last_fm_status};
pub use lastfm::*;
pub use md5::md5;
pub use scrobble_queue::{MAX_QUEUED_SCROBBLES, append_to_scrobble_queue, decode_scrobble_queue, encode_scrobble_queue};
pub use scrobble_sink::{LOCAL_SCROBBLE_QUEUE_KEY, LocalScrobbleSink, NowPlayingTrack, ScrobbleSink};
pub use scrobble_tracker::{ScrobbleTracker, ScrobbleTrackerCallbacks, ScrobbleTrackInfo};
