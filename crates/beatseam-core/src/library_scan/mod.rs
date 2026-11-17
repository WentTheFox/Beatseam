//! Port of `packages/core/src/library-scan`.

mod audio_files;
mod error;
mod scan;
mod walk;

pub use audio_files::is_audio_file_name;
pub use error::ScanError;
pub use scan::{ScanProgress, ScanPhase, ScanResult, UnresolvedEntry, describe_unresolved_entries, missing_track_id, scan_root};
pub use walk::{WalkProgress, WalkResult, walk_directory};
