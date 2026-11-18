use std::future::Future;

use beatseam_core::file_access::{CallOptions, DirectoryEntry, FileAccess, FileAccessError, FileRef, GrantedRoot, LyricsAnswer, RootKind};
use beatseam_core::offline::{BackendError, OfflineBackend, OfflineLibrary, OfflineRuntime, OfflineSource};
use beatseam_core::library_store::LibraryStore;

/// An operation that only makes sense with local folders (granting a folder, editing an .m3u8 in place, ...).
pub fn unsupported(what: &str) -> FileAccessError {
    FileAccessError::Unsupported(format!("{what} isn't available while the library comes from a server"))
}

/// What file access needs of the server connection.
pub trait ServerLink {
    /// Whether the library currently comes from a server (decided per call, so signing in or out takes effect at once).
    fn is_active(&self) -> bool;
    /// The roots named by the last sync (no network).
    fn roots(&self) -> Vec<GrantedRoot>;
    /// A track's audio from the server; fails when there is no connection.
    fn stream_bytes(&self, file_id: &str) -> impl Future<Output = Result<Vec<u8>, FileAccessError>>;
    /// The server's parsed lyrics for a track.
    fn lyrics_for(&self, track_file_id: &str) -> impl Future<Output = Result<LyricsAnswer, FileAccessError>>;
}

/// Downloaded copies, as file access uses them.
pub trait OfflineAccess {
    /// A downloaded (and still current) copy's bytes, if there is one.
    fn read(&self, file_id: &str) -> impl Future<Output = Result<Option<Vec<u8>>, BackendError>>;
    /// Keeping what is played: one transfer to storage serves both the play and the copy. `true` when a copy is stored.
    fn fetch_for_playback(&self, file_id: &str) -> impl Future<Output = bool>;
}

/// No offline storage on this platform.
pub struct NoOffline;

impl OfflineAccess for NoOffline {
    async fn read(&self, _file_id: &str) -> Result<Option<Vec<u8>>, BackendError> {
        Ok(None)
    }
    async fn fetch_for_playback(&self, _file_id: &str) -> bool {
        false
    }
}

impl<B, S, V, R> OfflineAccess for OfflineLibrary<B, S, V, R>
where
    B: OfflineBackend + 'static,
    S: LibraryStore + 'static,
    V: OfflineSource + 'static,
    R: OfflineRuntime + 'static,
{
    async fn read(&self, file_id: &str) -> Result<Option<Vec<u8>>, BackendError> {
        OfflineLibrary::read(self, file_id).await
    }
    async fn fetch_for_playback(&self, file_id: &str) -> bool {
        OfflineLibrary::fetch_for_playback(self, file_id).await
    }
}

/// File access over the server's library. The library itself arrives through the sync, so this only has to name the roots
/// (from the last sync, no network) and hand over a track's audio on request.
pub struct ServerFileAccess<V, O> {
    link: V,
    offline: O,
}

impl<V: ServerLink, O: OfflineAccess> FileAccess for ServerFileAccess<V, O> {
    async fn request_root(&self, _kind: RootKind) -> Result<Option<GrantedRoot>, FileAccessError> {
        Err(unsupported("Adding a folder"))
    }

    async fn list_granted_roots(&self) -> Result<Vec<GrantedRoot>, FileAccessError> {
        Ok(self.link.roots())
    }

    async fn revoke_root(&self, _root_id: &str) -> Result<(), FileAccessError> {
        Err(unsupported("Removing a folder"))
    }

    async fn list_directory(&self, _root_id: &str, _relative_path: Option<&str>, _opts: CallOptions) -> Result<Vec<DirectoryEntry>, FileAccessError> {
        Ok(Vec::new())
    }

    async fn read_file_bytes(&self, file: &FileRef, _opts: CallOptions) -> Result<Vec<u8>, FileAccessError> {
        // A downloaded (and still current) copy plays with no network at all.
        if let Some(stored) = self.offline.read(&file.id).await.map_err(|e| FileAccessError::Io(e.0))? {
            return Ok(stored);
        }
        if self.offline.fetch_for_playback(&file.id).await {
            if let Some(kept) = self.offline.read(&file.id).await.map_err(|e| FileAccessError::Io(e.0))? {
                return Ok(kept);
            }
        }
        self.link.stream_bytes(&file.id).await
    }

