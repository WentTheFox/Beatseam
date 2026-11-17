use std::fmt;

use crate::lyrics::ParsedLyrics;

/// Opaque handle to a file inside a user-granted root directory. Shape is intentionally minimal - platform adapters
/// attach whatever native handle/URI they need internally and expose only this surface.
#[derive(Clone, Debug, PartialEq)]
pub struct FileRef {
    /// Stable within a root grant; used as the identity key for library/analysis rows.
    pub id: String,
    pub name: String,
    /// Path relative to the root directory that was granted, for display and m3u8 resolution.
    pub relative_path: String,
    pub size_bytes: u64,
    pub last_modified_ms: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EntryKind {
    File(FileRef),
    Directory,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DirectoryEntry {
    pub name: String,
    /// Path relative to the granted root; pass to `list_directory`/`read_file_*` to descend or read.
    pub relative_path: String,
    pub kind: EntryKind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RootKind {
    #[default]
    Library,
    /// A root granted purely to hold `.lrc` files - a refresh must never scan one for playlists/tracks, so picking an
    /// independent lyrics folder through a real directory picker doesn't leave a phantom empty "library" behind.
    Lyrics,
}

/// One user-granted root (a folder the user picked, which may contain playlists and audio in any nested structure).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantedRoot {
    pub id: String,
    pub display_name: String,
    pub kind: RootKind,
    /// False for a root the user can't actually remove - currently just the self-hosted server's operator-mounted roots
    /// (the volume mount is what grants it, so removing it from the UI would either do nothing or, worse, look like it
    /// worked while the folder reappears on the next refresh). Lets the UI hide the remove control instead of offering
    /// an action that can't do what it says.
    pub removable: bool,
}

impl GrantedRoot {
    pub fn new(id: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self { id: id.into(), display_name: display_name.into(), kind: RootKind::Library, removable: true }
    }
}

/// Per-call options for the read operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallOptions {
    /// Default true. When false, the call must never prompt the user (no `requestPermission()`-equivalent) - it's
    /// running from idle/background scheduling rather than a real user gesture, and browsers reject an attempt to prompt
    /// outside one. Adapters with no such permission model (Android, desktop, the server backend) ignore this entirely -
    /// only the web adapter's `showDirectoryPicker`-backed grants have teeth here. See [`BackgroundFileAccess`](super::BackgroundFileAccess).
    pub allow_prompt: bool,
}

impl Default for CallOptions {
    fn default() -> Self {
        Self { allow_prompt: true }
    }
}

impl CallOptions {
    pub const BACKGROUND: Self = Self { allow_prompt: false };
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileAccessError {
    /// Returned instead of prompting when a call is marked `allow_prompt: false` and the underlying grant needs
    /// re-confirming.
    PermissionPending { root_display_name: String },
    /// No root with that id is granted (it may have been revoked).
    UnknownRoot(String),
    /// The file or directory is gone.
    NotFound(String),
    /// This adapter can't do that (e.g. writing on the server-backed adapter).
    Unsupported(String),
    Io(String),
}

impl fmt::Display for FileAccessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PermissionPending { root_display_name } => write!(f, "Read permission for \"{root_display_name}\" needs to be re-granted, but this call isn't allowed to prompt for it."),
            Self::UnknownRoot(id) => write!(f, "No library root \"{id}\""),
            Self::NotFound(what) => write!(f, "Not found: {what}"),
            Self::Unsupported(what) => f.write_str(what),
            Self::Io(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for FileAccessError {}

/// What a file source says when asked for a track's lyrics directly (see [`FileAccess::load_lyrics`]).
#[derive(Clone, Debug, PartialEq)]
pub enum LyricsAnswer {
    /// This source has no say; the caller finds and parses `.lrc` files the file way.
    NoSay,
    /// The source says there are no lyrics.
    NoLyrics,
    Lyrics(ParsedLyrics),
}

/// The single, centralized point of contact for touching a user's files on every platform (desktop, Android, web, and
/// the self-hosted server) - every concrete adapter implements exactly this surface and nothing more.
///
/// Read-only apart from one deliberate exception, [`write_file_text`](Self::write_file_text), added for
/// playlist-from-folder generation. Adapters whose underlying permission was requested read-only (the web adapter's
/// `showDirectoryPicker({ mode: 'read' })`) request write access lazily, only when that is actually called, rather than
/// widening every root's grant because one feature occasionally needs to write. The server adapter doesn't support
/// writing and says so with [`FileAccessError::Unsupported`] rather than silently doing nothing.
///
/// The futures need not be `Send`: the same trait serves the single-threaded wasm adapter and the multi-threaded
/// native ones.
pub trait FileAccess {
    /// Prompts the platform's directory picker and persists the grant. `None` if the person cancelled.
    fn request_root(&self, kind: RootKind) -> impl Future<Output = Result<Option<GrantedRoot>, FileAccessError>>;
    /// Roots granted in a previous session, restored without re-prompting.
    fn list_granted_roots(&self) -> impl Future<Output = Result<Vec<GrantedRoot>, FileAccessError>>;
    fn revoke_root(&self, root_id: &str) -> impl Future<Output = Result<(), FileAccessError>>;

    /// Lists one level (immediate children only) of a directory; callers recurse via
    /// [`walk_directory`](crate::library_scan::walk_directory). `relative_path` is `None` for the root itself (some
    /// adapters key their root entry by "no path" specifically, so this is not the same as `Some("")`).
    fn list_directory(&self, root_id: &str, relative_path: Option<&str>, opts: CallOptions) -> impl Future<Output = Result<Vec<DirectoryEntry>, FileAccessError>>;

    fn read_file_bytes(&self, file: &FileRef, opts: CallOptions) -> impl Future<Output = Result<Vec<u8>, FileAccessError>>;
    fn read_file_text(&self, file: &FileRef, opts: CallOptions) -> impl Future<Output = Result<String, FileAccessError>>;

    /// Creates (or overwrites) a text file at `relative_path` under `root_id`. The parent directory must already exist;
    /// this does not create missing intermediate directories (every call site writes into a folder it just finished
    /// walking).
    fn write_file_text(&self, root_id: &str, relative_path: &str, contents: &str) -> impl Future<Output = Result<(), FileAccessError>>;

    /// When present, an absolute, same-origin HTTP(S) URL the file's bytes can be fetched from with Range support -
    /// used by metadata reading to fetch only the small header/footer chunks tag parsing needs instead of the whole
    /// file. Only the server-backed adapter has one (its files are already served over HTTP); every other adapter reads
    /// local storage and returns `None`.
    fn stream_url(&self, _file: &FileRef) -> Option<String> {
        None
    }

    /// Hands over a track's lyrics ready-made instead of leaving the caller to find and parse `.lrc` files - the
    /// server-backed source, where the server already matched and parsed them.
    fn load_lyrics(&self, _track_file_id: &str) -> impl Future<Output = Result<LyricsAnswer, FileAccessError>> {
        async { Ok(LyricsAnswer::NoSay) }
    }

    /// A real content hash for `file` computed WITHOUT the caller having to download it (the server hashes its own
    /// local copy and returns just the digest). Every other adapter already has the full bytes whenever it needs a hash
    /// and returns `None`.
    fn content_hash(&self, _file: &FileRef) -> impl Future<Output = Result<Option<String>, FileAccessError>> {
        async { Ok(None) }
    }
}
