use std::collections::HashMap;

use super::error::LibraryError;
use super::scan::scan_root;
use crate::file_access::{FileAccess, GrantedRoot};
use crate::library_store::{LibraryStore, PlaylistRecord, TrackRecord};

#[derive(Clone, Debug)]
pub struct LoadedRoot {
    pub root: GrantedRoot,
    pub playlists: Vec<PlaylistRecord>,
    pub tracks_by_id: HashMap<String, TrackRecord>,
}

#[derive(Clone, Copy, Debug)]
pub struct LoadRootLibraryOptions {
    /// Scan the root's folders when nothing is stored for it yet (the default). Off when the library comes from a
    /// server: there are no folders to walk, and the sync fills the store instead.
    pub scan_if_empty: bool,
}

impl Default for LoadRootLibraryOptions {
    fn default() -> Self {
        Self { scan_if_empty: true }
    }
}

/// Loads one granted root's playlists/tracks from the store, scanning it first if the store has nothing for it yet - a
/// root can reach `list_granted_roots()` without ever going through the explicit request-root-then-scan flow (e.g. a
/// root the self-hosted server exposes just by having a volume mounted), so this covers that case the same way every
/// app's refresh does.
///
/// Shared so the per-root logic can't drift between apps, and so the startup restore path can load just the one root it
/// needs to resume playback without pulling in every other granted root first.
pub async fn load_root_library<F: FileAccess, S: LibraryStore>(file_access: &F, store: &S, root: GrantedRoot, options: LoadRootLibraryOptions) -> Result<LoadedRoot, LibraryError> {
    let mut playlists = store.list_playlists(&root.id).await?;
    let mut tracks = store.list_tracks(&root.id).await?;
    if playlists.is_empty() && tracks.is_empty() && options.scan_if_empty {
        scan_root(file_access, store, &root.id, None, None).await?;
        playlists = store.list_playlists(&root.id).await?;
        tracks = store.list_tracks(&root.id).await?;
    }
    Ok(LoadedRoot { root, playlists, tracks_by_id: tracks.into_iter().map(|t| (t.file_id.clone(), t)).collect() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_access::MemoryFileAccess;
    use crate::http::block_on;
    use crate::library_store::MemoryLibraryStore;

    fn fa() -> MemoryFileAccess {
        MemoryFileAccess::new([("Mix.m3u8", "Track.mp3"), ("Track.mp3", "x")])
    }

    #[test]
    fn scans_an_unknown_root_first_then_loads_it_from_the_store() {
        let store = MemoryLibraryStore::new();
        let loaded = block_on(load_root_library(&fa(), &store, GrantedRoot::new("r", "R"), Default::default())).unwrap();
        assert_eq!(loaded.playlists.len(), 1);
        assert!(loaded.tracks_by_id.contains_key("Track.mp3"));
        assert_eq!(store.snapshot_tracks().len(), 1, "the scan persisted what it found");
    }

    #[test]
    fn does_not_scan_when_the_store_already_has_data_for_the_root() {
        let store = MemoryLibraryStore::new();
        block_on(store.upsert_track(TrackRecord { file_id: "Old".into(), root_id: "r".into(), relative_path: "Old".into(), size_bytes: 1, last_modified_ms: 0.0, missing: false })).unwrap();
        let loaded = block_on(load_root_library(&fa(), &store, GrantedRoot::new("r", "R"), Default::default())).unwrap();
        assert_eq!(loaded.tracks_by_id.keys().collect::<Vec<_>>(), ["Old"]);
        assert!(loaded.playlists.is_empty());
    }

    #[test]
    fn a_server_backed_root_is_never_scanned_even_when_empty() {
        let store = MemoryLibraryStore::new();
        let loaded = block_on(load_root_library(&fa(), &store, GrantedRoot::new("r", "R"), LoadRootLibraryOptions { scan_if_empty: false })).unwrap();
        assert!(loaded.playlists.is_empty() && loaded.tracks_by_id.is_empty());
    }
}
