use std::collections::HashMap;

use super::error::LibraryError;
use super::walk::{WalkProgress, walk_directory};
use crate::file_access::{CallOptions, FileAccess};
use crate::library_store::{LibraryStore, PlaylistRecord, TrackRecord};
use crate::playlist::{parse_m3u8, resolve_m3u8_entry_path};
use crate::tasks::AbortSignal;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanPhase {
    Listing,
    Playlists,
    Saving,
}

/// A scan's live status, reported through `scan_root`'s `on_progress`. `Listing` has no total (the walk only discovers
/// the tree as it goes) - only the running folder/file counts; `Playlists` and `Saving` know their total up front, so
/// `current`/`total` are meaningful for those two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanProgress {
    pub phase: ScanPhase,
    pub folders_listed: usize,
    pub files_found: usize,
    pub current: usize,
    pub total: usize,
}

/// A playlist entry that didn't resolve to a file under the root - surfaced for diagnostics, not fatal (each also gets a
/// `missing` placeholder track record - see [`missing_track_id`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnresolvedEntry {
    pub playlist_relative_path: String,
    pub playlist_name: String,
    pub raw_path: String,
    pub resolved_path: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScanResult {
    pub playlists: Vec<PlaylistRecord>,
    pub tracks: Vec<TrackRecord>,
    pub unresolved_entries: Vec<UnresolvedEntry>,
}

/// Deterministic id for a missing-track placeholder, derived from where it was expected to be rather than a real
/// `FileRef::id` (there's no file to have one) - stable across rescans so the exact same missing entry upserts in place
/// instead of accumulating a fresh placeholder row every time, and so "locate this file" can be pointed at the same
/// resolved path it was synthesized from.
pub fn missing_track_id(root_id: &str, resolved_path: &str) -> String {
    format!("missing:{root_id}:{resolved_path}")
}

/// Turns a scan's unresolved entries into a notification-ready (title, detail) pair - `None` when there's nothing to
/// report. Shared by every app so the wording can't drift between them.
pub fn describe_unresolved_entries(unresolved: &[UnresolvedEntry], root_display_name: &str) -> Option<(String, String)> {
    if unresolved.is_empty() {
        return None;
    }
    let title = format!("{} missing file{} in \"{root_display_name}\"", unresolved.len(), if unresolved.len() == 1 { "" } else { "s" });
    let detail = unresolved.iter().map(|e| format!("{}: {}", e.playlist_name, e.raw_path)).collect::<Vec<_>>().join("\n");
    Some((title, detail))
}

/// A playlist file's display name: the file name without a trailing `.m3u8`/`.m3u` (the JS `/\.m3u8?$/i`).
fn playlist_name(file_name: &str) -> String {
    let lower = file_name.to_lowercase();
    // The lowercased copy can differ in byte length from the original for exotic characters; the extension is ASCII, so
    // measure it on the lowercased text and cut the same number of characters off the original.
    let strip = if lower.ends_with(".m3u8") { 5 } else if lower.ends_with(".m3u") { 4 } else { 0 };
    let keep = file_name.chars().count() - strip;
    file_name.chars().take(keep).collect()
}

fn check_cancelled(signal: Option<&AbortSignal>) -> Result<(), LibraryError> {
    if signal.is_some_and(AbortSignal::is_aborted) { Err(LibraryError::Cancelled) } else { Ok(()) }
}

