use std::fmt;
use std::future::Future;

use crate::tasks::AbortSignal;

/// Why a backend operation failed (disk full, permission, ...).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendError(pub String);

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BackendError {}

/// Why a download did not produce a file. The backend sees the HTTP exchange, so it does the mapping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownloadError {
    /// The server can't be reached (offline, down, no answer in time): not this track's fault, so the queue pauses and
    /// keeps its place.
    Network,
    /// The server answered 401: the sign-in lapsed. The queue pauses too.
    Unauthorized,
    /// The server answered 404: the track is gone from it.
    NotFound,
    /// Anything else (a 5xx, a disk error, a hash mismatch): retried a few times, then reported.
    Other(String),
}

/// What a backend needs to fetch one track.
pub struct DownloadRequest<'a> {
    pub file_id: &'a str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub signal: AbortSignal,
    /// Bytes received so far.
    pub on_progress: &'a dyn Fn(u64),
}

/// What a finished download stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Downloaded {
    pub size_bytes: u64,
    pub sha256: String,
}

/// Where downloaded audio lives on a platform. The backend does the transfer itself - streaming it to disk and hashing it
/// where it can do so natively - so the app never holds or hashes a whole track just to save it. Writes must be atomic: a
/// file that exists is a complete file.
pub trait OfflineBackend {
    /// Fetches `request.url` into storage under `request.file_id`, replacing any earlier copy, and reports what it stored.
    fn download(&self, request: DownloadRequest<'_>) -> impl Future<Output = Result<Downloaded, DownloadError>>;
    /// The stored bytes, or `None` if there are none.
    fn read(&self, file_id: &str) -> impl Future<Output = Result<Option<Vec<u8>>, BackendError>>;
    fn remove(&self, file_id: &str) -> impl Future<Output = Result<(), BackendError>>;
    /// Everything currently stored, as (file id, size in bytes).
    fn list(&self) -> impl Future<Output = Result<Vec<(String, u64)>, BackendError>>;
}

/// Where to fetch a track from: the signed-in server. `None` when not signed in.
pub struct DownloadTarget {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

pub trait OfflineSource {
    fn download_target(&self, file_id: &str) -> Option<DownloadTarget>;
}

/// Spawning, timers and the clock, supplied by the host (the library itself is runtime-free).
pub use crate::tasks::Runtime as OfflineRuntime;

/// A track kept for offline play.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfflineRecord {
    pub file_id: String,
    pub size_bytes: u64,
    /// What the server said the file hashes to (and what was verified on arrival); a change on the server makes this copy
    /// stale.
    pub sha256: String,
    pub downloaded_at_ms: u64,
    pub last_used_at_ms: u64,
    /// Asked for explicitly (a playlist made available offline); never evicted automatically.
    pub pinned: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfflineFailure {
    pub file_id: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfflineState {
    pub downloaded_count: usize,
    pub downloaded_bytes: u64,
    /// Waiting for a turn (not counting those in flight).
    pub queued_count: usize,
    pub active_count: usize,
    /// Of those queued or in flight, how many the person asked for (a download or a playlist made available offline), as
    /// opposed to copies kept quietly because they were played. What a progress notification should be about.
    pub requested_count: usize,
    /// Offline copies being deleted right now: (done, total); `None` when nothing is being removed.
    pub removing: Option<(usize, usize)>,
    /// Bytes received so far for each download in flight.
    pub progress: std::collections::BTreeMap<String, u64>,
    /// 0 means no limit. Unpinned copies are evicted, least recently played first, to stay under it.
    pub budget_bytes: u64,
    /// Tracks are kept (unpinned) as they are played from the server, instead of being streamed and forgotten.
    pub cache_on_play: bool,
    /// Not transferring: the server can't be reached (or the sign-in lapsed). Resumes on its own when it can.
    pub paused: bool,
    /// Recent downloads that gave up, newest last.
    pub failures: Vec<OfflineFailure>,
}

/// What the offline library needs to know of the server connection (the TypeScript subscribed to it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerStatusSummary {
    pub offline: bool,
    pub active: bool,
    pub last_synced_at_ms: Option<u64>,
}
