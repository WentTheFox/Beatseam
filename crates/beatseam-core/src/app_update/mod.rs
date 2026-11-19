//! Port of `packages/core/src/appUpdate`: the GitHub Releases update check and the platform bridge for installing.

mod check_for_update;
mod native_bridge;

pub use check_for_update::{AvailableUpdate, GITHUB_REPO, GITHUB_REPO_URL, ReleaseAsset, UpdateCheckError, check_for_update, parse_checksums_file};
pub use native_bridge::{AppUpdateNativeBridge, BridgeError, DownloadedFile};
