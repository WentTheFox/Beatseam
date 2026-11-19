//! File access over `std::fs` for native platforms with plain paths (desktop; Android with all-files access).

use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use beatseam_core::file_access::{CallOptions, DirectoryEntry, EntryKind, FileAccess, FileAccessError, FileRef, GrantedRoot, RootKind};

struct Root {
    granted: GrantedRoot,
    path: PathBuf,
}

/// [`FileAccess`] over folders on disk. A "granted root" is a folder the host added with [`add_root`](Self::add_root) (after its own
/// folder picker, or from the command line) - there is no OS permission model to satisfy, so the background flag is irrelevant.
///
/// A file's id is `<root id>:<path relative to the root, with / separators>`: the same relative path in two roots never collides, and
/// the root is recoverable from an id alone (reads take only a [`FileRef`]).
///
/// The I/O is plain blocking `std::fs`; a host that must never block its UI thread runs the work on a worker.
#[derive(Default)]
pub struct StdFileAccess {
    roots: Mutex<Vec<Root>>,
}

/// A short, stable id for a folder: the same path gives the same id on every run, so persisted library rows keep pointing at it.
fn root_id_for(path: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.to_string_lossy().bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("root-{hash:08x}")
}

fn io_error(path: &str, error: std::io::Error) -> FileAccessError {
    match error.kind() {
        std::io::ErrorKind::NotFound => FileAccessError::NotFound(path.to_owned()),
        _ => FileAccessError::Io(format!("{path}: {error}")),
    }
}

impl StdFileAccess {
    pub fn new() -> Self {
        Self::default()
    }

