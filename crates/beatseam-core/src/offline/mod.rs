//! Port of `packages/core/src/offline`: tracks kept on the device for offline play.

mod offline_library;
mod types;

pub use offline_library::{OfflineLibrary, OfflineOptions, decode_offline_index, encode_offline_index};
pub use types::*;

#[cfg(test)]
mod tests;
