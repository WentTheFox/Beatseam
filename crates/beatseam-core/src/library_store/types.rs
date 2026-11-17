use std::fmt;

use crate::metadata::{CoverArtBytes, TrackMetadata};

#[derive(Clone, Debug, PartialEq)]
pub struct TrackRecord {
    /// Matches `FileRef::id`. For a missing track (see `missing`) this is a synthetic id derived from root id and path
    /// instead - never a real `FileRef::id`, since no file exists to have one.
    pub file_id: String,
    pub root_id: String,
    pub relative_path: String,
    pub size_bytes: u64,
    pub last_modified_ms: f64,
    /// True for a placeholder standing in for a playlist entry that a scan couldn't resolve to a real file
    /// (`relative_path` is the path it expected to find, for display and for "locate this file" to search from) -
    /// without this, an unresolved entry used to just silently vanish from the playlist. Never true for a track that
    /// resolved fine at scan time and later failed to *decode* at play time - that's a playback-time-only condition,
    /// not persisted here.
    pub missing: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistRecord {
    pub id: String,
    pub root_id: String,
    pub file_id: String,
    pub name: String,
    /// Ordered list of track file ids, resolved from the m3u8's entries.
    pub track_file_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnalysisResult {
    pub file_id: String,
    /// Gain multiplier to apply so this track matches the reference loudness target.
    pub normalization_gain: f64,
    pub analyzed_at_ms: f64,
    /// The track record's size/modified time at analysis time - a file id alone isn't a content identity (it's derived
    /// from the file's path, so a file edited in place keeps the same id), so freshness checks compare these against the
    /// current track record to detect changed files.
    pub size_bytes: u64,
    pub last_modified_ms: f64,
    /// `ANALYSIS_ALGORITHM_VERSION` at analysis time - a mismatch means the algorithm changed since, so the result is
    /// stale even though the file itself didn't.
    pub algorithm_version: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LoopMode {
    #[default]
    Off,
    All,
    One,
}

/// A folder to scan for `.lrc` lyrics files, expressed as a subfolder of an ALREADY-granted root rather than a root of
/// its own - `relative_path: ""` means the whole root. Requesting a brand-new top-level OS grant just for lyrics hit a
/// real-world wall (a broken Samsung "My Files" picker rejected every folder), so lyrics are scoped to a subfolder of a
/// root the user already granted for music, which never needed that picker.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LyricsScope {
    pub root_id: String,
    pub relative_path: String,
}

/// Which playlist screen the user last had open, which may not be the playing playlist (browsing playlist B while a track
/// from playlist A keeps playing).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum OpenedPlaylist {
    /// State persisted before this was tracked: treated as "not yet navigated away from the playing playlist", so
    /// restore falls back to the playing playlist's screen.
    #[default]
    Unset,
    /// The user was explicitly on the Library screen: restore leaves the screen at its default instead of forcing the
    /// playing playlist's screen open.
    Library,
    Playlist(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackState {
    /// The playlist the CURRENTLY PLAYING track belongs to - a separate field from "the playlist screen that's open".
    pub playlist_id: Option<String>,
    pub current_track_file_id: Option<String>,
    /// Which granted root `playlist_id` belongs to - lets startup restore go straight to the right root without checking
    /// every granted root first. `None` for state persisted before this field existed.
    pub root_id: Option<String>,
    pub opened_playlist_id: OpenedPlaylist,
    /// Which granted root the opened playlist belongs to - same role/fallback as `root_id`.
    pub opened_root_id: Option<String>,
    pub position_seconds: f64,
    pub loop_mode: LoopMode,
    pub shuffle_enabled: bool,
    /// The shuffle order as file ids, so a relaunch resumes the exact same shuffled sequence instead of silently
    /// re-shuffling. `None` when shuffle is off, or for state persisted before this field existed.
    pub shuffle_order: Option<Vec<String>>,
    /// User-facing master volume `[0, 1]`. Persisted so the next launch doesn't blast out at whatever volume happened to
    /// be in effect before it's set once.
    pub volume: f64,
    /// Whether the Now Playing screen was showing when last closed.
    pub now_playing_open: bool,
}

/// A failure in the persistence layer (disk full, corrupt database, ...).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreError(pub String);

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StoreError {}

/// A track's display name - just its filename, not the full relative path (which includes the folder structure within
/// the granted root).
pub fn track_display_name(track: &TrackRecord) -> &str {
    track.relative_path.rsplit('/').next().unwrap_or(&track.relative_path)
}

/// The persisted library. Implemented over `rusqlite` on desktop and Android, SQLite-wasm on web, and in memory for
/// tests ([`MemoryLibraryStore`](super::MemoryLibraryStore)). The futures need not be `Send`.
pub trait LibraryStore {
    fn upsert_track(&self, track: TrackRecord) -> impl Future<Output = Result<(), StoreError>>;
    /// Removes a single track row outright - used by a scan to clean up a stale row it has just superseded (e.g. a
    /// `missing:`-prefixed placeholder once the real file resolves, or the reverse once a previously-real file goes
    /// missing). That can't just be another `upsert_track`: the old and new rows have different file ids for the same
    /// relative path, so upserting the new one alone leaves the old one orphaned. Not used for an "Unplaylisted" track
    /// that simply stops being referenced by any playlist - that's an intentional, still-valid row.
    fn delete_track(&self, file_id: &str) -> impl Future<Output = Result<(), StoreError>>;
    fn upsert_playlist(&self, playlist: PlaylistRecord) -> impl Future<Output = Result<(), StoreError>>;
    /// Counterpart to `delete_track`, for the same reason - used to clean up a root's playlist rows once it's removed.
    fn delete_playlist(&self, id: &str) -> impl Future<Output = Result<(), StoreError>>;
    fn list_tracks(&self, root_id: &str) -> impl Future<Output = Result<Vec<TrackRecord>, StoreError>>;
    fn list_playlists(&self, root_id: &str) -> impl Future<Output = Result<Vec<PlaylistRecord>, StoreError>>;
    /// Every track row across every root, regardless of whether that root is still granted - used to discover a removed
    /// root's leftover data (which can't be driven by the currently-granted roots list alone).
    fn list_all_tracks(&self) -> impl Future<Output = Result<Vec<TrackRecord>, StoreError>>;
    /// Counterpart to `list_all_tracks`.
    fn list_all_playlists(&self) -> impl Future<Output = Result<Vec<PlaylistRecord>, StoreError>>;

    fn get_analysis(&self, file_id: &str) -> impl Future<Output = Result<Option<AnalysisResult>, StoreError>>;
    fn put_analysis(&self, result: AnalysisResult) -> impl Future<Output = Result<(), StoreError>>;

    fn get_metadata(&self, file_id: &str) -> impl Future<Output = Result<Option<TrackMetadata>, StoreError>>;
    fn put_metadata(&self, result: TrackMetadata) -> impl Future<Output = Result<(), StoreError>>;

    /// A URI ready to hand straight to an image view for the cover art extracted alongside a track's metadata - `None`
    /// when the file has none, or hasn't been scanned yet (indistinguishable here). What kind of URI it is is up to the
    /// adapter: a `data:` URI where storage is text-based, a `blob:` object URL on web.
    fn get_cover_art(&self, file_id: &str) -> impl Future<Output = Result<Option<String>, StoreError>>;
    /// `None` clears any previously stored art (e.g. the file changed and no longer has any). Raw bytes, not a
    /// pre-encoded string - each adapter decides its own storage/URI representation.
    fn put_cover_art(&self, file_id: &str, art: Option<CoverArtBytes>) -> impl Future<Output = Result<(), StoreError>>;

    fn get_playback_state(&self) -> impl Future<Output = Result<Option<PlaybackState>, StoreError>>;
    fn put_playback_state(&self, state: PlaybackState) -> impl Future<Output = Result<(), StoreError>>;

    /// Every configured lyrics scope, across every root.
    fn get_lyrics_scopes(&self) -> impl Future<Output = Result<Vec<LyricsScope>, StoreError>>;
    /// No-ops if this exact (root, path) pair is already present.
    fn add_lyrics_scope(&self, scope: LyricsScope) -> impl Future<Output = Result<(), StoreError>>;
    fn remove_lyrics_scope(&self, root_id: &str, relative_path: &str) -> impl Future<Output = Result<(), StoreError>>;

    /// The `.lrc` file (a `FileRef::id` from a lyrics scope) assigned to this track - `None` if none. Set either by
    /// auto-match or a manual override; both go through this same call.
    fn get_lyrics_assignment(&self, file_id: &str) -> impl Future<Output = Result<Option<String>, StoreError>>;
    fn put_lyrics_assignment(&self, file_id: &str, lrc_file_id: Option<String>) -> impl Future<Output = Result<(), StoreError>>;

    /// Generic string key-value storage for small, standalone bits of app state that don't warrant their own table (e.g.
    /// the last known lyrics-match count). Callers `serde_json` anything structured; kept to strings here so every
    /// adapter can store it identically without a schema per key.
    fn get_setting(&self, key: &str) -> impl Future<Output = Result<Option<String>, StoreError>>;
    fn put_setting(&self, key: &str, value: &str) -> impl Future<Output = Result<(), StoreError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(path: &str) -> TrackRecord {
        TrackRecord { file_id: path.into(), root_id: "r".into(), relative_path: path.into(), size_bytes: 1, last_modified_ms: 0.0, missing: false }
    }

    #[test]
    fn the_display_name_is_the_filename() {
        assert_eq!(track_display_name(&track("Artist/Album/Song.mp3")), "Song.mp3");
        assert_eq!(track_display_name(&track("Song.mp3")), "Song.mp3");
        assert_eq!(track_display_name(&track("")), "");
        assert_eq!(track_display_name(&track("dir/")), "");
    }
}