    /// Grants a folder. Idempotent: adding the same folder again returns the root it already has. The display name defaults to the
    /// folder's own name.
    pub fn add_root(&self, path: &Path, display_name: Option<&str>, kind: RootKind) -> Result<GrantedRoot, FileAccessError> {
        let canonical = std::fs::canonicalize(path).map_err(|e| io_error(&path.display().to_string(), e))?;
        if !canonical.is_dir() {
            return Err(FileAccessError::Io(format!("{} is not a folder", path.display())));
        }
        // `canonicalize` on Windows yields a `\\?\` path; strip it so ids and display names stay readable.
        let clean = PathBuf::from(canonical.to_string_lossy().trim_start_matches(r"\\?\").to_owned());
        let id = root_id_for(&clean);
        let mut roots = self.roots.lock().unwrap();
        if let Some(existing) = roots.iter().find(|r| r.granted.id == id) {
            return Ok(existing.granted.clone());
        }
        let name = display_name.map(str::to_owned).or_else(|| clean.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_else(|| clean.display().to_string());
        let granted = GrantedRoot { id, display_name: name, kind, removable: true };
        roots.push(Root { granted: granted.clone(), path: clean });
        Ok(granted)
    }

    fn root_path(&self, root_id: &str) -> Result<PathBuf, FileAccessError> {
        self.roots.lock().unwrap().iter().find(|r| r.granted.id == root_id).map(|r| r.path.clone()).ok_or_else(|| FileAccessError::UnknownRoot(root_id.to_owned()))
    }

    /// The absolute path of a root-relative path. Anything that could leave the root (`..`, an absolute path, a drive prefix) is
    /// refused: playlists are untrusted text, and an entry like `../../secret` must not read outside the library.
    fn resolve(&self, root_id: &str, relative: &str) -> Result<PathBuf, FileAccessError> {
        let mut path = self.root_path(root_id)?;
        for component in Path::new(relative).components() {
            match component {
                Component::Normal(part) => path.push(part),
                Component::CurDir => {}
                _ => return Err(FileAccessError::NotFound(relative.to_owned())),
            }
        }
        Ok(path)
    }

    fn file_ref(root_id: &str, relative_path: &str, metadata: &std::fs::Metadata) -> FileRef {
        let modified_ms = metadata.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0.0, |d| d.as_secs_f64() * 1000.0);
        FileRef { id: format!("{root_id}:{relative_path}"), name: relative_path.rsplit('/').next().unwrap_or(relative_path).to_owned(), relative_path: relative_path.to_owned(), size_bytes: metadata.len(), last_modified_ms: modified_ms }
    }

    fn path_of(&self, file: &FileRef) -> Result<PathBuf, FileAccessError> {
        let (root_id, _) = file.id.split_once(':').ok_or_else(|| FileAccessError::NotFound(file.id.clone()))?;
        self.resolve(root_id, &file.relative_path)
    }
}

impl FileAccess for StdFileAccess {
    /// A command line has no folder picker: the host adds roots with [`StdFileAccess::add_root`] instead.
    async fn request_root(&self, _kind: RootKind) -> Result<Option<GrantedRoot>, FileAccessError> {
        Ok(None)
    }

    async fn list_granted_roots(&self) -> Result<Vec<GrantedRoot>, FileAccessError> {
        Ok(self.roots.lock().unwrap().iter().map(|r| r.granted.clone()).collect())
    }

    async fn revoke_root(&self, root_id: &str) -> Result<(), FileAccessError> {
        self.roots.lock().unwrap().retain(|r| r.granted.id != root_id);
        Ok(())
    }

    async fn list_directory(&self, root_id: &str, relative_path: Option<&str>, _opts: CallOptions) -> Result<Vec<DirectoryEntry>, FileAccessError> {
        let relative = relative_path.unwrap_or("").trim_matches('/');
        let directory = self.resolve(root_id, relative)?;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&directory).map_err(|e| io_error(relative, e))? {
            // An entry that vanished mid-listing, or can't be statted (a broken link), is skipped rather than failing the folder.
            let Ok(entry) = entry else { continue };
            let Ok(metadata) = std::fs::metadata(entry.path()) else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = if relative.is_empty() { name.clone() } else { format!("{relative}/{name}") };
            let kind = if metadata.is_dir() { EntryKind::Directory } else { EntryKind::File(Self::file_ref(root_id, &child, &metadata)) };
            entries.push(DirectoryEntry { name, relative_path: child, kind });
        }
        // The OS lists in no particular order; a stable one keeps scans and screens deterministic.
        entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.name.cmp(&b.name)));
        Ok(entries)
    }

    async fn read_file_bytes(&self, file: &FileRef, _opts: CallOptions) -> Result<Vec<u8>, FileAccessError> {
        std::fs::read(self.path_of(file)?).map_err(|e| io_error(&file.relative_path, e))
    }

    async fn read_file_text(&self, file: &FileRef, _opts: CallOptions) -> Result<String, FileAccessError> {
        // Lossy, like every text file here: a stray byte in a playlist or lyrics file must not make the whole file unreadable.
        std::fs::read(self.path_of(file)?).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()).map_err(|e| io_error(&file.relative_path, e))
    }

    async fn write_file_text(&self, root_id: &str, relative_path: &str, contents: &str) -> Result<(), FileAccessError> {
        let path = self.resolve(root_id, relative_path)?;
        // Written beside the target and renamed over it, so a crash mid-write can't leave a playlist half-written.
        let staging = path.with_extension(format!("{}.beatseam-tmp", path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default()));
        std::fs::write(&staging, contents).map_err(|e| io_error(relative_path, e))?;
        std::fs::rename(&staging, &path).map_err(|e| {
            let _ = std::fs::remove_file(&staging);
            io_error(relative_path, e)
        })
    }
}

#[cfg(test)]
mod tests {
    use beatseam_core::http::block_on;
    use beatseam_core::library_scan::{load_root_library, scan_root};
    use beatseam_core::library_store::LibraryStore;

    use super::*;
    use crate::SqliteLibraryStore;

    /// A throwaway library on disk: `Music/` with two tracks and a playlist, plus a lyrics file.
    struct Library {
        dir: PathBuf,
    }

