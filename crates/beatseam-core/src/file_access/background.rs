use super::types::{CallOptions, DirectoryEntry, FileAccess, FileAccessError, FileRef, GrantedRoot, LyricsAnswer, RootKind};

/// Wraps a [`FileAccess`] so every read call is marked "may not prompt" - for background/idle-scheduled passes (lyrics
/// matching) that have no user gesture behind them to call `requestPermission()` from; see
/// [`CallOptions::allow_prompt`] for why that fails instead of prompting.
///
/// Create it once per underlying `FileAccess` and reuse it: a scan-result cache keyed by file-access identity (as the
/// lyrics scope scanner keeps) would never hit if a fresh wrapper were made per call.
pub struct BackgroundFileAccess<F: FileAccess> {
    inner: F,
}

impl<F: FileAccess> BackgroundFileAccess<F> {
    pub fn new(inner: F) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> &F {
        &self.inner
    }
}

impl<F: FileAccess> FileAccess for BackgroundFileAccess<F> {
    fn request_root(&self, kind: RootKind) -> impl Future<Output = Result<Option<GrantedRoot>, FileAccessError>> {
        self.inner.request_root(kind)
    }

    fn list_granted_roots(&self) -> impl Future<Output = Result<Vec<GrantedRoot>, FileAccessError>> {
        self.inner.list_granted_roots()
    }

    fn revoke_root(&self, root_id: &str) -> impl Future<Output = Result<(), FileAccessError>> {
        self.inner.revoke_root(root_id)
    }

    fn list_directory(&self, root_id: &str, relative_path: Option<&str>, _opts: CallOptions) -> impl Future<Output = Result<Vec<DirectoryEntry>, FileAccessError>> {
        self.inner.list_directory(root_id, relative_path, CallOptions::BACKGROUND)
    }

    fn read_file_bytes(&self, file: &FileRef, _opts: CallOptions) -> impl Future<Output = Result<Vec<u8>, FileAccessError>> {
        self.inner.read_file_bytes(file, CallOptions::BACKGROUND)
    }

    fn read_file_text(&self, file: &FileRef, _opts: CallOptions) -> impl Future<Output = Result<String, FileAccessError>> {
        self.inner.read_file_text(file, CallOptions::BACKGROUND)
    }

    fn write_file_text(&self, root_id: &str, relative_path: &str, contents: &str) -> impl Future<Output = Result<(), FileAccessError>> {
        self.inner.write_file_text(root_id, relative_path, contents)
    }

    fn stream_url(&self, file: &FileRef) -> Option<String> {
        self.inner.stream_url(file)
    }

    fn load_lyrics(&self, track_file_id: &str) -> impl Future<Output = Result<LyricsAnswer, FileAccessError>> {
        self.inner.load_lyrics(track_file_id)
    }

    fn content_hash(&self, file: &FileRef) -> impl Future<Output = Result<Option<String>, FileAccessError>> {
        self.inner.content_hash(file)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::http::block_on;

    /// Records the options each read call arrived with.
    #[derive(Default)]
    struct Spy {
        seen: RefCell<Vec<(&'static str, bool)>>,
    }

    impl FileAccess for Spy {
        async fn request_root(&self, _kind: RootKind) -> Result<Option<GrantedRoot>, FileAccessError> {
            Ok(None)
        }
        async fn list_granted_roots(&self) -> Result<Vec<GrantedRoot>, FileAccessError> {
            Ok(vec![GrantedRoot::new("r", "R")])
        }
        async fn revoke_root(&self, _root_id: &str) -> Result<(), FileAccessError> {
            Ok(())
        }
        async fn list_directory(&self, _root_id: &str, _path: Option<&str>, opts: CallOptions) -> Result<Vec<DirectoryEntry>, FileAccessError> {
            self.seen.borrow_mut().push(("list", opts.allow_prompt));
            Ok(vec![])
        }
        async fn read_file_bytes(&self, _file: &FileRef, opts: CallOptions) -> Result<Vec<u8>, FileAccessError> {
            self.seen.borrow_mut().push(("bytes", opts.allow_prompt));
            Ok(vec![])
        }
        async fn read_file_text(&self, _file: &FileRef, opts: CallOptions) -> Result<String, FileAccessError> {
            self.seen.borrow_mut().push(("text", opts.allow_prompt));
            Ok(String::new())
        }
        async fn write_file_text(&self, _root_id: &str, _path: &str, _contents: &str) -> Result<(), FileAccessError> {
            Err(FileAccessError::Unsupported("no writing".into()))
        }
    }

    fn file() -> FileRef {
        FileRef { id: "f".into(), name: "f".into(), relative_path: "f".into(), size_bytes: 0, last_modified_ms: 0.0 }
    }

    #[test]
    fn every_read_goes_out_marked_as_not_allowed_to_prompt_whatever_the_caller_asked_for() {
        let background = BackgroundFileAccess::new(Spy::default());
        let loud = CallOptions { allow_prompt: true };
        block_on(background.list_directory("r", None, loud)).unwrap();
        block_on(background.read_file_bytes(&file(), loud)).unwrap();
        block_on(background.read_file_text(&file(), loud)).unwrap();
        assert_eq!(*background.inner().seen.borrow(), [("list", false), ("bytes", false), ("text", false)]);
    }

    #[test]
    fn everything_else_passes_straight_through() {
        let background = BackgroundFileAccess::new(Spy::default());
        assert_eq!(block_on(background.list_granted_roots()).unwrap(), vec![GrantedRoot::new("r", "R")]);
        assert_eq!(block_on(background.write_file_text("r", "a", "x")), Err(FileAccessError::Unsupported("no writing".into())));
        assert_eq!(background.stream_url(&file()), None);
        assert_eq!(block_on(background.load_lyrics("f")), Ok(LyricsAnswer::NoSay));
        assert_eq!(block_on(background.content_hash(&file())), Ok(None));
    }

    #[test]
    fn the_permission_pending_message_names_the_root() {
        let e = FileAccessError::PermissionPending { root_display_name: "Music".into() };
        assert_eq!(e.to_string(), "Read permission for \"Music\" needs to be re-granted, but this call isn't allowed to prompt for it.");
    }
}
