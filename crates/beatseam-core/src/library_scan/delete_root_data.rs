use crate::library_store::{LibraryStore, StoreError};

/// Deletes every track and playlist row the store holds for a root - paired with `FileAccess::revoke_root` so removing a
/// root actually cleans up after itself, rather than revoking the OS-level grant while leaving all of that root's rows
/// behind forever (confirmed live: a root re-granted after its old access lapsed left over a thousand orphaned track
/// rows under the old grant, invisible in the UI but still sitting in storage indefinitely).
pub async fn delete_root_library_data<S: LibraryStore>(store: &S, root_id: &str) -> Result<(), StoreError> {
    let tracks = store.list_tracks(root_id).await?;
    let playlists = store.list_playlists(root_id).await?;
    for track in &tracks {
        store.delete_track(&track.file_id).await?;
    }
    for playlist in &playlists {
        store.delete_playlist(&playlist.id).await?;
    }
    Ok(())
}

/// Deletes the store's data for any root id that isn't in `granted_root_ids` - self-heals installs that predate
/// `delete_root_library_data` being wired into root removal (before that, revoking a root's OS-level grant left all of
/// its rows behind forever, still inflating library-wide counts). Meant to run as idle/background startup work, never on
/// the latency-sensitive restore path - a full `list_all_tracks()`/`list_all_playlists()` pass is the same cost class as
/// a library-wide rescan.
///
/// Returns the pruned root ids (first-seen order, tracks before playlists), for logging.
pub async fn prune_orphaned_root_data<S: LibraryStore>(store: &S, granted_root_ids: &[String]) -> Result<Vec<String>, StoreError> {
    let tracks = store.list_all_tracks().await?;
    let playlists = store.list_all_playlists().await?;

    let mut orphaned: Vec<String> = Vec::new();
    for root_id in tracks.iter().map(|t| &t.root_id).chain(playlists.iter().map(|p| &p.root_id)) {
        if !granted_root_ids.contains(root_id) && !orphaned.contains(root_id) {
            orphaned.push(root_id.clone());
        }
    }
    for root_id in &orphaned {
        delete_root_library_data(store, root_id).await?;
    }
    Ok(orphaned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::block_on;
    use crate::library_store::{MemoryLibraryStore, PlaylistRecord, TrackRecord};

    fn track(id: &str, root: &str) -> TrackRecord {
        TrackRecord { file_id: id.into(), root_id: root.into(), relative_path: id.into(), size_bytes: 1, last_modified_ms: 0.0, missing: false }
    }

    fn playlist(id: &str, root: &str) -> PlaylistRecord {
        PlaylistRecord { id: id.into(), root_id: root.into(), file_id: id.into(), name: id.into(), track_file_ids: vec![] }
    }

    fn seed(store: &MemoryLibraryStore) {
        block_on(async {
            for (t, r) in [("a", "r1"), ("b", "r1"), ("c", "r2"), ("d", "r3")] {
                store.upsert_track(track(t, r)).await.unwrap();
            }
            for (p, r) in [("p1", "r1"), ("p2", "r2"), ("p4", "r4")] {
                store.upsert_playlist(playlist(p, r)).await.unwrap();
            }
        });
    }

    fn ids(store: &MemoryLibraryStore) -> (Vec<String>, Vec<String>) {
        (store.snapshot_tracks().into_iter().map(|t| t.file_id).collect(), store.snapshot_playlists().into_iter().map(|p| p.id).collect())
    }

    #[test]
    fn deletes_every_track_and_playlist_of_one_root_and_nothing_else() {
        let store = MemoryLibraryStore::new();
        seed(&store);
        block_on(delete_root_library_data(&store, "r1")).unwrap();
        assert_eq!(ids(&store), (vec!["c".into(), "d".into()], vec!["p2".into(), "p4".into()]));
    }

    #[test]
    fn deleting_a_root_with_no_data_is_fine() {
        let store = MemoryLibraryStore::new();
        seed(&store);
        block_on(delete_root_library_data(&store, "nope")).unwrap();
        assert_eq!(store.snapshot_tracks().len(), 4);
    }

    #[test]
    fn prunes_every_root_that_is_not_granted_and_reports_them() {
        let store = MemoryLibraryStore::new();
        seed(&store);
        let pruned = block_on(prune_orphaned_root_data(&store, &["r1".to_string()])).unwrap();
        // r2 and r3 from tracks, then r4 from playlists only.
        assert_eq!(pruned, ["r2", "r3", "r4"]);
        assert_eq!(ids(&store), (vec!["a".into(), "b".into()], vec!["p1".into()]));
    }

    #[test]
    fn leaves_everything_when_every_root_is_still_granted() {
        let store = MemoryLibraryStore::new();
        seed(&store);
        let granted: Vec<String> = ["r1", "r2", "r3", "r4"].map(String::from).to_vec();
        assert_eq!(block_on(prune_orphaned_root_data(&store, &granted)).unwrap(), Vec::<String>::new());
        assert_eq!(store.snapshot_tracks().len(), 4);
    }
}