    async fn read_file_text(&self, _file: &FileRef, _opts: CallOptions) -> Result<String, FileAccessError> {
        Err(unsupported("Reading lyrics files"))
    }

    async fn write_file_text(&self, _root_id: &str, _relative_path: &str, _contents: &str) -> Result<(), FileAccessError> {
        Err(unsupported("Editing playlist files"))
    }
}

/// The file access the app uses: local folders normally, the server's library once signed in. Decided per call, so signing in
/// or out takes effect immediately without rebuilding anything.
pub struct ModeAwareFileAccess<L, V, O> {
    local: L,
    server: ServerFileAccess<V, O>,
}

impl<L, V, O> ModeAwareFileAccess<L, V, O> {
    pub fn new(local: L, link: V, offline: O) -> Self {
        Self { local, server: ServerFileAccess { link, offline } }
    }
}

impl<L: FileAccess, V: ServerLink, O: OfflineAccess> ModeAwareFileAccess<L, V, O> {
    fn active(&self) -> bool {
        self.server.link.is_active()
    }
}

impl<L: FileAccess, V: ServerLink, O: OfflineAccess> FileAccess for ModeAwareFileAccess<L, V, O> {
    async fn request_root(&self, kind: RootKind) -> Result<Option<GrantedRoot>, FileAccessError> {
        if self.active() { self.server.request_root(kind).await } else { self.local.request_root(kind).await }
    }

    async fn list_granted_roots(&self) -> Result<Vec<GrantedRoot>, FileAccessError> {
        if self.active() { self.server.list_granted_roots().await } else { self.local.list_granted_roots().await }
    }

    async fn revoke_root(&self, root_id: &str) -> Result<(), FileAccessError> {
        if self.active() { self.server.revoke_root(root_id).await } else { self.local.revoke_root(root_id).await }
    }

    async fn list_directory(&self, root_id: &str, relative_path: Option<&str>, opts: CallOptions) -> Result<Vec<DirectoryEntry>, FileAccessError> {
        if self.active() { self.server.list_directory(root_id, relative_path, opts).await } else { self.local.list_directory(root_id, relative_path, opts).await }
    }

    async fn read_file_bytes(&self, file: &FileRef, opts: CallOptions) -> Result<Vec<u8>, FileAccessError> {
        if self.active() { self.server.read_file_bytes(file, opts).await } else { self.local.read_file_bytes(file, opts).await }
    }

    async fn read_file_text(&self, file: &FileRef, opts: CallOptions) -> Result<String, FileAccessError> {
        if self.active() { self.server.read_file_text(file, opts).await } else { self.local.read_file_text(file, opts).await }
    }

    async fn write_file_text(&self, root_id: &str, relative_path: &str, contents: &str) -> Result<(), FileAccessError> {
        if self.active() { self.server.write_file_text(root_id, relative_path, contents).await } else { self.local.write_file_text(root_id, relative_path, contents).await }
    }

    /// Only meaningful for local folders (and the legacy web server): metadata comes from the sync in server mode.
    fn stream_url(&self, file: &FileRef) -> Option<String> {
        if self.active() { None } else { self.local.stream_url(file) }
    }

    async fn content_hash(&self, file: &FileRef) -> Result<Option<String>, FileAccessError> {
        if self.active() { Ok(None) } else { self.local.content_hash(file).await }
    }