    impl Library {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("beatseam-native-fs-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("Music/Rock")).unwrap();
            std::fs::write(dir.join("Music/Mix.m3u8"), "Rock/b side.mp3\nRock/A Song.mp3\n").unwrap();
            std::fs::write(dir.join("Music/Rock/A Song.mp3"), b"audio-a").unwrap();
            std::fs::write(dir.join("Music/Rock/b side.mp3"), b"audio-bb").unwrap();
            std::fs::write(dir.join("Music/Rock/A Song.lrc"), "[00:01.00]Hello \u{e9}").unwrap();
            Self { dir }
        }

        fn access(&self) -> (StdFileAccess, GrantedRoot) {
            let access = StdFileAccess::new();
            let root = access.add_root(&self.dir.join("Music"), None, RootKind::Library).unwrap();
            (access, root)
        }
    }

    impl Drop for Library {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn opts() -> CallOptions {
        CallOptions::default()
    }

    #[test]
    fn a_root_gets_a_stable_id_and_a_name_from_its_folder_and_adding_it_twice_is_the_same_root() {
        let library = Library::new("roots");
        let (access, root) = library.access();
        assert_eq!(root.display_name, "Music");
        assert!(root.id.starts_with("root-") && root.removable);
        assert_eq!(access.add_root(&library.dir.join("Music"), Some("Other name"), RootKind::Library).unwrap().id, root.id);
        assert_eq!(StdFileAccess::new().add_root(&library.dir.join("Music"), None, RootKind::Library).unwrap().id, root.id, "stable across runs");
        assert_eq!(block_on(access.list_granted_roots()).unwrap().len(), 1);
        block_on(access.revoke_root(&root.id)).unwrap();
        assert!(block_on(access.list_granted_roots()).unwrap().is_empty());
        assert!(access.add_root(&library.dir.join("nope"), None, RootKind::Library).is_err());
        assert!(access.add_root(&library.dir.join("Music/Mix.m3u8"), None, RootKind::Library).is_err(), "a file is not a folder");
    }

    #[test]
    fn listing_a_folder_gives_sorted_entries_with_root_scoped_ids_sizes_and_times() {
        let library = Library::new("list");
        let (access, root) = library.access();
        let top = block_on(access.list_directory(&root.id, None, opts())).unwrap();
        assert_eq!(top.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["Mix.m3u8", "Rock"]);
        assert_eq!(top[1].kind, EntryKind::Directory);
        let rock = block_on(access.list_directory(&root.id, Some("Rock"), opts())).unwrap();
        assert_eq!(rock.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["A Song.lrc", "A Song.mp3", "b side.mp3"], "case-insensitive order");
        let EntryKind::File(song) = &rock[1].kind else { panic!("a file") };
        assert_eq!((song.id.clone(), song.relative_path.as_str(), song.name.as_str(), song.size_bytes), (format!("{}:Rock/A Song.mp3", root.id), "Rock/A Song.mp3", "A Song.mp3", 7));
        assert!(song.last_modified_ms > 1_500_000_000_000.0, "a plausible modified time in ms: {}", song.last_modified_ms);
        assert_eq!(rock[1].relative_path, "Rock/A Song.mp3");
    }

    #[test]
    fn files_read_as_bytes_or_lossy_text_and_a_missing_one_is_not_found() {
        let library = Library::new("read");
        let (access, root) = library.access();
        let rock = block_on(access.list_directory(&root.id, Some("Rock"), opts())).unwrap();
        let EntryKind::File(lrc) = &rock[0].kind else { panic!("a file") };
        let EntryKind::File(song) = &rock[1].kind else { panic!("a file") };
        assert_eq!(block_on(access.read_file_bytes(song, opts())).unwrap(), b"audio-a");
        assert_eq!(block_on(access.read_file_text(lrc, opts())).unwrap(), "[00:01.00]Hello \u{e9}");
        std::fs::write(library.dir.join("Music/Rock/A Song.lrc"), [b'o', b'k', 0xff, b'!']).unwrap();
        assert_eq!(block_on(access.read_file_text(lrc, opts())).unwrap(), "ok\u{fffd}!");
        std::fs::remove_file(library.dir.join("Music/Rock/A Song.mp3")).unwrap();
        assert!(matches!(block_on(access.read_file_bytes(song, opts())), Err(FileAccessError::NotFound(_))));
        assert!(matches!(block_on(access.list_directory(&root.id, Some("Gone"), opts())), Err(FileAccessError::NotFound(_))));
    }

