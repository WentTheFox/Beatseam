use std::fmt;

use crate::file_access::FileAccessError;
use crate::library_store::StoreError;

/// Why a scan or other library operation stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LibraryError {
    /// The caller's `AbortSignal` fired mid-scan - a distinct variant (rather than a message to match on) so callers can
    /// tell "the user cancelled this" apart from a real failure. The TypeScript's `ScanCancelledError`.
    Cancelled,
    FileAccess(FileAccessError),
    Store(StoreError),
    /// A playlist's `file_id` no longer matches any `.m3u8` actually found under its root - it was moved or deleted
    /// since the last scan.
    PlaylistFileNotFound { playlist_name: String },
    /// A playlist move was asked to move zero tracks, or to anchor a track on itself, or its anchor is gone.
    InvalidPlaylistMove(String),
}

impl fmt::Display for LibraryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("Scan cancelled"),
            Self::FileAccess(e) => e.fmt(f),
            Self::Store(e) => e.fmt(f),
            Self::PlaylistFileNotFound { playlist_name } => write!(f, "Could not find the playlist file for \"{playlist_name}\" - it may have been moved, renamed, or deleted since the last scan."),
            Self::InvalidPlaylistMove(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for LibraryError {}

impl From<FileAccessError> for LibraryError {
    fn from(e: FileAccessError) -> Self {
        Self::FileAccess(e)
    }
}

impl From<StoreError> for LibraryError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}
