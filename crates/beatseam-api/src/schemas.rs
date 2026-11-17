//! Library, lyrics, paging and scan-status DTOs (`packages/api/src/schemas.ts`).
//!
//! Number types follow the data: counts and byte sizes are integers; anything that was a float or a timestamp on the
//! TypeScript server (`lastModifiedMs` is a Postgres `Float`, so it can be fractional) is `f64`, so a value read from the
//! server and sent back (or compared) is bit-identical.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The API version this build speaks, and the one a client sends. Bumped only for a breaking change - see
/// `docs/api-compatibility.md` before touching it (or anything else on the wire).
pub const API_VERSION: u32 = 1;
/// The versions a server built from this code answers. Normally just `[API_VERSION]`; while a deprecation window is
/// open it also lists the previous version, so apps that haven't updated yet keep working.
pub const SUPPORTED_API_VERSIONS: &[u32] = &[1];
/// Request header clients send with the version they speak. The server answers 426 when it can't serve that version.
pub const API_VERSION_HEADER: &str = "X-Beatseam-Api";
/// Response header on every /api/v1 answer: the versions the server speaks, comma-separated. Servers from before it
/// existed omit it, and speak only version 1.
pub const API_VERSIONS_HEADER: &str = "X-Beatseam-Api-Versions";
/// The `code` of the 426 a server answers with when it doesn't speak the client's version.
pub const API_VERSION_UNSUPPORTED_CODE: &str = "api_version_unsupported";

pub const MAX_PAGE_SIZE: u32 = 500;
pub const MAX_LYRICS_LINES: usize = 5000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RootKind {
    Library,
    Lyrics,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RootDto {
    pub id: String,
    pub display_name: String,
    pub kind: RootKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackDto {
    /// Opaque key for URLs - see `encode_track_key`.
    pub id: String,
    pub root_id: String,
    pub relative_path: String,
    pub size_bytes: u64,
    pub last_modified_ms: f64,
    /// A playlist referenced this path but the file isn't on disk.
    pub missing: bool,
    pub title: Option<String>,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_seconds: Option<f64>,
    /// sha256 (hex) of the file once scanned; lets a client verify a download or detect a changed file.
    pub content_hash: Option<String>,
    /// Linear gain that brings the track to the reference loudness, once analysed.
    pub normalization_gain: Option<f64>,
    pub has_cover: bool,
    pub has_lyrics: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistDto {
    pub id: String,
    pub root_id: String,
    pub name: String,
    /// Track keys in play order (duplicates allowed).
    pub track_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricLineDto {
    pub time_seconds: Option<f64>,
    pub text: String,
    /// The matching line from a sibling translation file. `null` and absent both mean "none": the TypeScript server
    /// sends `null` for a timed line a merged translation had no match for and omits the key otherwise, and every
    /// reader treats them alike, so this port omits it whenever it is `None` (a harmless, compatible difference for
    /// old apps, whose schema is `nullable().optional()`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translation: Option<String>,
}

/// A track's lyrics, parsed (and matched with its translation) by the server: same shape as the app's `ParsedLyrics`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricsDto {
    /// At least one line has a timestamp; false means plain text, one line per entry.
    pub synced: bool,
    pub tags: BTreeMap<String, String>,
    pub lines: Vec<LyricLineDto>,
}

/// A lyrics file the server found in its lyrics folders: what "Link a lyrics file" lets an admin choose from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricsFileDto {
    pub id: String,
    pub name: String,
    pub relative_path: String,
    pub root_id: String,
}

/// Where a track's lyrics come from: the file assigned to it (`None` = none) and every file it could be assigned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricsSourceDto {
    pub assigned: Option<String>,
    pub candidates: Vec<LyricsFileDto>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricsAssignRequestDto {
    pub lrc_file_id: Option<String>,
}

/// "These lyrics are wrong": any signed-in user can flag a track's lyrics for an admin to look at.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricsReportRequestDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricsReportDto {
    pub id: String,
    /// The track's key, as the other routes take it.
    pub track_id: String,
    /// Where the track was when it was reported (it may be gone since).
    pub relative_path: String,
    pub reported_by: String,
    pub reason: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricsSaveLineDto {
    pub time_seconds: Option<f64>,
    pub text: String,
}

/// The lines of a hand-synced or hand-edited lyrics file, written back to the track's assigned .lrc.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricsSaveRequestDto {
    pub lines: Vec<LyricsSaveLineDto>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

/// `GET /api/v1/tracks` query. Serialized to a query string in field order, skipping `None`s.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListTracksQuery {
    pub root_id: Option<String>,
    /// Case-insensitive match against title, artist, album and path.
    pub q: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

impl ListTracksQuery {
    pub fn to_pairs(&self) -> Vec<(&'static str, String)> {
        let mut pairs = Vec::new();
        if let Some(v) = &self.root_id {
            pairs.push(("rootId", v.clone()));
        }
        if let Some(v) = &self.q {
            pairs.push(("q", v.clone()));
        }
        if let Some(v) = &self.cursor {
            pairs.push(("cursor", v.clone()));
        }
        if let Some(v) = self.limit {
            pairs.push(("limit", v.to_string()));
        }
        pairs
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobCountsDto {
    pub pending: u64,
    /// Pending jobs that already failed once and are waiting out their backoff (included in `pending`).
    pub retrying: u64,
    pub running: u64,
    pub done: u64,
    /// Failed every attempt; left alone until the file changes or someone retries them.
    pub skipped: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanLastDto {
    pub started_at_ms: f64,
    pub finished_at_ms: f64,
    pub roots: u64,
    pub tracks: u64,
    pub queued: u64,
    pub jobs: JobCountsDto,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanWorkersDto {
    pub workers: u64,
    /// The counters below are since this server process started.
    pub succeeded: u64,
    pub failed_attempts: u64,
    pub skipped: u64,
    pub in_flight: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanStatusDto {
    pub running: bool,
    pub last: Option<ScanLastDto>,
    pub jobs: JobCountsDto,
    pub workers: ScanWorkersDto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum JobStateDto {
    Pending,
    Running,
    Done,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanJobDto {
    pub track_id: String,
    /// `None` if the track has since been removed.
    pub relative_path: Option<String>,
    pub state: JobStateDto,
    pub attempts: u64,
    pub last_error: Option<String>,
    pub last_error_stage: Option<String>,
    pub skipped_at_ms: Option<f64>,
    pub updated_at_ms: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanFailureDto {
    pub id: u64,
    pub track_id: String,
    pub relative_path: Option<String>,
    pub stage: String,
    pub attempt: u64,
    pub error: String,
    pub will_retry: bool,
    pub worker_id: String,
    pub at_ms: f64,
}
