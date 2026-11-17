use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;

use super::types::{CallOptions, DirectoryEntry, EntryKind, FileAccess, FileAccessError, FileRef, GrantedRoot, RootKind};

/// A [`FileAccess`] over a flat `{ relative path: content }` map: for tests, fixtures and the visual-parity renderer.
/// Every root id sees the same tree. File ids are the relative path, and a file's size is its content length in bytes.
/// Directory listings follow insertion order of the first file under each child, like the TypeScript fake it replaces.
#[derive(Default)]
pub struct MemoryFileAccess {
    files: Mutex<Vec<(String, String)>>,
    granted: Mutex<Vec<GrantedRoot>>,
}

impl MemoryFileAccess {
    pub fn new<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self { files: Mutex::new(files.into_iter().map(|(p, c)| (p.to_owned(), c.to_owned())).collect()), granted: Mutex::default() }
    }

    pub fn with_roots(self, roots: impl IntoIterator<Item = GrantedRoot>) -> Self {
        *self.granted.lock().unwrap() = roots.into_iter().collect();
        self
    }

    /// Simulates a file showing up between scans (e.g. a sync finishing), or being rewritten.
    pub fn add_file(&self, relative_path: &str, content: &str) {
        let mut files = self.files.lock().unwrap();
        match files.iter_mut().find(|(p, _)| p == relative_path) {
            Some(slot) => slot.1 = content.to_owned(),
            None => files.push((relative_path.to_owned(), content.to_owned())),
        }
    }

    /// Simulates a file going missing between scans.
    pub fn remove_file(&self, relative_path: &str) {
        self.files.lock().unwrap().retain(|(p, _)| p != relative_path);
    }

    pub fn contents(&self) -> BTreeMap<String, String> {
        self.files.lock().unwrap().iter().cloned().collect()
    }

    fn file_ref(path: &str, content: &str) -> FileRef {
        FileRef {
            id: path.to_owned(),
            name: path.rsplit('/').next().unwrap_or(path).to_owned(),
            relative_path: path.to_owned(),
            size_bytes: content.len() as u64,
            last_modified_ms: 0.0,
        }
    }

    fn content_of(&self, file: &FileRef) -> Result<String, FileAccessError> {
        self.files.lock().unwrap().iter().find(|(p, _)| *p == file.relative_path).map(|(_, c)| c.clone()).ok_or_else(|| FileAccessError::NotFound(file.relative_path.clone()))
    }
}

impl FileAccess for MemoryFileAccess {
    async fn request_root(&self, _kind: RootKind) -> Result<Option<GrantedRoot>, FileAccessError> {
        Ok(None)
    }

    async fn list_granted_roots(&self) -> Result<Vec<GrantedRoot>, FileAccessError> {
        Ok(self.granted.lock().unwrap().clone())
    }

    async fn revoke_root(&self, root_id: &str) -> Result<(), FileAccessError> {
        self.granted.lock().unwrap().retain(|r| r.id != root_id);
        Ok(())
    }

    async fn list_directory(&self, _root_id: &str, relative_path: Option<&str>, _opts: CallOptions) -> Result<Vec<DirectoryEntry>, FileAccessError> {
        let relative_path = relative_path.unwrap_or("");
        let prefix = if relative_path.is_empty() { String::new() } else { format!("{relative_path}/") };
        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        for (path, content) in self.files.lock().unwrap().iter() {
            let Some(remainder) = path.strip_prefix(&prefix) else { continue };
            let (child, rest) = match remainder.split_once('/') {
                Some((child, rest)) => (child, Some(rest)),
                None => (remainder, None),
            };
            if child.is_empty() || !seen.insert(child.to_owned()) {
                continue;
            }
            let child_path = format!("{prefix}{child}");
            entries.push(DirectoryEntry {
                name: child.to_owned(),
                kind: if rest.is_none() { EntryKind::File(Self::file_ref(&child_path, content)) } else { EntryKind::Directory },
                relative_path: child_path,
            });
        }
        Ok(entries)
    }

    async fn read_file_bytes(&self, file: &FileRef, _opts: CallOptions) -> Result<Vec<u8>, FileAccessError> {
        Ok(self.content_of(file)?.into_bytes())
    }

    async fn read_file_text(&self, file: &FileRef, _opts: CallOptions) -> Result<String, FileAccessError> {
        self.content_of(file)
    }

    async fn write_file_text(&self, _root_id: &str, relative_path: &str, contents: &str) -> Result<(), FileAccessError> {
        self.add_file(relative_path, contents);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::block_on;

    #[test]
    fn lists_one_level_with_files_and_directories() {
        let fa = MemoryFileAccess::new([("A/one.mp3", "x"), ("A/B/two.mp3", "yy"), ("top.m3u8", "z")]);
        let root = block_on(fa.list_directory("r", None, CallOptions::default())).unwrap();
        assert_eq!(root.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["A", "top.m3u8"]);
        assert_eq!(root[0].kind, EntryKind::Directory);
        let a = block_on(fa.list_directory("r", Some("A"), CallOptions::default())).unwrap();
        assert_eq!(a.iter().map(|e| e.relative_path.as_str()).collect::<Vec<_>>(), ["A/one.mp3", "A/B"]);
        assert!(matches!(&a[0].kind, EntryKind::File(f) if f.id == "A/one.mp3" && f.size_bytes == 1 && f.name == "one.mp3"));
    }

    #[test]
    fn reads_writes_and_removes() {
        let fa = MemoryFileAccess::new([("a.txt", "hello")]);
        let file = FileRef { id: "a.txt".into(), name: "a.txt".into(), relative_path: "a.txt".into(), size_bytes: 5, last_modified_ms: 0.0 };
        assert_eq!(block_on(fa.read_file_text(&file, CallOptions::default())).unwrap(), "hello");
        block_on(fa.write_file_text("r", "a.txt", "bye")).unwrap();
        assert_eq!(block_on(fa.read_file_bytes(&file, CallOptions::default())).unwrap(), b"bye");
        fa.remove_file("a.txt");
        assert_eq!(block_on(fa.read_file_text(&file, CallOptions::default())), Err(FileAccessError::NotFound("a.txt".into())));
    }
}
