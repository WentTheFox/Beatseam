//! Port of `packages/core/src/file-access`: the single seam through which every platform touches a user's files.

mod background;
mod memory;
mod types;

pub use background::BackgroundFileAccess;
pub use memory::MemoryFileAccess;
pub use types::{CallOptions, DirectoryEntry, EntryKind, FileAccess, FileAccessError, FileRef, GrantedRoot, LyricsAnswer, RootKind};
