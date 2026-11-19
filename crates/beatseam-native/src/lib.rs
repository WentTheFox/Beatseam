//! Desktop and Android services for `beatseam-core`: file access over `std::fs` and a SQLite library store.
#![allow(clippy::type_complexity, clippy::arc_with_non_send_sync, clippy::too_many_arguments)] // see beatseam-core's lib.rs

mod fs_access;
mod sqlite_store;

pub use fs_access::StdFileAccess;
pub use sqlite_store::SqliteLibraryStore;
