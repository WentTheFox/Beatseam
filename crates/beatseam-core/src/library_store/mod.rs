//! Port of `packages/core/src/library-store`: the persisted library, and an in-memory store for tests and fixtures.

mod memory;
mod types;

pub use memory::MemoryLibraryStore;
pub use types::{AnalysisResult, LibraryStore, LoopMode, LyricsScope, OpenedPlaylist, PlaybackState, PlaylistRecord, StoreError, TrackRecord, track_display_name};