    #[test]
    fn nothing_can_reach_outside_the_root_and_an_unknown_root_is_reported() {
        let library = Library::new("escape");
        std::fs::write(library.dir.join("secret.txt"), "secret").unwrap();
        let (access, root) = library.access();
        // A drive path is only a path escape on Windows; elsewhere `C:\...` is one ordinary file name inside the root.
        let mut sneaky_paths = vec!["../secret.txt", "Rock/../../secret.txt", "/etc/passwd"];
        if cfg!(windows) {
            sneaky_paths.push("C:\\Windows\\win.ini");
        }
        for sneaky in sneaky_paths {
            let file = FileRef { id: format!("{}:{sneaky}", root.id), name: "x".into(), relative_path: sneaky.into(), size_bytes: 0, last_modified_ms: 0.0 };
            assert!(block_on(access.read_file_text(&file, opts())).is_err(), "{sneaky}");
            assert!(block_on(access.list_directory(&root.id, Some(sneaky), opts())).is_err(), "{sneaky}");
            assert!(block_on(access.write_file_text(&root.id, sneaky, "x")).is_err(), "{sneaky}");
        }
        assert_eq!(std::fs::read_to_string(library.dir.join("secret.txt")).unwrap(), "secret");
        assert!(matches!(block_on(access.list_directory("root-nope", None, opts())), Err(FileAccessError::UnknownRoot(_))));
        let orphan = FileRef { id: "no-colon".into(), name: "x".into(), relative_path: "x".into(), size_bytes: 0, last_modified_ms: 0.0 };
        assert!(block_on(access.read_file_bytes(&orphan, opts())).is_err());
    }

    #[test]
    fn writing_replaces_a_file_atomically_leaving_no_staging_file() {
        let library = Library::new("write");
        let (access, root) = library.access();
        block_on(access.write_file_text(&root.id, "Mix.m3u8", "#EXTM3U\nnew\n")).unwrap();
        assert_eq!(std::fs::read_to_string(library.dir.join("Music/Mix.m3u8")).unwrap(), "#EXTM3U\nnew\n");
        block_on(access.write_file_text(&root.id, "Rock/New.m3u8", "x")).unwrap();
        let names: Vec<String> = std::fs::read_dir(library.dir.join("Music/Rock")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(names.iter().all(|n| !n.contains("beatseam-tmp")), "{names:?}");
        assert!(block_on(access.write_file_text(&root.id, "NoSuchFolder/x.m3u8", "x")).is_err(), "the parent must already exist");
    }

    #[test]
    fn a_real_folder_scans_into_a_sqlite_library_that_loads_back_the_same_way() {
        let library = Library::new("scan");
        let (access, root) = library.access();
        let store = SqliteLibraryStore::in_memory().unwrap();
        let scanned = block_on(scan_root(&access, &store, &root.id, None, None)).unwrap();
        assert!(scanned.unresolved_entries.is_empty(), "{:?}", scanned.unresolved_entries);
        let loaded = block_on(load_root_library(&access, &store, root.clone(), Default::default())).unwrap();
        assert_eq!(loaded.playlists.len(), 1);
        let playlist = &loaded.playlists[0];
        assert_eq!((playlist.name.as_str(), playlist.track_file_ids.len()), ("Mix", 2));
        assert_eq!(playlist.track_file_ids[0], format!("{}:Rock/b side.mp3", root.id), "the playlist's own order, resolved relative to its file");
        assert_eq!(loaded.tracks_by_id.len(), 2);
        assert_eq!(loaded.tracks_by_id[&playlist.track_file_ids[1]].size_bytes, 7);
        assert_eq!(block_on(store.list_playlists(&root.id)).unwrap().len(), 1, "persisted in the database");
    }
}
