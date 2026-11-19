//! Port of `store/library`: the scanned library as the screens see it.

use std::collections::{HashMap, HashSet};

use beatseam_core::file_access::GrantedRoot;
use beatseam_core::library_store::{LyricsScope, PlaylistRecord, TrackRecord};

/// The serializable form held in state - tracks as a list, looked up by id through [`RootWithLibrary`].
#[derive(Clone, Debug, PartialEq)]
pub struct LibraryRootData {
    pub root: GrantedRoot,
    pub playlists: Vec<PlaylistRecord>,
    pub tracks: Vec<TrackRecord>,
}

/// A granted library root with its scanned playlists and tracks, `tracks_by_id` as the map the UI looks tracks up in.
#[derive(Clone, Debug, PartialEq)]
pub struct RootWithLibrary {
    pub root: GrantedRoot,
    pub playlists: Vec<PlaylistRecord>,
    pub tracks_by_id: HashMap<String, TrackRecord>,
}

impl From<&LibraryRootData> for RootWithLibrary {
    fn from(data: &LibraryRootData) -> Self {
        Self { root: data.root.clone(), playlists: data.playlists.clone(), tracks_by_id: data.tracks.iter().map(|t| (t.file_id.clone(), t.clone())).collect() }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LibraryState {
    /// Every granted root, including lyrics-only ones.
    pub granted_roots: Vec<GrantedRoot>,
    /// Scanned data for the library-kind roots only.
    pub roots: Vec<LibraryRootData>,
    /// True until the first refresh resolves `roots` at least once. A refresh is fire-and-forget from the startup restore (which
    /// must not wait on scanning every root), so the restoring screen dismisses well before this finishes, and the library
    /// screen needs it to show a loading state rather than a misleading empty one.
    pub is_loading_roots: bool,
    /// Tracks whose file failed to open this session.
    pub missing_file_ids: Vec<String>,
    pub lyrics_scopes: Vec<LyricsScope>,
    /// `None` until known. Seeded from last launch's final count, then ticked live.
    pub matched_lyrics_count: Option<u32>,
    /// The direct-action error banner (add folder, rescan, etc.); one-off playback errors go to notifications instead.
    pub error: Option<String>,
    /// Once-per-launch guard for pruning orphaned root data: cheap insurance, not worth a full scan on every manual refresh.
    pub orphan_prune_started: bool,
}

impl Default for LibraryState {
    fn default() -> Self {
        Self { granted_roots: Vec::new(), roots: Vec::new(), is_loading_roots: true, missing_file_ids: Vec::new(), lyrics_scopes: Vec::new(), matched_lyrics_count: None, error: None, orphan_prune_started: false }
    }
}

impl LibraryState {
    pub fn granted_roots_loaded(&mut self, roots: Vec<GrantedRoot>) {
        self.granted_roots = roots;
    }

    pub fn library_loaded(&mut self, roots: Vec<LibraryRootData>) {
        self.roots = roots;
        self.is_loading_roots = false;
    }

    /// Listing the granted roots failed - otherwise `is_loading_roots` would stay true forever.
    pub fn roots_load_failed(&mut self) {
        self.is_loading_roots = false;
    }

    pub fn orphan_prune_started(&mut self) {
        self.orphan_prune_started = true;
    }

    pub fn lyrics_scopes_loaded(&mut self, scopes: Vec<LyricsScope>) {
        self.lyrics_scopes = scopes;
    }

    pub fn matched_lyrics_count_set(&mut self, count: Option<u32>) {
        self.matched_lyrics_count = count;
    }

    /// The match pass found a track without a match: tick the optimistic count down (never below 0).
    pub fn matched_lyrics_count_decremented(&mut self) {
        if let Some(count) = &mut self.matched_lyrics_count {
            *count = count.saturating_sub(1);
        }
    }

    pub fn library_error_set(&mut self, error: Option<String>) {
        self.error = error;
    }

    pub fn file_missing_reported(&mut self, file_id: &str) {
        if !self.missing_file_ids.iter().any(|id| id == file_id) {
            self.missing_file_ids.push(file_id.to_owned());
        }
    }

    pub fn file_found_reported(&mut self, file_id: &str) {
        self.missing_file_ids.retain(|id| id != file_id);
    }

    /// Rebuilds the per-root lookup maps the UI uses.
    pub fn roots_with_library(&self) -> Vec<RootWithLibrary> {
        self.roots.iter().map(RootWithLibrary::from).collect()
    }

    pub fn missing_file_id_set(&self) -> HashSet<&str> {
        self.missing_file_ids.iter().map(String::as_str).collect()
    }
}

#[cfg(test)]
mod tests {
    use beatseam_core::file_access::RootKind;

    use super::*;

    fn root(id: &str) -> GrantedRoot {
        GrantedRoot { id: id.into(), display_name: id.into(), kind: RootKind::Library, removable: true }
    }

    fn track(id: &str) -> TrackRecord {
        TrackRecord { file_id: id.into(), root_id: "r".into(), relative_path: id.into(), size_bytes: 1, last_modified_ms: 1.0, missing: false }
    }

    #[test]
    fn loading_is_true_until_a_library_arrives_or_the_listing_fails() {
        let mut state = LibraryState::default();
        assert!(state.is_loading_roots);
        state.library_loaded(vec![]);
        assert!(!state.is_loading_roots);
        let mut failed = LibraryState::default();
        failed.roots_load_failed();
        assert!(!failed.is_loading_roots);
    }

    #[test]
    fn roots_come_with_a_track_lookup_by_id() {
        let mut state = LibraryState::default();
        state.library_loaded(vec![LibraryRootData { root: root("r"), playlists: vec![], tracks: vec![track("a"), track("b")] }]);
        let with = state.roots_with_library();
        assert_eq!(with.len(), 1);
        assert_eq!(with[0].tracks_by_id.len(), 2);
        assert_eq!(with[0].tracks_by_id["b"].file_id, "b");
    }

    #[test]
    fn missing_files_are_a_set_that_can_be_reported_and_cleared() {
        let mut state = LibraryState::default();
        state.file_missing_reported("a");
        state.file_missing_reported("a");
        state.file_missing_reported("b");
        assert_eq!(state.missing_file_ids, ["a", "b"]);
        state.file_found_reported("a");
        assert_eq!(state.missing_file_id_set().into_iter().collect::<Vec<_>>(), ["b"]);
    }

    #[test]
    fn the_optimistic_lyrics_count_ticks_down_but_never_below_zero_or_from_unknown() {
        let mut state = LibraryState::default();
        state.matched_lyrics_count_decremented();
        assert_eq!(state.matched_lyrics_count, None);
        state.matched_lyrics_count_set(Some(1));
        state.matched_lyrics_count_decremented();
        state.matched_lyrics_count_decremented();
        assert_eq!(state.matched_lyrics_count, Some(0));
    }

    #[test]
    fn simple_setters_store_what_they_are_given() {
        let mut state = LibraryState::default();
        state.granted_roots_loaded(vec![root("r")]);
        state.lyrics_scopes_loaded(vec![LyricsScope { root_id: "r".into(), relative_path: "L".into() }]);
        state.library_error_set(Some("boom".into()));
        state.orphan_prune_started();
        assert_eq!((state.granted_roots.len(), state.lyrics_scopes.len(), state.error.as_deref(), state.orphan_prune_started), (1, 1, Some("boom"), true));
    }
}