    /// The server matched and parsed the lyrics; signed out, the local .lrc files are read as before (no say).
    async fn load_lyrics(&self, track_file_id: &str) -> Result<LyricsAnswer, FileAccessError> {
        if self.active() { self.server.link.lyrics_for(track_file_id).await } else { Ok(LyricsAnswer::NoSay) }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use beatseam_core::file_access::MemoryFileAccess;
    use beatseam_core::http::block_on;

    use super::*;

    #[derive(Clone)]
    struct Link {
        active: Rc<Cell<bool>>,
        streamed: Rc<RefCell<Vec<String>>>,
    }

    impl ServerLink for Link {
        fn is_active(&self) -> bool {
            self.active.get()
        }
        fn roots(&self) -> Vec<GrantedRoot> {
            vec![GrantedRoot { id: "server-root".into(), display_name: "Server".into(), kind: RootKind::Library, removable: false }]
        }
        async fn stream_bytes(&self, file_id: &str) -> Result<Vec<u8>, FileAccessError> {
            self.streamed.borrow_mut().push(file_id.to_owned());
            Ok(b"streamed".to_vec())
        }
        async fn lyrics_for(&self, _id: &str) -> Result<LyricsAnswer, FileAccessError> {
            Ok(LyricsAnswer::NoLyrics)
        }
    }

    struct Stored {
        has: Cell<bool>,
        keep_on_play: Cell<bool>,
    }

    impl OfflineAccess for Rc<Stored> {
        async fn read(&self, _id: &str) -> Result<Option<Vec<u8>>, BackendError> {
            Ok(self.has.get().then(|| b"stored".to_vec()))
        }
        async fn fetch_for_playback(&self, _id: &str) -> bool {
            if self.keep_on_play.get() {
                self.has.set(true);
            }
            self.keep_on_play.get()
        }
    }

    fn file() -> FileRef {
        FileRef { id: "a".into(), name: "a.mp3".into(), relative_path: "a.mp3".into(), size_bytes: 1, last_modified_ms: 1.0 }
    }

    fn setup(has: bool, keep: bool) -> (ModeAwareFileAccess<MemoryFileAccess, Link, Rc<Stored>>, Link) {
        let link = Link { active: Rc::new(Cell::new(true)), streamed: Rc::default() };
        let stored = Rc::new(Stored { has: Cell::new(has), keep_on_play: Cell::new(keep) });
        (ModeAwareFileAccess::new(MemoryFileAccess::new([("a.mp3", "local")]), link.clone(), stored), link)
    }

    fn bytes(fa: &impl FileAccess) -> Vec<u8> {
        block_on(fa.read_file_bytes(&file(), CallOptions::default())).unwrap()
    }

    #[test]
    fn a_downloaded_copy_wins_then_a_kept_play_then_the_stream() {
        let (fa, link) = setup(true, false);
        assert_eq!(bytes(&fa), b"stored");
        assert!(link.streamed.borrow().is_empty());

        let (fa, link) = setup(false, true);
        assert_eq!(bytes(&fa), b"stored", "the play fetch stored it, so it is read from storage");
        assert!(link.streamed.borrow().is_empty());

        let (fa, link) = setup(false, false);
        assert_eq!(bytes(&fa), b"streamed");
        assert_eq!(*link.streamed.borrow(), ["a"]);
    }

    #[test]
    fn the_mode_is_decided_per_call() {
        let (fa, link) = setup(false, false);
        assert_eq!(block_on(fa.list_granted_roots()).unwrap()[0].id, "server-root");
        assert_eq!(block_on(fa.load_lyrics("a")).unwrap(), LyricsAnswer::NoLyrics);
        link.active.set(false);
        assert_eq!(bytes(&fa), b"local");
        assert_eq!(block_on(fa.load_lyrics("a")).unwrap(), LyricsAnswer::NoSay);
        assert_eq!(block_on(fa.content_hash(&file())).unwrap(), None);
    }

    #[test]
    fn folder_operations_are_refused_in_server_mode_with_a_message() {
        let (fa, _) = setup(false, false);
        assert_eq!(block_on(fa.request_root(RootKind::Library)).unwrap_err().to_string(), "Adding a folder isn't available while the library comes from a server");
        assert_eq!(block_on(fa.revoke_root("x")).unwrap_err().to_string(), "Removing a folder isn't available while the library comes from a server");
        assert_eq!(block_on(fa.read_file_text(&file(), CallOptions::default())).unwrap_err().to_string(), "Reading lyrics files isn't available while the library comes from a server");
        assert_eq!(block_on(fa.write_file_text("r", "p", "c")).unwrap_err().to_string(), "Editing playlist files isn't available while the library comes from a server");
        assert_eq!(block_on(fa.list_directory("r", None, CallOptions::default())).unwrap(), vec![]);
    }
}
