use std::fmt;

use crate::file_access::FileAccessError;
use crate::library_store::StoreError;

/// Why a scan or other library operation stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanError {
    /// The caller's `AbortSignal` fired mid-scan - a distinct variant (rather than a message to match on) so callers can
    /// tell "the user cancelled this" apart from a real failure. The TypeScript's `ScanCancelledError`.
    Cancelled,
    FileAccess(FileAccessError),
    Store(StoreError),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("Scan cancelled"),
            Self::FileAccess(e) => e.fmt(f),
            Self::Store(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ScanError {}

impl From<FileAccessError> for ScanError {
    fn from(e: FileAccessError) -> Self {
        Self::FileAccess(e)
    }
}

impl From<StoreError> for ScanError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}
