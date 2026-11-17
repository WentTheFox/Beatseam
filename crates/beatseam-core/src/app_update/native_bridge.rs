use std::fmt;

/// The platform failed to do what was asked (no disk space, the installer refused, the user denied a permission).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeError(pub String);

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BridgeError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadedFile {
    pub path: String,
    /// SHA-256 (lowercase hex) computed while streaming the file to disk.
    pub sha256: String,
}

/// Platform-specific half of the update flow (downloading a release asset to a real file, and triggering the OS package
/// installer on it) - the version check itself ([`check_for_update`](super::check_for_update)) is an HTTP request plus
/// string comparison and needs no native code, but this part does. Only Android implements it for real (the Kotlin
/// `AppUpdateModule`); other platforms have no APK/install concept, so the update UI is only ever driven with a bridge
/// there in the first place.
pub trait AppUpdateNativeBridge {
    /// Downloads `url` to a private, app-managed location named `file_name`, returning its path and the SHA-256
    /// computed while streaming it to disk.
    fn download_file(&self, url: &str, file_name: &str) -> impl Future<Output = Result<DownloadedFile, BridgeError>>;
    /// Launches the OS package installer for a file `download_file` already saved.
    fn install_apk(&self, file_path: &str) -> impl Future<Output = Result<(), BridgeError>>;
    /// Whether this app is currently allowed to trigger a package install (Android 8+'s per-app "install unknown apps"
    /// grant).
    fn can_request_install_packages(&self) -> impl Future<Output = Result<bool, BridgeError>>;
    /// Opens the system settings screen for granting [`can_request_install_packages`](Self::can_request_install_packages)
    /// - fire-and-forget; the caller re-checks it once the app resumes.
    fn request_install_permission(&self) -> impl Future<Output = Result<(), BridgeError>>;
}
