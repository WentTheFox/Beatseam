use std::collections::HashMap;
use std::sync::Mutex;

use super::types::{AnalysisResult, LibraryStore, LyricsScope, PlaybackState, PlaylistRecord, StoreError, TrackRecord};
use crate::metadata::{CoverArtBytes, TrackMetadata};

#[derive(Default)]
struct State {
    /// Insertion-ordered, like a JavaScript `Map` (re-setting a key keeps its original position).
    tracks: Vec<TrackRecord>,
    playlists: Vec<PlaylistRecord>,
    analysis: HashMap<String, AnalysisResult>,
    metadata: HashMap<String, TrackMetadata>,
    cover_art: HashMap<String, CoverArtBytes>,
    playback_state: Option<PlaybackState>,
    lyrics_scopes: Vec<LyricsScope>,
    lyrics_assignments: HashMap<String, String>,
    settings: HashMap<String, String>,
}

/// A [`LibraryStore`] that keeps everything in memory: for tests, fixtures and the visual-parity renderer. Cover art is
/// handed back as a `data:` URI. Thread-safe, so it can also stand in on a server-side test.
#[derive(Default)]
pub struct MemoryLibraryStore {
    state: Mutex<State>,
}

impl MemoryLibraryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> Result<R, StoreError> {
        let mut state = self.state.lock().map_err(|_| StoreError("the in-memory store was poisoned by a panic".into()))?;
        Ok(f(&mut state))
    }

    /// Everything in the store, for assertions.
    pub fn snapshot_tracks(&self) -> Vec<TrackRecord> {
        self.state.lock().unwrap().tracks.clone()
    }

    pub fn snapshot_playlists(&self) -> Vec<PlaylistRecord> {
        self.state.lock().unwrap().playlists.clone()
    }
}

impl LibraryStore for MemoryLibraryStore {
    async fn upsert_track(&self, track: TrackRecord) -> Result<(), StoreError> {
        self.with(|s| match s.tracks.iter_mut().find(|t| t.file_id == track.file_id) {
            Some(slot) => *slot = track,
            None => s.tracks.push(track),
        })
    }

    async fn delete_track(&self, file_id: &str) -> Result<(), StoreError> {
        self.with(|s| s.tracks.retain(|t| t.file_id != file_id))
    }

    async fn upsert_playlist(&self, playlist: PlaylistRecord) -> Result<(), StoreError> {
        self.with(|s| match s.playlists.iter_mut().find(|p| p.id == playlist.id) {
            Some(slot) => *slot = playlist,
            None => s.playlists.push(playlist),
        })
    }

    async fn delete_playlist(&self, id: &str) -> Result<(), StoreError> {
        self.with(|s| s.playlists.retain(|p| p.id != id))
    }

    async fn list_tracks(&self, root_id: &str) -> Result<Vec<TrackRecord>, StoreError> {
        self.with(|s| s.tracks.iter().filter(|t| t.root_id == root_id).cloned().collect())
    }

    async fn list_playlists(&self, root_id: &str) -> Result<Vec<PlaylistRecord>, StoreError> {
        self.with(|s| s.playlists.iter().filter(|p| p.root_id == root_id).cloned().collect())
    }

    async fn list_all_tracks(&self) -> Result<Vec<TrackRecord>, StoreError> {
        self.with(|s| s.tracks.clone())
    }

    async fn list_all_playlists(&self) -> Result<Vec<PlaylistRecord>, StoreError> {
        self.with(|s| s.playlists.clone())
    }

    async fn get_analysis(&self, file_id: &str) -> Result<Option<AnalysisResult>, StoreError> {
        self.with(|s| s.analysis.get(file_id).cloned())
    }

    async fn put_analysis(&self, result: AnalysisResult) -> Result<(), StoreError> {
        self.with(|s| drop(s.analysis.insert(result.file_id.clone(), result)))
    }

    async fn get_metadata(&self, file_id: &str) -> Result<Option<TrackMetadata>, StoreError> {
        self.with(|s| s.metadata.get(file_id).cloned())
    }

    async fn put_metadata(&self, result: TrackMetadata) -> Result<(), StoreError> {
        self.with(|s| drop(s.metadata.insert(result.file_id.clone(), result)))
    }

    async fn get_cover_art(&self, file_id: &str) -> Result<Option<String>, StoreError> {
        self.with(|s| s.cover_art.get(file_id).map(|a| format!("data:{};base64,{}", a.mime_type, crate::metadata::encode_base64(&a.data))))
    }

    async fn put_cover_art(&self, file_id: &str, art: Option<CoverArtBytes>) -> Result<(), StoreError> {
        self.with(|s| match art {
            Some(art) => drop(s.cover_art.insert(file_id.to_owned(), art)),
            None => drop(s.cover_art.remove(file_id)),
        })
    }

    async fn get_playback_state(&self) -> Result<Option<PlaybackState>, StoreError> {
        self.with(|s| s.playback_state.clone())
    }

    async fn put_playback_state(&self, state: PlaybackState) -> Result<(), StoreError> {
        self.with(|s| s.playback_state = Some(state))
    }

