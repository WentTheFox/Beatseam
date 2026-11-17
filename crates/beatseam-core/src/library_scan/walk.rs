use std::collections::VecDeque;

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;

use super::error::LibraryError;
use crate::file_access::{CallOptions, EntryKind, FileAccess, FileRef};
use crate::tasks::AbortSignal;

const PLAYLIST_EXTENSIONS: [&str; 2] = [".m3u8", ".m3u"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalkProgress {
    pub folders_listed: usize,
    /// Every file seen so far, playlists and non-audio files included.
    pub files_found: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WalkResult {
    /// Every file found under the root, playlists included.
    pub files: Vec<FileRef>,
    pub playlist_files: Vec<FileRef>,
}

fn is_playlist_file(name: &str) -> bool {
    let lower = name.to_lowercase();
    PLAYLIST_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

/// A dot-prefixed directory is always tooling's own bookkeeping, never content a user actually curated - e.g.
/// Syncthing's ".stversions" (old revisions of synced files, kept around for conflict recovery) and ".stfolder" (its
/// per-folder marker), or a plain ".git". Recursing into one wastes a whole `list_directory` round-trip (a real HTTP
/// request for the self-hosted server adapter) per level for files that could never be legitimate playlist entries
/// anyway, and for Syncthing specifically could mean scanning the same tracks' entire version history as if they were
/// real library content.
fn is_ignored_directory(name: &str) -> bool {
    name.starts_with('.')
}

/// Upper bound on `list_directory` calls in flight at once during a walk. Walking siblings concurrently used to be
/// unbounded, which on Windows fired one storage-broker listing per artist folder all at once - a large library
/// ballooned the app to ~850 threads and the scan hung forever with zero I/O. A small cap keeps most of the concurrency
/// win for the self-hosted server adapter's HTTP round-trips without flooding any platform's native directory API.
pub const MAX_CONCURRENT_LISTINGS: usize = 8;

fn cancelled(signal: Option<&AbortSignal>) -> bool {
    signal.is_some_and(AbortSignal::is_aborted)
}

/// Recursively walks a granted root via repeated single-level `list_directory` calls, since that's the operation every
/// platform's adapter can implement directly against its native directory APIs. Up to [`MAX_CONCURRENT_LISTINGS`]
/// listings run at once - each is a full HTTP round-trip for the server adapter, so a library with hundreds of artist
/// folders would otherwise be hundreds of round-trips back to back. Slots are only held around the listing itself, never
/// across the descent, so a tree deeper than the cap can't deadlock. Result order is discovery order across the
/// concurrent listings and is not meaningful (nothing downstream relies on it).
///
/// `signal` is checked before each listing starts and after each finishes (there's no way to abort a native listing
/// that's already in flight, only to stop starting new ones). One that's already aborted when this is called fails
/// immediately without listing anything. `on_progress` is called after every completed listing with running totals -
/// there's no known total up front, a walk only discovers the tree as it goes.
pub async fn walk_directory<F: FileAccess>(
    file_access: &F,
    root_id: &str,
    start_path: Option<&str>,
    signal: Option<&AbortSignal>,
    mut on_progress: Option<&mut dyn FnMut(WalkProgress)>,
) -> Result<WalkResult, LibraryError> {
    let mut result = WalkResult::default();
    let mut folders_listed = 0;
    // An empty start path is the root itself, passed as `None` (not `Some("")`): some adapters key their root entry by
    // "no path" specifically.
    let mut pending: VecDeque<Option<String>> = VecDeque::from([start_path.filter(|p| !p.is_empty()).map(str::to_owned)]);
    let mut in_flight = FuturesUnordered::new();

    loop {
        while in_flight.len() < MAX_CONCURRENT_LISTINGS {
            let Some(dir) = pending.pop_front() else { break };
            if cancelled(signal) {
                return Err(LibraryError::Cancelled);
            }
            in_flight.push(async move { file_access.list_directory(root_id, dir.as_deref(), CallOptions::default()).await });
        }
        let Some(listing) = in_flight.next().await else { break };
        let entries = listing?;
        if cancelled(signal) {
            return Err(LibraryError::Cancelled);
        }
        for entry in entries {
            match entry.kind {
                EntryKind::File(file) => {
                    if is_playlist_file(&file.name) {
                        result.playlist_files.push(file.clone());
                    }
                    result.files.push(file);
                }
                EntryKind::Directory if !is_ignored_directory(&entry.name) => pending.push_back(Some(entry.relative_path)),
                EntryKind::Directory => {}
            }
        }
        folders_listed += 1;
        if let Some(on_progress) = on_progress.as_mut() {
            on_progress(WalkProgress { folders_listed, files_found: result.files.len() });
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;
    use crate::file_access::{DirectoryEntry, FileAccessError, GrantedRoot, MemoryFileAccess, RootKind};
    use crate::tasks::AbortHandle;
    use crate::tasks::test_executor::{Pool, yield_now};

    /// A `FileAccess` over a set of paths where each listing takes a few scheduler turns to resolve, so concurrent
    /// listings overlap, and which records the most listings ever in flight at once.
    struct Tracking {
        inner: MemoryFileAccess,
        in_flight: Cell<usize>,
        max_in_flight: Cell<usize>,
        calls: Cell<usize>,
    }

    impl Tracking {
        fn new(paths: &[String]) -> Self {
            Self { inner: MemoryFileAccess::new(paths.iter().map(|p| (p.as_str(), "x"))), in_flight: Cell::new(0), max_in_flight: Cell::new(0), calls: Cell::new(0) }
        }
    }

    impl FileAccess for Tracking {
        async fn request_root(&self, _k: RootKind) -> Result<Option<GrantedRoot>, FileAccessError> {
            Ok(None)
        }
        async fn list_granted_roots(&self) -> Result<Vec<GrantedRoot>, FileAccessError> {
            Ok(vec![])
        }
        async fn revoke_root(&self, _r: &str) -> Result<(), FileAccessError> {
            Ok(())
        }
        async fn list_directory(&self, root_id: &str, relative_path: Option<&str>, opts: CallOptions) -> Result<Vec<DirectoryEntry>, FileAccessError> {
            self.calls.set(self.calls.get() + 1);
            self.in_flight.set(self.in_flight.get() + 1);
            self.max_in_flight.set(self.max_in_flight.get().max(self.in_flight.get()));
            yield_now().await;
            self.in_flight.set(self.in_flight.get() - 1);
            self.inner.list_directory(root_id, relative_path, opts).await
        }
        async fn read_file_bytes(&self, f: &FileRef, o: CallOptions) -> Result<Vec<u8>, FileAccessError> {
            self.inner.read_file_bytes(f, o).await
        }
        async fn read_file_text(&self, f: &FileRef, o: CallOptions) -> Result<String, FileAccessError> {
            self.inner.read_file_text(f, o).await
        }
        async fn write_file_text(&self, r: &str, p: &str, c: &str) -> Result<(), FileAccessError> {
            self.inner.write_file_text(r, p, c).await
        }
    }

    #[test]
    fn caps_concurrent_listings_for_a_wide_library_instead_of_listing_every_folder_at_once() {
        // 200 artist folders x 3 albums each - previously 200, then 600, listings all in flight at the same moment.
        let paths: Vec<String> = (0..200).flat_map(|artist| (0..3).map(move |album| format!("Artist {artist}/Album {album}/01.mp3"))).collect();
        let fa = Rc::new(Tracking::new(&paths));
        let pool = Pool::new();
        let f = fa.clone();
        let result = pool.spawn(async move { walk_directory(&*f, "root-1", None, None, None).await });
        pool.run_until_stalled();

        let walked = result.take().expect("finished").unwrap();
        assert_eq!(walked.files.len(), 600);
        assert_eq!(fa.calls.get(), 1 + 200 + 600);
        assert!(fa.max_in_flight.get() > 1, "listings overlap");
        assert!(fa.max_in_flight.get() <= MAX_CONCURRENT_LISTINGS, "{}", fa.max_in_flight.get());
    }

    #[test]
    fn finishes_a_tree_deeper_than_the_concurrency_cap() {
        let deep = (0..30).map(|i| format!("d{i}")).collect::<Vec<_>>().join("/");
        let fa = Rc::new(Tracking::new(&[format!("{deep}/track.mp3"), "top.mp3".to_string()]));
        let pool = Pool::new();
        let f = fa.clone();
        let result = pool.spawn(async move { walk_directory(&*f, "root-1", None, None, None).await });
        pool.run_until_stalled();
        let mut names: Vec<String> = result.take().unwrap().unwrap().files.into_iter().map(|f| f.name).collect();
        names.sort();
        assert_eq!(names, ["top.mp3", "track.mp3"]);
    }

    #[test]
    fn separates_playlists_skips_dot_directories_and_reports_progress() {
        let fa = MemoryFileAccess::new([("Mix.M3U8", "a"), ("A/one.mp3", "b"), ("A/x.m3u", "c"), (".stversions/old.m3u8", "d"), (".git/config", "e")]);
        let mut progress = Vec::new();
        let mut record = |p: WalkProgress| progress.push(p);
        let walked = crate::http::block_on(walk_directory(&fa, "r", None, None, Some(&mut record))).unwrap();
        let mut all: Vec<_> = walked.files.iter().map(|f| f.relative_path.as_str()).collect();
        all.sort();
        assert_eq!(all, ["A/one.mp3", "A/x.m3u", "Mix.M3U8"]);
        let mut playlists: Vec<_> = walked.playlist_files.iter().map(|f| f.relative_path.as_str()).collect();
        playlists.sort();
        assert_eq!(playlists, ["A/x.m3u", "Mix.M3U8"]);
        assert_eq!(progress.len(), 2, "the root and A (the dot directories are never listed)");
        assert_eq!(progress.last().unwrap(), &WalkProgress { folders_listed: 2, files_found: 3 });
    }

    #[test]
    fn an_empty_start_path_means_the_root_and_a_subpath_walks_just_that_subtree() {
        let fa = MemoryFileAccess::new([("A/one.mp3", "b"), ("B/two.mp3", "c")]);
        let from_root = crate::http::block_on(walk_directory(&fa, "r", Some(""), None, None)).unwrap();
        assert_eq!(from_root.files.len(), 2);
        let from_a = crate::http::block_on(walk_directory(&fa, "r", Some("A"), None, None)).unwrap();
        assert_eq!(from_a.files.iter().map(|f| f.relative_path.as_str()).collect::<Vec<_>>(), ["A/one.mp3"]);
    }

    #[test]
    fn an_already_aborted_signal_fails_before_listing_anything() {
        let fa = Rc::new(Tracking::new(&["a/b.mp3".to_string()]));
        let handle = AbortHandle::new();
        handle.abort();
        let (pool, f, signal) = (Pool::new(), fa.clone(), handle.signal());
        let result = pool.spawn(async move { walk_directory(&*f, "r", None, Some(&signal), None).await });
        pool.run_until_stalled();
        assert_eq!(result.take(), Some(Err(LibraryError::Cancelled)));
        assert_eq!(fa.calls.get(), 0);
    }

    #[test]
    fn a_listing_error_stops_the_walk_with_that_error() {
        struct Broken;
        impl FileAccess for Broken {
            async fn request_root(&self, _k: RootKind) -> Result<Option<GrantedRoot>, FileAccessError> {
                Ok(None)
            }
            async fn list_granted_roots(&self) -> Result<Vec<GrantedRoot>, FileAccessError> {
                Ok(vec![])
            }
            async fn revoke_root(&self, _r: &str) -> Result<(), FileAccessError> {
                Ok(())
            }
            async fn list_directory(&self, root_id: &str, _p: Option<&str>, _o: CallOptions) -> Result<Vec<DirectoryEntry>, FileAccessError> {
                Err(FileAccessError::UnknownRoot(root_id.into()))
            }
            async fn read_file_bytes(&self, _f: &FileRef, _o: CallOptions) -> Result<Vec<u8>, FileAccessError> {
                Ok(vec![])
            }
            async fn read_file_text(&self, _f: &FileRef, _o: CallOptions) -> Result<String, FileAccessError> {
                Ok(String::new())
            }
            async fn write_file_text(&self, _r: &str, _p: &str, _c: &str) -> Result<(), FileAccessError> {
                Ok(())
            }
        }
        let err = crate::http::block_on(walk_directory(&Broken, "Lyrics", None, None, None)).unwrap_err();
        assert_eq!(err.to_string(), "No library root \"Lyrics\"");
    }
}
