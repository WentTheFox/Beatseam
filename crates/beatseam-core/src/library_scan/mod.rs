//! Port of `packages/core/src/library-scan`.

mod add_tracks_to_playlist;
mod audio_files;
mod create_playlist_from_folder;
mod delete_root_data;
mod edit_playlist_tracks;
mod error;
mod find_unplaylisted_tracks;
mod load_root_library;
mod relocate_missing_track;
mod scan;
mod scan_coordinator;
mod walk;

pub use add_tracks_to_playlist::{AddPosition, add_tracks_to_playlist};
pub use audio_files::is_audio_file_name;
pub use create_playlist_from_folder::{PlaylistCandidateTrack, PlaylistSortCriterion, PlaylistSortOrder, find_playlist_candidate_tracks, sort_playlist_candidates, write_playlist_file};
pub use delete_root_data::{delete_root_library_data, prune_orphaned_root_data};
pub use edit_playlist_tracks::{MovePosition, find_playlist_file, move_playlist_tracks, remove_playlist_tracks};
pub use error::LibraryError;
pub use find_unplaylisted_tracks::{UnplaylistedTracks, find_unplaylisted_tracks, find_unplaylisted_tracks_in_store, find_unplaylisted_tracks_queued};
pub use load_root_library::{LoadRootLibraryOptions, LoadedRoot, load_root_library};
pub use relocate_missing_track::relocate_missing_track;
pub use scan_coordinator::{ScanCoordinator, ScanSubscription, describe_scan_progress};
pub use scan::{ScanPhase, ScanProgress, ScanResult, UnresolvedEntry, describe_unresolved_entries, missing_track_id, scan_root};
pub use walk::{MAX_CONCURRENT_LISTINGS, WalkProgress, WalkResult, walk_directory};