    async fn get_lyrics_scopes(&self) -> Result<Vec<LyricsScope>, StoreError> {
        self.with(|s| s.lyrics_scopes.clone())
    }

    async fn add_lyrics_scope(&self, scope: LyricsScope) -> Result<(), StoreError> {
        self.with(|s| {
            if !s.lyrics_scopes.contains(&scope) {
                s.lyrics_scopes.push(scope);
            }
        })
    }

    async fn remove_lyrics_scope(&self, root_id: &str, relative_path: &str) -> Result<(), StoreError> {
        self.with(|s| s.lyrics_scopes.retain(|sc| !(sc.root_id == root_id && sc.relative_path == relative_path)))
    }

    async fn get_lyrics_assignment(&self, file_id: &str) -> Result<Option<String>, StoreError> {
        self.with(|s| s.lyrics_assignments.get(file_id).cloned())
    }

    async fn put_lyrics_assignment(&self, file_id: &str, lrc_file_id: Option<String>) -> Result<(), StoreError> {
        self.with(|s| match lrc_file_id {
            Some(lrc) => drop(s.lyrics_assignments.insert(file_id.to_owned(), lrc)),
            None => drop(s.lyrics_assignments.remove(file_id)),
        })
    }

    async fn get_setting(&self, key: &str) -> Result<Option<String>, StoreError> {
        self.with(|s| s.settings.get(key).cloned())
    }

    async fn put_setting(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.with(|s| drop(s.settings.insert(key.to_owned(), value.to_owned())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::block_on;

    fn track(id: &str, root: &str) -> TrackRecord {
        TrackRecord { file_id: id.into(), root_id: root.into(), relative_path: id.into(), size_bytes: 1, last_modified_ms: 0.0, missing: false }
    }

    #[test]
    fn upserting_replaces_in_place_and_keeps_the_original_position() {
        let store = MemoryLibraryStore::new();
        block_on(async {
            for id in ["a", "b", "c"] {
                store.upsert_track(track(id, "r")).await.unwrap();
            }
            store.upsert_track(TrackRecord { size_bytes: 99, ..track("a", "r") }).await.unwrap();
        });
        let tracks = store.snapshot_tracks();
        assert_eq!(tracks.iter().map(|t| (t.file_id.as_str(), t.size_bytes)).collect::<Vec<_>>(), [("a", 99), ("b", 1), ("c", 1)]);
    }

    #[test]
    fn listing_is_per_root_and_deleting_removes_just_that_row() {
        let store = MemoryLibraryStore::new();
        block_on(async {
            store.upsert_track(track("a", "r1")).await.unwrap();
            store.upsert_track(track("b", "r2")).await.unwrap();
            assert_eq!(store.list_tracks("r1").await.unwrap().len(), 1);
            assert_eq!(store.list_all_tracks().await.unwrap().len(), 2);
            store.delete_track("a").await.unwrap();
            assert!(store.list_tracks("r1").await.unwrap().is_empty());
            store.delete_track("nope").await.unwrap(); // deleting a missing row is fine
        });
    }

    #[test]
    fn lyrics_scopes_deduplicate_and_assignments_can_be_cleared() {
        let store = MemoryLibraryStore::new();
        block_on(async {
            let scope = LyricsScope { root_id: "r".into(), relative_path: "Lyrics".into() };
            store.add_lyrics_scope(scope.clone()).await.unwrap();
            store.add_lyrics_scope(scope.clone()).await.unwrap();
            assert_eq!(store.get_lyrics_scopes().await.unwrap(), [scope]);
            store.remove_lyrics_scope("r", "Lyrics").await.unwrap();
            assert!(store.get_lyrics_scopes().await.unwrap().is_empty());

            store.put_lyrics_assignment("t", Some("L:t.lrc".into())).await.unwrap();
            assert_eq!(store.get_lyrics_assignment("t").await.unwrap().as_deref(), Some("L:t.lrc"));
            store.put_lyrics_assignment("t", None).await.unwrap();
            assert_eq!(store.get_lyrics_assignment("t").await.unwrap(), None);
        });
    }

    #[test]
    fn cover_art_comes_back_as_a_data_uri_and_none_clears_it() {
        let store = MemoryLibraryStore::new();
        block_on(async {
            store.put_cover_art("t", Some(CoverArtBytes { mime_type: "image/png".into(), data: b"foo".to_vec() })).await.unwrap();
            assert_eq!(store.get_cover_art("t").await.unwrap().as_deref(), Some("data:image/png;base64,Zm9v"));
            store.put_cover_art("t", None).await.unwrap();
            assert_eq!(store.get_cover_art("t").await.unwrap(), None);
        });
    }

    #[test]
    fn settings_round_trip() {
        let store = MemoryLibraryStore::new();
        block_on(async {
            assert_eq!(store.get_setting("k").await.unwrap(), None);
            store.put_setting("k", "v").await.unwrap();
            assert_eq!(store.get_setting("k").await.unwrap().as_deref(), Some("v"));
        });
    }
}
