use super::edit_playlist_tracks::find_playlist_file;
use super::error::LibraryError;
use crate::file_access::{CallOptions, FileAccess};
use crate::library_store::{PlaylistRecord, TrackRecord};
use crate::playlist::{M3u8Entry, format_m3u8, parse_m3u8, relativize_m3u8_entry_path};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AddPosition {
    Start,
    #[default]
    End,
}

/// Adds tracks to the start or end of an existing real playlist's own `.m3u8`, expressed relative to the playlist's own
/// location (a playlist references tracks relative to itself, not the library root). Built for the Unplaylisted
/// automatic view's "add to playlist" action, but works on any real playlist.
///
/// Deliberately doesn't touch the library store itself, like the other playlist writers - the caller rescans the root
/// afterwards, so the playlist's persisted track ids go through the exact same ingestion path as any other
/// externally-edited `.m3u8` rather than a second, parallel way of updating the store.
///
/// Doesn't de-duplicate against the playlist's existing entries - the one current caller can only ever offer tracks that
/// aren't already in any playlist, so a duplicate here would mean the library was rescanned out from under the dialog.
///
/// Requires `FileAccess::write_file_text`.
pub async fn add_tracks_to_playlist<F: FileAccess>(file_access: &F, root_id: &str, playlist: &PlaylistRecord, tracks_to_add: &[TrackRecord], position: AddPosition) -> Result<(), LibraryError> {
    if tracks_to_add.is_empty() {
        return Ok(());
    }

    let playlist_file = find_playlist_file(file_access, root_id, playlist).await?;
    let existing = parse_m3u8(&file_access.read_file_text(&playlist_file, CallOptions::default()).await?);
    let new_entries = tracks_to_add.iter().map(|t| M3u8Entry::path(relativize_m3u8_entry_path(&playlist_file.relative_path, &t.relative_path)));
    let merged: Vec<M3u8Entry> = match position {
        AddPosition::Start => new_entries.chain(existing).collect(),
        AddPosition::End => existing.into_iter().chain(new_entries).collect(),
    };
    file_access.write_file_text(root_id, &playlist_file.relative_path, &format_m3u8(&merged)).await?;
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

    fn playlist(file_id: &str, name: &str) -> PlaylistRecord {
        PlaylistRecord { id: "p1".into(), root_id: "root-1".into(), file_id: file_id.into(), name: name.into(), track_file_ids: vec![] }
    }

    fn raw_paths(fa: &MemoryFileAccess, file: &str) -> Vec<String> {
        parse_m3u8(&fa.contents()[file]).into_iter().map(|e| e.raw_path).collect()
    }

    #[test]
    fn appends_tracks_to_the_end_relative_to_the_playlist_files_own_location() {
        let fa = MemoryFileAccess::new([("Mixes/Party.m3u8", "#EXTM3U\nTrack A.mp3")]);
        block_on(add_tracks_to_playlist(&fa, "root-1", &playlist("Mixes/Party.m3u8", "Party"), &[track("Mixes/Track B.mp3"), track("Loose/Track C.mp3")], AddPosition::End)).unwrap();
        assert_eq!(raw_paths(&fa, "Mixes/Party.m3u8"), ["Track A.mp3", "./Track B.mp3", "../Loose/Track C.mp3"]);
    }

    #[test]
    fn inserts_tracks_at_the_start_when_asked() {
        let fa = MemoryFileAccess::new([("Party.m3u8", "#EXTM3U\nTrack A.mp3")]);
        block_on(add_tracks_to_playlist(&fa, "root-1", &playlist("Party.m3u8", "Party"), &[track("Track B.mp3")], AddPosition::Start)).unwrap();
        assert_eq!(raw_paths(&fa, "Party.m3u8"), ["./Track B.mp3", "Track A.mp3"]);
    }

    #[test]
    fn an_empty_track_list_is_a_no_op_that_does_not_even_look_for_the_file() {
        let fa = MemoryFileAccess::new([("Party.m3u8", "#EXTM3U\nTrack A.mp3")]);
        block_on(add_tracks_to_playlist(&fa, "root-1", &playlist("Gone.m3u8", "Gone"), &[], AddPosition::End)).unwrap();
        assert_eq!(fa.contents()["Party.m3u8"], "#EXTM3U\nTrack A.mp3");
    }

    #[test]
    fn fails_with_playlist_file_not_found_when_the_playlist_file_is_gone() {
        let fa = MemoryFileAccess::new([("Track A.mp3", "fake-audio")]);
        let result = block_on(add_tracks_to_playlist(&fa, "root-1", &playlist("Gone.m3u8", "Gone"), &[track("Track A.mp3")], AddPosition::End));
        assert_eq!(result, Err(LibraryError::PlaylistFileNotFound { playlist_name: "Gone".into() }));
    }
}
