use super::error::LibraryError;
use super::walk::walk_directory;
use crate::file_access::{CallOptions, FileAccess};
use crate::playlist::{format_m3u8, parse_m3u8, relativize_m3u8_entry_path, resolve_m3u8_entry_path};

/// Rewrites every playlist under `root_id` that references `old_relative_path` (a missing track's expected-but-not-found
/// path - see `TrackRecord::missing`) to point at `new_relative_path` instead, each expressed relative to that
/// playlist's own location (playlists reference tracks relative to themselves, not the root). Returns how many playlists
/// were actually rewritten (0 means no playlist under this root referenced the old path - not necessarily an error, just
/// a no-op).
///
/// Requires `FileAccess::write_file_text`, which not every adapter supports - it fails there rather than silently doing
/// nothing, same as playlist-from-folder's own write. Doesn't touch the library store: the caller rescans afterwards.
pub async fn relocate_missing_track<F: FileAccess>(file_access: &F, root_id: &str, old_relative_path: &str, new_relative_path: &str) -> Result<usize, LibraryError> {
    let walked = walk_directory(file_access, root_id, None, None, None).await?;
    let mut rewritten_count = 0;

    for playlist_file in &walked.playlist_files {
        let text = file_access.read_file_text(playlist_file, CallOptions::default()).await?;
        let mut entries = parse_m3u8(&text);
        let mut changed = false;

        for entry in &mut entries {
            if resolve_m3u8_entry_path(&playlist_file.relative_path, &entry.raw_path) == old_relative_path {
                entry.raw_path = relativize_m3u8_entry_path(&playlist_file.relative_path, new_relative_path);
                changed = true;
            }
        }

        if changed {
            file_access.write_file_text(root_id, &playlist_file.relative_path, &format_m3u8(&entries)).await?;
            rewritten_count += 1;
        }
    }
    Ok(rewritten_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_access::MemoryFileAccess;
    use crate::http::block_on;

    #[test]
    fn rewrites_only_the_playlists_that_reference_the_old_path_relative_to_each_playlist() {
        let fa = MemoryFileAccess::new([
            ("Mixes/Party.m3u8", "#EXTM3U\n../Old/Track.mp3\nOther.mp3"),
            ("Chill.m3u8", "Old/Track.mp3"),
            ("Unrelated.m3u8", "Something.mp3"),
        ]);
        let count = block_on(relocate_missing_track(&fa, "r", "Old/Track.mp3", "New/Dir/Track.mp3")).unwrap();
        assert_eq!(count, 2);
        let files = fa.contents();
        assert_eq!(files["Mixes/Party.m3u8"], "#EXTM3U\n../New/Dir/Track.mp3\nOther.mp3\n");
        assert_eq!(files["Chill.m3u8"], "#EXTM3U\n./New/Dir/Track.mp3\n");
        assert_eq!(files["Unrelated.m3u8"], "Something.mp3", "untouched");
    }

    #[test]
    fn a_path_nothing_references_is_a_no_op() {
        let fa = MemoryFileAccess::new([("Mix.m3u8", "A.mp3")]);
        assert_eq!(block_on(relocate_missing_track(&fa, "r", "Nope.mp3", "New.mp3")).unwrap(), 0);
        assert_eq!(fa.contents()["Mix.m3u8"], "A.mp3");
    }

    #[test]
    fn keeps_extinf_info_of_rewritten_entries() {
        let fa = MemoryFileAccess::new([("Mix.m3u8", "#EXTM3U\n#EXTINF:215,Artist - Song\nOld.mp3")]);
        block_on(relocate_missing_track(&fa, "r", "Old.mp3", "Moved/Old.mp3")).unwrap();
        assert_eq!(fa.contents()["Mix.m3u8"], "#EXTM3U\n#EXTINF:215,Artist - Song\n./Moved/Old.mp3\n");
    }
}
