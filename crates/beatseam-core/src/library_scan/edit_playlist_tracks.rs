use std::collections::HashMap;

use super::error::LibraryError;
use super::walk::walk_directory;
use crate::file_access::{CallOptions, FileAccess, FileRef};
use crate::library_store::{PlaylistRecord, TrackRecord};
use crate::playlist::{M3u8Entry, format_m3u8, parse_m3u8, resolve_m3u8_entry_path};

/// Re-finds a playlist's own `.m3u8` file by walking the root fresh and matching `playlist.file_id`, so every writer
/// (add, move, remove, and the raw editor) shares one copy of this instead of re-deriving it. Fails with
/// [`LibraryError::PlaylistFileNotFound`] if the id no longer matches anything on disk.
pub async fn find_playlist_file<F: FileAccess>(file_access: &F, root_id: &str, playlist: &PlaylistRecord) -> Result<FileRef, LibraryError> {
    let walked = walk_directory(file_access, root_id, None, None, None).await?;
    walked
        .playlist_files
        .into_iter()
        .find(|f| f.id == playlist.file_id)
        .ok_or_else(|| LibraryError::PlaylistFileNotFound { playlist_name: playlist.name.clone() })
}

/// Pairs each parsed entry with the file id it resolves to, via the same path resolution a scan itself uses, matched
/// against a relative-path -> file-id map built from the caller's already-known tracks (a fresh walk alone isn't enough
/// here - file id identity, not just relative path, is what the caller's selection is expressed in). An entry whose
/// resolved path doesn't match any known track (e.g. the file was deleted since the last scan) gets `None` - left alone
/// by both operations, rather than silently dropped.
fn resolve_entries(playlist_file: &FileRef, entries: Vec<M3u8Entry>, tracks_by_id: &HashMap<String, TrackRecord>) -> Vec<(M3u8Entry, Option<String>)> {
    let mut file_id_by_path: HashMap<&str, &str> = HashMap::new();
    for track in tracks_by_id.values() {
        file_id_by_path.insert(track.relative_path.as_str(), track.file_id.as_str());
    }
    entries
        .into_iter()
        .map(|entry| {
            let path = resolve_m3u8_entry_path(&playlist_file.relative_path, &entry.raw_path);
            let file_id = file_id_by_path.get(path.as_str()).map(|id| (*id).to_owned());
            (entry, file_id)
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MovePosition {
    Before,
    After,
}

/// Moves one or more existing entries of a real playlist's `.m3u8` to just before or after another entry (the "anchor"),
/// identified by file id - the building block behind the track list's reorder mode. The moved entries keep their own
/// relative order *in the playlist* (not the order they were selected in); everything else keeps its relative order too,
/// as if the moved block had simply been lifted out and reinserted at the anchor. A moved entry keeps its exact original
/// text: a move only repositions, it never rewrites a line.
///
/// Doesn't touch the library store: the caller rescans the root afterwards so the persisted order comes back through
/// the normal ingestion path, not a second parallel one. Requires `FileAccess::write_file_text`.
pub async fn move_playlist_tracks<F: FileAccess>(
    file_access: &F,
    root_id: &str,
    playlist: &PlaylistRecord,
    tracks_by_id: &HashMap<String, TrackRecord>,
    moving_file_ids: &[String],
    anchor_file_id: &str,
    position: MovePosition,
) -> Result<(), LibraryError> {
    if moving_file_ids.is_empty() {
        return Err(LibraryError::InvalidPlaylistMove("No tracks selected to move.".into()));
    }
    if moving_file_ids.iter().any(|id| id == anchor_file_id) {
        return Err(LibraryError::InvalidPlaylistMove("Cannot move a track to a position anchored on itself.".into()));
    }

    let playlist_file = find_playlist_file(file_access, root_id, playlist).await?;
    let entries = parse_m3u8(&file_access.read_file_text(&playlist_file, CallOptions::default()).await?);
    let resolved = resolve_entries(&playlist_file, entries, tracks_by_id);

    let is_moving = |file_id: &Option<String>| file_id.as_ref().is_some_and(|id| moving_file_ids.contains(id));
    let (moving, remaining): (Vec<_>, Vec<_>) = resolved.into_iter().partition(|(_, id)| is_moving(id));

    let anchor_index = remaining
        .iter()
        .position(|(_, id)| id.as_deref() == Some(anchor_file_id))
        .ok_or_else(|| LibraryError::InvalidPlaylistMove("The anchor track is no longer in this playlist.".into()))?;
    let insert_at = match position {
        MovePosition::Before => anchor_index,
        MovePosition::After => anchor_index + 1,
    };

    let mut result: Vec<M3u8Entry> = remaining.iter().take(insert_at).map(|(e, _)| e.clone()).collect();
    result.extend(moving.into_iter().map(|(e, _)| e));
    result.extend(remaining.into_iter().skip(insert_at).map(|(e, _)| e));
    file_access.write_file_text(root_id, &playlist_file.relative_path, &format_m3u8(&result)).await?;
    Ok(())
}

/// Removes one or more existing entries from a real playlist's `.m3u8`, identified by file id - the other half of the
/// track list's reorder mode (see [`move_playlist_tracks`] for the shared conventions, including not touching the store).
/// Entries that don't resolve to a known track are left alone.
pub async fn remove_playlist_tracks<F: FileAccess>(
    file_access: &F,
    root_id: &str,
    playlist: &PlaylistRecord,
    tracks_by_id: &HashMap<String, TrackRecord>,
    removing_file_ids: &[String],
) -> Result<(), LibraryError> {
    if removing_file_ids.is_empty() {
        return Ok(());
    }

    let playlist_file = find_playlist_file(file_access, root_id, playlist).await?;
    let entries = parse_m3u8(&file_access.read_file_text(&playlist_file, CallOptions::default()).await?);
    let resolved = resolve_entries(&playlist_file, entries, tracks_by_id);

    let result: Vec<M3u8Entry> = resolved.into_iter().filter(|(_, id)| !id.as_ref().is_some_and(|id| removing_file_ids.contains(id))).map(|(e, _)| e).collect();
    file_access.write_file_text(root_id, &playlist_file.relative_path, &format_m3u8(&result)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_access::MemoryFileAccess;
    use crate::http::block_on;

    fn track(path: &str) -> TrackRecord {
        TrackRecord { file_id: path.into(), root_id: "root-1".into(), relative_path: path.into(), size_bytes: 0, last_modified_ms: 0.0, missing: false }
    }

    fn ids(order: &[&str]) -> Vec<String> {
        order.iter().map(|s| s.to_string()).collect()
    }

    /// A playlist "Party.m3u8" listing `order`, and a tracks map covering exactly those paths.
    fn setup(order: &[&str]) -> (MemoryFileAccess, PlaylistRecord, HashMap<String, TrackRecord>) {
        let fa = MemoryFileAccess::new([("Party.m3u8", format!("#EXTM3U\n{}", order.join("\n")).as_str())]);
        let playlist = PlaylistRecord { id: "p1".into(), root_id: "root-1".into(), file_id: "Party.m3u8".into(), name: "Party".into(), track_file_ids: ids(order) };
        (fa, playlist, order.iter().map(|p| (p.to_string(), track(p))).collect())
    }

    fn written_paths(fa: &MemoryFileAccess) -> Vec<String> {
        parse_m3u8(&fa.contents()["Party.m3u8"]).into_iter().map(|e| e.raw_path).collect()
    }

    fn mv(fa: &MemoryFileAccess, p: &PlaylistRecord, t: &HashMap<String, TrackRecord>, moving: &[&str], anchor: &str, pos: MovePosition) -> Result<(), LibraryError> {
        block_on(move_playlist_tracks(fa, "root-1", p, t, &ids(moving), anchor, pos))
    }

    #[test]
    fn moves_a_single_track_before_or_after_an_anchor() {
        let (fa, p, t) = setup(&["A.mp3", "B.mp3", "C.mp3"]);
        mv(&fa, &p, &t, &["C.mp3"], "A.mp3", MovePosition::Before).unwrap();
        assert_eq!(written_paths(&fa), ["C.mp3", "A.mp3", "B.mp3"]);

        let (fa, p, t) = setup(&["A.mp3", "B.mp3", "C.mp3"]);
        mv(&fa, &p, &t, &["A.mp3"], "B.mp3", MovePosition::After).unwrap();
        assert_eq!(written_paths(&fa), ["B.mp3", "A.mp3", "C.mp3"]);
    }

    #[test]
    fn moves_several_non_contiguous_tracks_together_keeping_their_original_relative_order() {
        let (fa, p, t) = setup(&["A.mp3", "B.mp3", "C.mp3", "D.mp3", "E.mp3"]);
        // Selected D then B, but they land as B, D (their order in the playlist) after C.
        mv(&fa, &p, &t, &["D.mp3", "B.mp3"], "C.mp3", MovePosition::After).unwrap();
        assert_eq!(written_paths(&fa), ["A.mp3", "C.mp3", "B.mp3", "D.mp3", "E.mp3"]);
    }

    #[test]
    fn refuses_to_move_nothing_or_to_anchor_a_track_on_itself_and_writes_nothing() {
        let (fa, p, t) = setup(&["A.mp3", "B.mp3"]);
        assert!(matches!(mv(&fa, &p, &t, &[], "A.mp3", MovePosition::Before), Err(LibraryError::InvalidPlaylistMove(_))));
        assert!(matches!(mv(&fa, &p, &t, &["A.mp3"], "A.mp3", MovePosition::Before), Err(LibraryError::InvalidPlaylistMove(_))));
        assert_eq!(fa.contents()["Party.m3u8"], "#EXTM3U\nA.mp3\nB.mp3", "untouched");
    }

    #[test]
    fn an_anchor_that_is_no_longer_in_the_playlist_is_an_invalid_move() {
        let (fa, p, t) = setup(&["A.mp3", "B.mp3"]);
        assert_eq!(mv(&fa, &p, &t, &["A.mp3"], "Zzz.mp3", MovePosition::Before), Err(LibraryError::InvalidPlaylistMove("The anchor track is no longer in this playlist.".into())));
    }

    #[test]
    fn leaves_an_entry_that_does_not_resolve_to_a_known_track_untouched_and_in_place() {
        let fa = MemoryFileAccess::new([("Party.m3u8", "#EXTM3U\nA.mp3\nGhost.mp3\nB.mp3")]);
        let p = PlaylistRecord { id: "p1".into(), root_id: "root-1".into(), file_id: "Party.m3u8".into(), name: "Party".into(), track_file_ids: ids(&["A.mp3", "B.mp3"]) };
        let t: HashMap<_, _> = [("A.mp3".to_string(), track("A.mp3")), ("B.mp3".to_string(), track("B.mp3"))].into();
        mv(&fa, &p, &t, &["B.mp3"], "A.mp3", MovePosition::Before).unwrap();
        assert_eq!(written_paths(&fa), ["B.mp3", "A.mp3", "Ghost.mp3"]);

        let fa = MemoryFileAccess::new([("Party.m3u8", "#EXTM3U\nA.mp3\nGhost.mp3\nB.mp3")]);
        block_on(remove_playlist_tracks(&fa, "root-1", &p, &t, &ids(&["A.mp3"]))).unwrap();
        assert_eq!(written_paths(&fa), ["Ghost.mp3", "B.mp3"]);
    }

    #[test]
    fn a_moved_entry_keeps_its_exact_text_and_extinf() {
        let fa = MemoryFileAccess::new([("Party.m3u8", "#EXTM3U\n#EXTINF:100,One\n./A.mp3\nB.mp3")]);
        let p = PlaylistRecord { id: "p1".into(), root_id: "root-1".into(), file_id: "Party.m3u8".into(), name: "Party".into(), track_file_ids: vec![] };
        let t: HashMap<_, _> = [("A.mp3".to_string(), track("A.mp3")), ("B.mp3".to_string(), track("B.mp3"))].into();
        mv(&fa, &p, &t, &["A.mp3"], "B.mp3", MovePosition::After).unwrap();
        assert_eq!(fa.contents()["Party.m3u8"], "#EXTM3U\nB.mp3\n#EXTINF:100,One\n./A.mp3\n");
    }

    #[test]
    fn removes_the_given_tracks_preserving_the_order_of_the_rest() {
        let (fa, p, t) = setup(&["A.mp3", "B.mp3", "C.mp3"]);
        block_on(remove_playlist_tracks(&fa, "root-1", &p, &t, &ids(&["B.mp3"]))).unwrap();
        assert_eq!(written_paths(&fa), ["A.mp3", "C.mp3"]);
    }

    #[test]
    fn removing_nothing_writes_nothing() {
        let (fa, p, t) = setup(&["A.mp3", "B.mp3"]);
        block_on(remove_playlist_tracks(&fa, "root-1", &p, &t, &[])).unwrap();
        assert_eq!(fa.contents()["Party.m3u8"], "#EXTM3U\nA.mp3\nB.mp3");
    }

    #[test]
    fn fails_with_playlist_file_not_found_when_the_playlist_file_is_gone() {
        let fa = MemoryFileAccess::new([("A.mp3", "fake-audio")]);
        let p = PlaylistRecord { id: "p1".into(), root_id: "root-1".into(), file_id: "Gone.m3u8".into(), name: "Gone".into(), track_file_ids: vec![] };
        let t: HashMap<_, _> = [("A.mp3".to_string(), track("A.mp3"))].into();
        let expected = Err(LibraryError::PlaylistFileNotFound { playlist_name: "Gone".into() });
        assert_eq!(mv(&fa, &p, &t, &["A.mp3"], "B.mp3", MovePosition::Before), expected);
        assert_eq!(block_on(remove_playlist_tracks(&fa, "root-1", &p, &t, &ids(&["A.mp3"]))), expected);
        assert!(expected.clone().unwrap_err().to_string().contains("Could not find the playlist file for \"Gone\""));
    }
}