/// Walks a granted root, parses every playlist found in it, and resolves each playlist's entries against the files
/// actually present. Tracks are defined as "files referenced by at least one playlist" - we don't treat every
/// audio-looking file in the tree as library content, since playlists are the thing the user curates and arbitrary
/// folder structure is explicitly not something we organize around.
///
/// Used both for the initial add-folder scan and for a rescan; upserts are idempotent so re-scanning an unchanged root is
/// a no-op at the store level.
///
/// `signal`, when given, lets a caller cancel a scan in progress - checked between the walk, each playlist file read,
/// and before committing to the store, failing with [`LibraryError::Cancelled`] as soon as it fires. Prefer the scan
/// coordinator over calling this directly from UI code - it also de-dupes concurrent scans of the same root, which this
/// function alone doesn't guard against.
///
/// `on_progress` is called after every directory listing, playlist read and store write - often, for a big library, so
/// throttle before rendering it.
pub async fn scan_root<F: FileAccess, S: LibraryStore>(
    file_access: &F,
    store: &S,
    root_id: &str,
    signal: Option<&AbortSignal>,
    mut on_progress: Option<&mut dyn FnMut(ScanProgress)>,
) -> Result<ScanResult, LibraryError> {
    // Read upfront, before this scan writes anything - see the stale-track cleanup pass at the end for what this is for.
    let existing_tracks = store.list_tracks(root_id).await?;

    let mut walked = WalkProgress { folders_listed: 0, files_found: 0 };
    let walk = {
        let mut on_walk = |p: WalkProgress| {
            walked = p;
            if let Some(report) = on_progress.as_mut() {
                report(ScanProgress { phase: ScanPhase::Listing, folders_listed: p.folders_listed, files_found: p.files_found, current: 0, total: 0 });
            }
        };
        walk_directory(file_access, root_id, None, signal, Some(&mut on_walk)).await?
    };
    let mut report = |phase: ScanPhase, current: usize, total: usize| {
        if let Some(report) = on_progress.as_mut() {
            report(ScanProgress { phase, folders_listed: walked.folders_listed, files_found: walked.files_found, current, total });
        }
    };

    let files_by_path: HashMap<&str, &crate::file_access::FileRef> = walk.files.iter().map(|f| (f.relative_path.as_str(), f)).collect();
    // Insertion-ordered by first sight, like a JavaScript `Map`.
    let mut tracks: Vec<TrackRecord> = Vec::new();
    let mut track_index: HashMap<String, usize> = HashMap::new();
    let mut put_track = |tracks: &mut Vec<TrackRecord>, track: TrackRecord| match track_index.get(&track.file_id) {
        Some(i) => tracks[*i] = track,
        None => {
            track_index.insert(track.file_id.clone(), tracks.len());
            tracks.push(track);
        }
    };
    let mut playlists: Vec<PlaylistRecord> = Vec::new();
    let mut unresolved_entries: Vec<UnresolvedEntry> = Vec::new();

    for (index, playlist_file) in walk.playlist_files.iter().enumerate() {
        check_cancelled(signal)?;
        report(ScanPhase::Playlists, index, walk.playlist_files.len());
        let text = file_access.read_file_text(playlist_file, CallOptions::default()).await?;
        let name = playlist_name(&playlist_file.name);
        let mut track_file_ids = Vec::new();

        for entry in parse_m3u8(&text) {
            let resolved_path = resolve_m3u8_entry_path(&playlist_file.relative_path, &entry.raw_path);
            match files_by_path.get(resolved_path.as_str()) {
                None => {
                    unresolved_entries.push(UnresolvedEntry {
                        playlist_relative_path: playlist_file.relative_path.clone(),
                        playlist_name: name.clone(),
                        raw_path: entry.raw_path.clone(),
                        resolved_path: resolved_path.clone(),
                    });
                    // A placeholder, not a dropped entry - see `TrackRecord::missing`. Size and modified time are 0
                    // (there's no real file to read them from); the relative path is where it was expected, so the
                    // display name shows its filename rather than nothing.
                    let id = missing_track_id(root_id, &resolved_path);
                    track_file_ids.push(id.clone());
                    put_track(&mut tracks, TrackRecord { file_id: id, root_id: root_id.to_owned(), relative_path: resolved_path, size_bytes: 0, last_modified_ms: 0.0, missing: true });
                }
                Some(track_file) => {
                    track_file_ids.push(track_file.id.clone());
                    put_track(
                        &mut tracks,
                        TrackRecord {
                            file_id: track_file.id.clone(),
                            root_id: root_id.to_owned(),
                            relative_path: track_file.relative_path.clone(),
                            size_bytes: track_file.size_bytes,
                            last_modified_ms: track_file.last_modified_ms,
                            missing: false,
                        },
                    );
                }
            }
        }

        playlists.push(PlaylistRecord { id: playlist_file.id.clone(), root_id: root_id.to_owned(), file_id: playlist_file.id.clone(), name, track_file_ids });
    }

    // Checked once more before committing anything - a cancel that lands right after the last playlist read finishes but
    // before upserts start shouldn't still write a scan's worth of tracks/playlists to the store.
    check_cancelled(signal)?;

    let save_total = tracks.len() + playlists.len();
    let mut saved = 0;
    for track in &tracks {
        report(ScanPhase::Saving, saved, save_total);
        saved += 1;
        store.upsert_track(track.clone()).await?;
    }
    for playlist in &playlists {
        report(ScanPhase::Saving, saved, save_total);
        saved += 1;
        store.upsert_playlist(playlist.clone()).await?;
    }

    // A previously-stored track for the same relative path whose id this scan didn't reuse is stale - e.g. a
    // `missing:`-prefixed placeholder now that the real file has resolved, or the reverse (a real track's row left behind
    // once its file went missing and got a placeholder instead). Either way the old id's row is now dead weight: nothing
    // references it (the fresh upsert above already wrote the current, correct row under the new id for this path), and
    // left alone it silently persists forever, inflating library-wide counts for as long as the root exists. Doesn't
    // touch a track whose path isn't resolved by this scan at all - that's an "Unplaylisted" track no playlist currently
    // references, a legitimate, still-valid row, not something this cleanup should remove.
    let resolved_id_by_path: HashMap<&str, &str> = tracks.iter().map(|t| (t.relative_path.as_str(), t.file_id.as_str())).collect();
    for existing in &existing_tracks {
        if let Some(resolved) = resolved_id_by_path.get(existing.relative_path.as_str())
            && *resolved != existing.file_id {
                store.delete_track(&existing.file_id).await?;
            }
    }

    Ok(ScanResult { playlists, tracks, unresolved_entries })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_access::{DirectoryEntry, FileAccessError, FileRef, GrantedRoot, MemoryFileAccess, RootKind};
    use crate::http::block_on;
    use crate::library_store::MemoryLibraryStore;
    use crate::tasks::AbortHandle;

    fn scan(fa: &MemoryFileAccess, store: &MemoryLibraryStore) -> Result<ScanResult, LibraryError> {
        block_on(scan_root(fa, store, "root-1", None, None))
    }

    fn ids(store: &MemoryLibraryStore) -> Vec<String> {
        let mut ids: Vec<String> = store.snapshot_tracks().into_iter().map(|t| t.file_id).collect();
        ids.sort();
        ids
    }

    #[test]
    fn resolves_nested_playlists_against_arbitrarily_structured_folders_and_upserts_into_the_store() {
        let fa = MemoryFileAccess::new([
            ("Playlists/Party Mix.m3u8", "#EXTM3U\n#EXTINF:200,Track A\n../Artist A/Track A.mp3\nTrack B.mp3"),
            ("Playlists/Track B.mp3", "fake-audio-b"),
            ("Artist A/Track A.mp3", "fake-audio-a"),
        ]);
        let store = MemoryLibraryStore::new();
        let result = scan(&fa, &store).unwrap();

        assert!(result.unresolved_entries.is_empty());
        assert_eq!(result.playlists.len(), 1);
        assert_eq!(result.playlists[0].name, "Party Mix");
        assert_eq!(result.playlists[0].track_file_ids, ["Artist A/Track A.mp3", "Playlists/Track B.mp3"]);
        assert_eq!(store.snapshot_tracks().len(), 2);
        assert_eq!(store.snapshot_playlists()[0].name, "Party Mix");
        assert_eq!(store.snapshot_playlists()[0].id, "Playlists/Party Mix.m3u8");
    }

    #[test]
    fn ignores_dot_prefixed_directories_entirely_like_syncthings_stversions() {
        let fa = MemoryFileAccess::new([("Mix.m3u8", "Track A.mp3"), ("Track A.mp3", "a"), (".stversions/Track A.mp3", "old"), (".stversions/Mix.m3u8", "Track A.mp3")]);
        let store = MemoryLibraryStore::new();
        let result = scan(&fa, &store).unwrap();
        assert!(result.unresolved_entries.is_empty());
        assert_eq!(result.playlists.len(), 1);
        assert_eq!(result.playlists[0].track_file_ids, ["Track A.mp3"]);
        assert_eq!(store.snapshot_tracks().len(), 1);
    }

    #[test]
    fn surfaces_unresolved_entries_as_a_missing_track_placeholder_instead_of_dropping_them() {
        let fa = MemoryFileAccess::new([("Mix.m3u8", "Missing.mp3")]);
        let store = MemoryLibraryStore::new();
        let result = scan(&fa, &store).unwrap();

        assert_eq!(
            result.unresolved_entries,
            [UnresolvedEntry { playlist_relative_path: "Mix.m3u8".into(), playlist_name: "Mix".into(), raw_path: "Missing.mp3".into(), resolved_path: "Missing.mp3".into() }]
        );
        // Still appears in the playlist (as a placeholder), not silently dropped.
        assert_eq!(result.playlists[0].track_file_ids, ["missing:root-1:Missing.mp3"]);
        assert_eq!(
            store.snapshot_tracks(),
            [TrackRecord { file_id: "missing:root-1:Missing.mp3".into(), root_id: "root-1".into(), relative_path: "Missing.mp3".into(), size_bytes: 0, last_modified_ms: 0.0, missing: true }]
        );
    }

    #[test]
    fn deletes_a_stale_placeholder_once_the_real_file_resolves_on_a_later_scan() {
        let fa = MemoryFileAccess::new([("Mix.m3u8", "Track.mp3")]);
        let store = MemoryLibraryStore::new();
        scan(&fa, &store).unwrap();
        assert_eq!(ids(&store), ["missing:root-1:Track.mp3"]);

        fa.add_file("Track.mp3", "fake-audio"); // e.g. a sync that was still in progress the first time around
        scan(&fa, &store).unwrap();
        assert_eq!(ids(&store), ["Track.mp3"]);
    }

    #[test]
    fn deletes_a_stale_real_track_row_once_its_file_goes_missing_on_a_later_scan() {
        let fa = MemoryFileAccess::new([("Mix.m3u8", "Track.mp3"), ("Track.mp3", "fake-audio")]);
        let store = MemoryLibraryStore::new();
        scan(&fa, &store).unwrap();
        assert_eq!(ids(&store), ["Track.mp3"]);

        fa.remove_file("Track.mp3");
        scan(&fa, &store).unwrap();
        assert_eq!(ids(&store), ["missing:root-1:Track.mp3"]);
    }

    #[test]
    fn leaves_an_unplaylisted_track_alone() {
        // `find_unplaylisted_tracks` upserts tracks scan_root itself never touches - the cleanup pass must not treat
        // "not resolved by this scan" the same as "superseded by a different id for the same path".
        let fa = MemoryFileAccess::new([("Loose.mp3", "fake-audio")]);
        let store = MemoryLibraryStore::new();
        block_on(store.upsert_track(TrackRecord { file_id: "Loose.mp3".into(), root_id: "root-1".into(), relative_path: "Loose.mp3".into(), size_bytes: 12, last_modified_ms: 0.0, missing: false })).unwrap();
        scan(&fa, &store).unwrap();
        assert_eq!(ids(&store), ["Loose.mp3"]);
    }

    #[test]
    fn rescanning_an_unchanged_root_is_idempotent() {
        let fa = MemoryFileAccess::new([("Mix.m3u8", "Track.mp3"), ("Track.mp3", "fake-audio")]);
        let store = MemoryLibraryStore::new();
        scan(&fa, &store).unwrap();
        scan(&fa, &store).unwrap();
        assert_eq!((store.snapshot_tracks().len(), store.snapshot_playlists().len()), (1, 1));
    }

    #[test]
    fn fails_with_cancelled_and_writes_nothing_if_the_signal_is_already_aborted() {
        let fa = MemoryFileAccess::new([("Mix.m3u8", "Track.mp3"), ("Track.mp3", "fake-audio")]);
        let store = MemoryLibraryStore::new();
        let handle = AbortHandle::new();
        handle.abort();
        let signal = handle.signal();
        assert_eq!(block_on(scan_root(&fa, &store, "root-1", Some(&signal), None)), Err(LibraryError::Cancelled));
        assert!(store.snapshot_tracks().is_empty() && store.snapshot_playlists().is_empty());
    }

    #[test]
    fn stops_walking_and_writes_nothing_once_the_signal_aborts_mid_scan() {
        // Aborts right after the very first listing resolves - there's no way to interrupt a listing already in flight,
        // only to stop starting new ones once the signal is checked again.
        struct AbortingAfterFirstCall {
            inner: MemoryFileAccess,
            handle: AbortHandle,
        }
        impl FileAccess for AbortingAfterFirstCall {
            async fn request_root(&self, k: RootKind) -> Result<Option<GrantedRoot>, FileAccessError> {
                self.inner.request_root(k).await
            }
            async fn list_granted_roots(&self) -> Result<Vec<GrantedRoot>, FileAccessError> {
                self.inner.list_granted_roots().await
            }
            async fn revoke_root(&self, r: &str) -> Result<(), FileAccessError> {
                self.inner.revoke_root(r).await
            }
            async fn list_directory(&self, r: &str, p: Option<&str>, o: CallOptions) -> Result<Vec<DirectoryEntry>, FileAccessError> {
                let entries = self.inner.list_directory(r, p, o).await;
                self.handle.abort();
                entries
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
        let handle = AbortHandle::new();
        let fa = AbortingAfterFirstCall {
            inner: MemoryFileAccess::new([("A/Mix.m3u8", "Track.mp3"), ("A/Track.mp3", "x"), ("B/Other.m3u8", "Other.mp3"), ("B/Other.mp3", "y")]),
            handle: handle.clone(),
        };
        let store = MemoryLibraryStore::new();
        let signal = handle.signal();
        assert_eq!(block_on(scan_root(&fa, &store, "root-1", Some(&signal), None)), Err(LibraryError::Cancelled));
        assert!(store.snapshot_tracks().is_empty() && store.snapshot_playlists().is_empty());
    }

    #[test]
    fn reports_progress_through_listing_playlists_and_saving() {
        let fa = MemoryFileAccess::new([("Mix.m3u8", "Track.mp3"), ("Track.mp3", "x")]);
        let store = MemoryLibraryStore::new();
        let mut seen = Vec::new();
        let mut record = |p: ScanProgress| seen.push(p);
        block_on(scan_root(&fa, &store, "root-1", None, Some(&mut record))).unwrap();
        let phases: Vec<ScanPhase> = seen.iter().map(|p| p.phase).collect();
        assert_eq!(phases, [ScanPhase::Listing, ScanPhase::Playlists, ScanPhase::Saving, ScanPhase::Saving]);
        assert_eq!(seen[1], ScanProgress { phase: ScanPhase::Playlists, folders_listed: 1, files_found: 2, current: 0, total: 1 });
        assert_eq!((seen[2].current, seen[2].total, seen[3].current), (0, 2, 1));
    }

    #[test]
    fn a_read_error_fails_the_scan_with_it() {
        struct Unreadable(MemoryFileAccess);
        impl FileAccess for Unreadable {
            async fn request_root(&self, k: RootKind) -> Result<Option<GrantedRoot>, FileAccessError> {
                self.0.request_root(k).await
            }
            async fn list_granted_roots(&self) -> Result<Vec<GrantedRoot>, FileAccessError> {
                self.0.list_granted_roots().await
            }
            async fn revoke_root(&self, r: &str) -> Result<(), FileAccessError> {
                self.0.revoke_root(r).await
            }
            async fn list_directory(&self, r: &str, p: Option<&str>, o: CallOptions) -> Result<Vec<DirectoryEntry>, FileAccessError> {
                self.0.list_directory(r, p, o).await
            }
            async fn read_file_bytes(&self, f: &FileRef, o: CallOptions) -> Result<Vec<u8>, FileAccessError> {
                self.0.read_file_bytes(f, o).await
            }
            async fn read_file_text(&self, _f: &FileRef, _o: CallOptions) -> Result<String, FileAccessError> {
                Err(FileAccessError::Io("disk on fire".into()))
            }
            async fn write_file_text(&self, r: &str, p: &str, c: &str) -> Result<(), FileAccessError> {
                self.0.write_file_text(r, p, c).await
            }
        }
        let fa = Unreadable(MemoryFileAccess::new([("Mix.m3u8", "Track.mp3")]));
        let result = block_on(scan_root(&fa, &MemoryLibraryStore::new(), "root-1", None, None));
        assert_eq!(result, Err(LibraryError::FileAccess(FileAccessError::Io("disk on fire".into()))));
    }

    #[test]
    fn describes_unresolved_entries_for_a_notification() {
        assert_eq!(describe_unresolved_entries(&[], "Music"), None);
        let one = UnresolvedEntry { playlist_relative_path: "M.m3u8".into(), playlist_name: "M".into(), raw_path: "a.mp3".into(), resolved_path: "a.mp3".into() };
        assert_eq!(describe_unresolved_entries(std::slice::from_ref(&one), "Music"), Some(("1 missing file in \"Music\"".into(), "M: a.mp3".into())));
        let two = UnresolvedEntry { playlist_name: "N".into(), raw_path: "b.mp3".into(), ..one.clone() };
        assert_eq!(describe_unresolved_entries(&[one, two], "Music"), Some(("2 missing files in \"Music\"".into(), "M: a.mp3\nN: b.mp3".into())));
    }

    #[test]
    fn playlist_names_drop_a_trailing_m3u_or_m3u8_extension_in_any_case() {
        for (file, name) in [("Party Mix.m3u8", "Party Mix"), ("Mix.M3U", "Mix"), ("Mix.m3u8.m3u", "Mix.m3u8"), ("noext", "noext"), ("a.m3u8x", "a.m3u8x"), (".m3u8", "")] {
            assert_eq!(playlist_name(file), name, "{file}");
        }
    }
}
