//! Desktop and Android services for `beatseam-core`: file access over `std::fs` and a SQLite library store.

mod fs_access;
mod sqlite_store;

pub use fs_access::StdFileAccess;
pub use sqlite_store::SqliteLibraryStore;
