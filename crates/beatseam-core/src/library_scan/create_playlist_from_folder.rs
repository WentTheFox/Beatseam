use std::cmp::Ordering;

use super::audio_files::is_audio_file_name;
use super::error::LibraryError;
use super::walk::walk_directory;
use crate::file_access::{CallOptions, FileAccess, FileRef};
use crate::js_compat::{js_locale_compare, js_trim};
use crate::metadata::TagReader;
use crate::playlist::{M3u8Entry, format_m3u8};

const PLAYLIST_EXTENSIONS: [&str; 2] = [".m3u8", ".m3u"];

fn has_extension(name: &str, extensions: &[&str]) -> bool {
    let lower = name.to_lowercase();
    extensions.iter().any(|ext| lower.ends_with(ext))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaylistSortCriterion {
    DateCreated,
    Title,
    Artist,
    Album,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaylistSortOrder {
    Asc,
    Desc,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlaylistCandidateTrack {
    pub file: FileRef,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
}

/// Recursively finds every audio-looking file under `folder_relative_path` (`""` for the root itself) and reads each
/// one's tags, so a caller can offer sort-by-title/artist/album and a preview before the playlist is actually written.
/// Unlike a scan (which only ever treats "files referenced by an existing playlist" as tracks), this is the one place
/// Beatseam looks at a folder's audio files directly. Playlist files themselves are excluded (a `.m3u8` already sitting
/// in the folder shouldn't get "included" in a new one built from it). A file whose tags fail to read (corrupt,
/// unsupported, or unreadable) still gets included, just with no title/artist/album - falling back to its filename for
/// sorting/display is the caller's job (see [`sort_playlist_candidates`]), not a reason to drop it from the playlist.
pub async fn find_playlist_candidate_tracks<F: FileAccess, T: TagReader>(file_access: &F, root_id: &str, folder_relative_path: &str, tag_reader: &T) -> Result<Vec<PlaylistCandidateTrack>, LibraryError> {
    let walked = walk_directory(file_access, root_id, Some(folder_relative_path), None, None).await?;
    let mut candidates = Vec::new();
    for file in walked.files.into_iter().filter(|f| is_audio_file_name(&f.name) && !has_extension(&f.name, &PLAYLIST_EXTENSIONS)) {
        let tags = match file_access.read_file_bytes(&file, CallOptions::default()).await {
            Ok(bytes) => tag_reader.read_tags(&bytes),
            Err(_) => None,
        };
        candidates.push(match tags {
            Some(tags) => PlaylistCandidateTrack {
                title: tags.title,
                artist: (!tags.artists.is_empty()).then(|| tags.artists.join(", ")),
                album: tags.album,
                file,
            },
            None => PlaylistCandidateTrack { file, title: None, artist: None, album: None },
        });
    }
    Ok(candidates)
}

/// Pure sort, kept separate from [`find_playlist_candidate_tracks`] so the UI can re-sort instantly (switching sort
/// criterion/order) without re-reading every file's tags again. Falls back to the filename for title, and to an empty
/// string (sorting to one end) for artist/album, when a file has no such tag - `DateCreated` actually means
/// `FileRef::last_modified_ms`, the only timestamp any `FileAccess` adapter exposes; no platform surfaces a true
/// file-creation time.
///
/// Text comparison uses [`js_locale_compare`], an approximation of the TypeScript's `localeCompare`.
pub fn sort_playlist_candidates(tracks: &[PlaylistCandidateTrack], sort_by: PlaylistSortCriterion, order: PlaylistSortOrder) -> Vec<PlaylistCandidateTrack> {
    let compare = |a: &PlaylistCandidateTrack, b: &PlaylistCandidateTrack| -> Ordering {
        match sort_by {
            PlaylistSortCriterion::DateCreated => a.file.last_modified_ms.total_cmp(&b.file.last_modified_ms),
            PlaylistSortCriterion::Title => js_locale_compare(a.title.as_deref().unwrap_or(&a.file.name), b.title.as_deref().unwrap_or(&b.file.name)),
            PlaylistSortCriterion::Artist => js_locale_compare(a.artist.as_deref().unwrap_or(""), b.artist.as_deref().unwrap_or("")),
            PlaylistSortCriterion::Album => js_locale_compare(a.album.as_deref().unwrap_or(""), b.album.as_deref().unwrap_or("")),
        }
    };
    let mut sorted = tracks.to_vec();
    sorted.sort_by(compare); // stable, like Array.prototype.sort
    if order == PlaylistSortOrder::Desc {
        sorted.reverse();
    }
    sorted
}

/// Strips illegal filename characters common to Windows/Android/web filesystems, falling back to a generic name if
/// nothing usable is left.
fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = js_trim(name).chars().map(|c| if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c }).collect();
    if cleaned.is_empty() { "Playlist".to_owned() } else { cleaned }
}

/// Writes a new `.m3u8` directly into `folder_relative_path` (the same folder the tracks were found under), referencing
/// `tracks` in the order given - sort them with [`sort_playlist_candidates`] first. Returns the written playlist's
/// relative path. Deliberately doesn't touch the library store itself: the caller should rescan the root afterwards, so
/// the new playlist goes through the exact same ingestion path as any other externally-added `.m3u8` rather than
/// needing its own parallel logic.
pub async fn write_playlist_file<F: FileAccess>(file_access: &F, root_id: &str, folder_relative_path: &str, name: &str, tracks: &[PlaylistCandidateTrack]) -> Result<String, LibraryError> {
    let file_name = format!("{}.m3u8", sanitize_file_name(name));
    let relative_path = if folder_relative_path.is_empty() { file_name } else { format!("{folder_relative_path}/{file_name}") };
    let folder_prefix = if folder_relative_path.is_empty() { String::new() } else { format!("{folder_relative_path}/") };
    let entries: Vec<M3u8Entry> = tracks
        .iter()
        .map(|t| {
            // Relative to the playlist's own location (the same folder it's written into) - a plain filename for a track
            // directly inside that folder, a subpath for one found deeper during the recursive walk.
            let path = &t.file.relative_path;
            M3u8Entry::path(path.strip_prefix(folder_prefix.as_str()).unwrap_or(path))
        })
        .collect();
    file_access.write_file_text(root_id, &relative_path, &format_m3u8(&entries)).await?;
    Ok(relative_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_access::MemoryFileAccess;
    use crate::http::block_on;
    use crate::metadata::ParsedTags;
    use crate::playlist::parse_m3u8;

    fn track(path: &str) -> PlaylistCandidateTrack {
        let name = path.rsplit('/').next().unwrap();
        PlaylistCandidateTrack { file: FileRef { id: path.into(), name: name.into(), relative_path: path.into(), size_bytes: 0, last_modified_ms: 0.0 }, title: None, artist: None, album: None }
    }

    fn titled(path: &str, title: &str) -> PlaylistCandidateTrack {
        PlaylistCandidateTrack { title: Some(title.into()), ..track(path) }
    }

    fn paths(tracks: &[PlaylistCandidateTrack]) -> Vec<&str> {
        tracks.iter().map(|t| t.file.relative_path.as_str()).collect()
    }

    #[test]
    fn sorts_by_title_falling_back_to_the_filename_when_a_track_has_no_title_tag() {
        let tracks = [titled("c.mp3", "Charlie"), titled("a.mp3", "Alpha"), track("b.mp3")];
        assert_eq!(paths(&sort_playlist_candidates(&tracks, PlaylistSortCriterion::Title, PlaylistSortOrder::Asc)), ["a.mp3", "b.mp3", "c.mp3"]);
    }

    #[test]
    fn reverses_the_order_for_desc() {
        let tracks = [titled("a.mp3", "Alpha"), titled("b.mp3", "Beta")];
        assert_eq!(paths(&sort_playlist_candidates(&tracks, PlaylistSortCriterion::Title, PlaylistSortOrder::Desc)), ["b.mp3", "a.mp3"]);
    }

    #[test]
    fn sorts_by_date_using_last_modified() {
        let mut newer = track("newer.mp3");
        newer.file.last_modified_ms = 200.0;
        let mut older = track("older.mp3");
        older.file.last_modified_ms = 100.0;
        assert_eq!(paths(&sort_playlist_candidates(&[newer, older], PlaylistSortCriterion::DateCreated, PlaylistSortOrder::Asc)), ["older.mp3", "newer.mp3"]);
    }

    #[test]
    fn sorts_by_artist_with_untagged_tracks_first() {
        let with_artist = PlaylistCandidateTrack { artist: Some("Zeta".into()), ..track("withArtist.mp3") };
        assert_eq!(paths(&sort_playlist_candidates(&[with_artist, track("untagged.mp3")], PlaylistSortCriterion::Artist, PlaylistSortOrder::Asc)), ["untagged.mp3", "withArtist.mp3"]);
    }

    #[test]
    fn sorting_does_not_mutate_the_input_and_is_stable_for_equal_keys() {
        let tracks = [titled("b.mp3", "Same"), titled("a.mp3", "Same"), titled("c.mp3", "Other")];
        let sorted = sort_playlist_candidates(&tracks, PlaylistSortCriterion::Title, PlaylistSortOrder::Asc);
        assert_eq!(paths(&sorted), ["c.mp3", "b.mp3", "a.mp3"]);
        assert_eq!(paths(&tracks), ["b.mp3", "a.mp3", "c.mp3"]);
    }

    #[test]
    fn writes_an_m3u8_with_paths_relative_to_the_folder_for_a_folder_at_the_root() {
        let fa = MemoryFileAccess::default();
        let written = block_on(write_playlist_file(&fa, "root-1", "", "My Mix", &[track("Track A.mp3"), track("Sub/Track B.mp3")])).unwrap();
        assert_eq!(written, "My Mix.m3u8");
        let raw: Vec<String> = parse_m3u8(&fa.contents()["My Mix.m3u8"]).into_iter().map(|e| e.raw_path).collect();
        assert_eq!(raw, ["Track A.mp3", "Sub/Track B.mp3"]);
    }

    #[test]
    fn writes_into_a_nested_folder_stripping_that_folder_prefix_from_each_track_path() {
        let fa = MemoryFileAccess::default();
        let written = block_on(write_playlist_file(&fa, "root-1", "Music/Album", "Album Mix", &[track("Music/Album/Track A.mp3"), track("Music/Album/Disc2/Track B.mp3")])).unwrap();
        assert_eq!(written, "Music/Album/Album Mix.m3u8");
        let raw: Vec<String> = parse_m3u8(&fa.contents()[written.as_str()]).into_iter().map(|e| e.raw_path).collect();
        assert_eq!(raw, ["Track A.mp3", "Disc2/Track B.mp3"]);
    }

    #[test]
    fn sanitizes_illegal_filename_characters_out_of_the_playlist_name() {
        let fa = MemoryFileAccess::default();
        assert_eq!(block_on(write_playlist_file(&fa, "root-1", "", "Weekend: Party/Mix?", &[track("Track.mp3")])).unwrap(), "Weekend_ Party_Mix_.m3u8");
        assert_eq!(block_on(write_playlist_file(&fa, "root-1", "", "  \t ", &[track("Track.mp3")])).unwrap(), "Playlist.m3u8");
    }

    #[test]
    fn finds_audio_files_recursively_reads_their_tags_and_keeps_untaggable_ones() {
        let fa = MemoryFileAccess::new([("Album/one.mp3", "TAGGED"), ("Album/Disc2/two.flac", "BROKEN"), ("Album/mix.m3u8", "x"), ("Album/cover.jpg", "x"), ("Other/three.mp3", "TAGGED")]);
        let reader = |bytes: &[u8]| (bytes == b"TAGGED").then(|| ParsedTags { title: Some("T".into()), artists: vec!["A".into(), "B".into()], album: Some("Al".into()), cover_art: None });
        let mut found = block_on(find_playlist_candidate_tracks(&fa, "r", "Album", &reader)).unwrap();
        found.sort_by(|a, b| a.file.relative_path.cmp(&b.file.relative_path));
        assert_eq!(paths(&found), ["Album/Disc2/two.flac", "Album/one.mp3"]);
        assert_eq!((found[1].title.as_deref(), found[1].artist.as_deref(), found[1].album.as_deref()), (Some("T"), Some("A, B"), Some("Al")));
        assert_eq!((found[0].title.as_deref(), found[0].artist.as_deref(), found[0].album.as_deref()), (None, None, None));
    }

    #[test]
    fn a_file_that_cannot_be_read_is_still_included() {
        // MemoryFileAccess can't be made to fail reads per file, so use a tag reader that sees nothing: the file must stay.
        let fa = MemoryFileAccess::new([("one.mp3", "x")]);
        let nothing = |_: &[u8]| None;
        assert_eq!(paths(&block_on(find_playlist_candidate_tracks(&fa, "r", "", &nothing)).unwrap()), ["one.mp3"]);
    }
}
