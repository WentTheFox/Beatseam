use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::audio_files::is_audio_file_name;
use super::error::LibraryError;
use super::walk::walk_directory;
use crate::file_access::FileAccess;
use crate::http::HttpTransport;
use crate::metadata::{CoverArtResizer, IdleScheduler, MetadataError, MetadataService, MetadataTask, TagReader, scan_library_metadata_queued};
use crate::library_store::{LibraryStore, StoreError, TrackRecord};
use crate::tasks::{TaskOptions, TaskPriority, TaskProgress, TaskQueue};

/// Every track under `root_id` that no current playlist references - the "songs not in a playlist" library-wide view.
/// Two different things land in this list, both handled here:
///
///  - a track a scan already knows about (a real track record, because some playlist referenced it once) whose owning
///    playlist was since edited or deleted, orphaning it - a cheap diff against already-loaded data, no walk needed;
///  - a file that was never referenced by any playlist at all, so a scan (deliberately playlist-first) never turned it
///    into a track record in the first place. This function does the one full audio-file walk needed to find those and
///    upserts each newly-found one into the store so it becomes a real, permanent library track from here on - metadata
///    and cover art then work on it like any other track. Only the *playlist membership* stays computed fresh on every
///    call rather than persisted, since that's the part that's actually "dynamic".
///
/// Deliberately not part of a scan's own pass (which stays fast/idle-safe for the startup restore path) - this does a
/// real directory walk, so it's meant to be called from an explicit, user-initiated action only, never on every launch.
pub async fn find_unplaylisted_tracks<F: FileAccess, S: LibraryStore>(
    file_access: &F,
    store: &S,
    root_id: &str,
    mut on_progress: Option<&mut dyn FnMut(TaskProgress)>,
) -> Result<Vec<TrackRecord>, LibraryError> {
    let playlists = store.list_playlists(root_id).await?;
    let existing_tracks = store.list_tracks(root_id).await?;
    let referenced: HashSet<&str> = playlists.iter().flat_map(|p| p.track_file_ids.iter().map(String::as_str)).collect();

    let walked = {
        let mut on_walk = |p: super::walk::WalkProgress| {
            if let Some(report) = on_progress.as_mut() {
                report(TaskProgress { detail: format!("{} files in {} folder{}", p.files_found, p.folders_listed, if p.folders_listed == 1 { "" } else { "s" }), current: 0, total: 0 });
            }
        };
        walk_directory(file_access, root_id, None, None, Some(&mut on_walk)).await?
    };
    let files = walked.files;
    let walked_paths: HashSet<&str> = files.iter().map(|f| f.relative_path.as_str()).collect();

    // A previously-known, unreferenced track whose relative path this walk didn't find at all is a ghost, not a
    // legitimate "unplaylisted" entry - its file was moved/renamed/deleted outside of any playlist entry tracking it
    // (e.g. a library organizer retagging and relocating files). A scan's own stale-row cleanup only fires for a path
    // still referenced by a playlist, so a ghost like this survives every rescan untouched and shows up here forever with
    // no metadata (confirmed live as songs appearing duplicated, with the dead copy never gaining a title/artist). A
    // `missing:` placeholder is a different, intentional case and is left alone.
    //
    // ...but a walk that found no files at all is not evidence that every file is gone - it's a root that couldn't be
    // listed (a revoked grant, an unmounted drive, a server-backed root with no folders). Deleting every stored track on
    // that basis is not recoverable, so it never happens.
    let ghost_ids: HashSet<String> = if files.is_empty() {
        HashSet::new()
    } else {
        existing_tracks.iter().filter(|t| !t.missing && !referenced.contains(t.file_id.as_str()) && !walked_paths.contains(t.relative_path.as_str())).map(|t| t.file_id.clone()).collect()
    };
    for id in &ghost_ids {
        store.delete_track(id).await?;
    }

    // Insertion order: what the store already had (minus the ghosts), then newly discovered files in walk order.
    let mut tracks: Vec<TrackRecord> = existing_tracks.into_iter().filter(|t| !ghost_ids.contains(&t.file_id)).collect();
    let mut known: HashSet<String> = tracks.iter().map(|t| t.file_id.clone()).collect();
    let mut newly_discovered = Vec::new();
    for file in &files {
        if !is_audio_file_name(&file.name) || known.contains(&file.id) {
            continue;
        }
        let track = TrackRecord { file_id: file.id.clone(), root_id: root_id.to_owned(), relative_path: file.relative_path.clone(), size_bytes: file.size_bytes, last_modified_ms: file.last_modified_ms, missing: false };
        known.insert(track.file_id.clone());
        newly_discovered.push(track);
    }
    for (saved, track) in newly_discovered.iter().enumerate() {
        store.upsert_track(track.clone()).await?;
        if let Some(report) = on_progress.as_mut() {
            report(TaskProgress { detail: "saving new tracks".into(), current: saved as u64 + 1, total: newly_discovered.len() as u64 });
        }
    }
    tracks.extend(newly_discovered);

    Ok(tracks.into_iter().filter(|t| !referenced.contains(t.file_id.as_str()) && !t.missing).collect())
}

/// "Songs not in a playlist" when the library comes from a server: every track is already in the store (the sync put it
/// there), so this is just the ones no playlist references. Never walks folders and never deletes anything - the server
/// decides what exists.
pub async fn find_unplaylisted_tracks_in_store<S: LibraryStore>(store: &S, root_id: &str) -> Result<Vec<TrackRecord>, StoreError> {
    let playlists = store.list_playlists(root_id).await?;
    let tracks = store.list_tracks(root_id).await?;
    let referenced: HashSet<&str> = playlists.iter().flat_map(|p| p.track_file_ids.iter().map(String::as_str)).collect();
    Ok(tracks.into_iter().filter(|t| !t.missing && !referenced.contains(t.file_id.as_str())).collect())
}

/// What [`find_unplaylisted_tracks_queued`] returns: the tracks, plus the metadata pass to run for them.
pub struct UnplaylistedTracks {
    pub tracks: Vec<TrackRecord>,
    /// Reads tags for the tracks found: tracks found here for the first time have never had theirs read, and the
    /// library-wide metadata pass only covers tracks that existed when it started. Not awaited here - the caller spawns it
    /// on its executor, or drops it to skip. As `Visible` work it takes turns with that pass instead of racing it for the
    /// same files, but goes ahead of it (it pauses at its next track), so what is on screen fills in first. Already-fresh
    /// tracks are skipped without a read.
    pub metadata_pass: Pin<Box<dyn Future<Output = Result<(), MetadataError>>>>,
}

/// [`find_unplaylisted_tracks`] as a `Foreground` task on the shared queue - waits for any folder scan in progress,
/// pauses background metadata/lyrics passes while it walks, and shows its progress in the notification bell. The metadata
/// service may read through a different file access than the walk (e.g. web's non-prompting background wrapper, since
/// nothing user-initiated is behind that pass).
pub async fn find_unplaylisted_tracks_queued<F, S, MF, MS, T, H, R, I>(
    queue: &TaskQueue,
    file_access: &F,
    store: &S,
    root_id: &str,
    root_display_name: &str,
    metadata: Arc<MetadataService<MF, MS, T, H, R>>,
    idle: Arc<I>,
) -> Result<UnplaylistedTracks, LibraryError>
where
    F: FileAccess,
    S: LibraryStore,
    MF: FileAccess + 'static,
    MS: LibraryStore + 'static,
    T: TagReader + 'static,
    H: HttpTransport + 'static,
    R: CoverArtResizer + 'static,
    I: IdleScheduler + 'static,
{
    let options = TaskOptions::new(format!("unplaylisted-{root_id}"), format!("Finding songs not in a playlist in \"{root_display_name}\""), TaskPriority::Foreground);
    let tracks = queue
        .run_task(options, |context| async move {
            let mut report = |p: TaskProgress| context.report_progress(p);
            find_unplaylisted_tracks(file_access, store, root_id, Some(&mut report)).await
        })
        .await?;
    let task = MetadataTask {
        id: format!("metadata-unplaylisted-{root_id}"),
        label: format!("Reading tags for songs not in a playlist in \"{root_display_name}\""),
        priority: TaskPriority::Visible,
        join_key: String::new(),
    };
    let pass_queue = queue.clone();
    let pass_tracks = tracks.clone();
    let metadata_pass = Box::pin(async move { scan_library_metadata_queued(&pass_queue, task, metadata, idle, pass_tracks, false, None, None).await });
    Ok(UnplaylistedTracks { tracks, metadata_pass })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_access::MemoryFileAccess;
    use crate::http::block_on;
    use crate::library_store::{MemoryLibraryStore, PlaylistRecord};

    fn rec(id: &str, size: u64, mtime: f64) -> TrackRecord {
        TrackRecord { file_id: id.into(), root_id: "root-1".into(), relative_path: id.into(), size_bytes: size, last_modified_ms: mtime, missing: false }
    }

    fn playlist(refs: &[&str]) -> PlaylistRecord {
        PlaylistRecord { id: "p1".into(), root_id: "root-1".into(), file_id: "Party Mix.m3u8".into(), name: "Party Mix".into(), track_file_ids: refs.iter().map(|s| s.to_string()).collect() }
    }

    fn run(fa: &MemoryFileAccess, store: &MemoryLibraryStore) -> Vec<String> {
        block_on(find_unplaylisted_tracks(fa, store, "root-1", None)).unwrap().into_iter().map(|t| t.file_id).collect()
    }

    fn stored(store: &MemoryLibraryStore) -> Vec<String> {
        let mut ids: Vec<String> = store.snapshot_tracks().into_iter().map(|t| t.file_id).collect();
        ids.sort();
        ids
    }

    #[test]
    fn finds_a_file_never_referenced_by_any_playlist_and_stores_it_as_a_real_track() {
        let fa = MemoryFileAccess::new([("Party Mix.m3u8", "#EXTM3U\nTrack A.mp3"), ("Track A.mp3", "a"), ("Loose Track.mp3", "loose")]);
        let store = MemoryLibraryStore::new();
        block_on(async {
            store.upsert_track(rec("Track A.mp3", 12, 0.0)).await.unwrap();
            store.upsert_playlist(playlist(&["Track A.mp3"])).await.unwrap();
        });
        assert_eq!(run(&fa, &store), ["Loose Track.mp3"]);
        assert_eq!(stored(&store), ["Loose Track.mp3", "Track A.mp3"], "the newly-found file is now a persisted track");
    }

    #[test]
    fn includes_a_track_orphaned_by_a_playlist_edit_with_no_walk_needed_to_find_it() {
        let fa = MemoryFileAccess::new([("Track A.mp3", "a")]);
        let store = MemoryLibraryStore::new();
        block_on(store.upsert_track(rec("Track A.mp3", 12, 0.0))).unwrap();
        assert_eq!(run(&fa, &store), ["Track A.mp3"]);
    }

    #[test]
    fn excludes_a_track_referenced_by_any_playlist() {
        let fa = MemoryFileAccess::new([("Party Mix.m3u8", "Track A.mp3"), ("Track A.mp3", "a")]);
        let store = MemoryLibraryStore::new();
        block_on(store.upsert_playlist(playlist(&["Track A.mp3"]))).unwrap();
        assert_eq!(run(&fa, &store), Vec::<String>::new());
    }

    #[test]
    fn excludes_a_missing_placeholder_track_even_if_unreferenced() {
        let store = MemoryLibraryStore::new();
        block_on(store.upsert_track(TrackRecord { file_id: "ghost".into(), root_id: "root-1".into(), relative_path: "Ghost Track.mp3".into(), size_bytes: 0, last_modified_ms: 0.0, missing: true })).unwrap();
        assert_eq!(run(&MemoryFileAccess::default(), &store), Vec::<String>::new());
    }

    #[test]
    fn does_not_rediscover_or_duplicate_upsert_a_file_that_is_already_a_known_track() {
        let fa = MemoryFileAccess::new([("Track A.mp3", "a")]);
        let store = MemoryLibraryStore::new();
        block_on(store.upsert_track(rec("Track A.mp3", 999, 42.0))).unwrap();
        run(&fa, &store);
        // Untouched - not re-derived from the walked file (which would report the fake content's size).
        assert_eq!(store.snapshot_tracks(), [rec("Track A.mp3", 999, 42.0)]);
    }

    #[test]
    fn drops_and_deletes_a_ghost_row_whose_file_was_moved_away_instead_of_surfacing_a_dead_duplicate() {
        let fa = MemoryFileAccess::new([("Track A (renamed).mp3", "fake-audio-a")]);
        let store = MemoryLibraryStore::new();
        block_on(store.upsert_track(rec("Track A.mp3", 12, 0.0))).unwrap();
        assert_eq!(run(&fa, &store), ["Track A (renamed).mp3"]);
        assert_eq!(store.snapshot_tracks(), [rec("Track A (renamed).mp3", 12, 0.0)]);
    }

    #[test]
    fn never_deletes_stored_tracks_when_the_walk_finds_no_files_at_all() {
        let store = MemoryLibraryStore::new();
        block_on(async {
            store.upsert_track(rec("a.mp3", 1, 0.0)).await.unwrap();
            store.upsert_track(rec("b.mp3", 1, 0.0)).await.unwrap();
        });
        let mut ids = run(&MemoryFileAccess::default(), &store);
        ids.sort();
        assert_eq!(ids, ["a.mp3", "b.mp3"]);
        assert_eq!(stored(&store), ["a.mp3", "b.mp3"]);
    }

    #[test]
    fn leaves_a_ghost_row_alone_if_some_playlist_still_references_it() {
        let store = MemoryLibraryStore::new();
        block_on(async {
            store.upsert_track(rec("Track A.mp3", 12, 0.0)).await.unwrap();
            store.upsert_playlist(playlist(&["Track A.mp3"])).await.unwrap();
        });
        // A walk that does find something (so the empty-walk guard does not apply), but not this track's file.
        assert_eq!(run(&MemoryFileAccess::new([("other.txt", "x")]), &store), Vec::<String>::new());
        assert_eq!(store.snapshot_tracks(), [rec("Track A.mp3", 12, 0.0)]);
    }

    #[test]
    fn ignores_non_audio_files_entirely() {
        let fa = MemoryFileAccess::new([("readme.txt", "not audio"), ("cover.jpg", "not audio either")]);
        let store = MemoryLibraryStore::new();
        assert_eq!(run(&fa, &store), Vec::<String>::new());
        assert!(store.snapshot_tracks().is_empty());
    }

    #[test]
    fn reports_progress_while_walking_and_saving() {
        let fa = MemoryFileAccess::new([("a.mp3", "x"), ("b.mp3", "y")]);
        let store = MemoryLibraryStore::new();
        let mut seen = Vec::new();
        let mut record = |p: TaskProgress| seen.push(p);
        block_on(find_unplaylisted_tracks(&fa, &store, "root-1", Some(&mut record))).unwrap();
        assert_eq!(seen[0].detail, "2 files in 1 folder");
        assert_eq!((seen[1].detail.as_str(), seen[1].current, seen[1].total), ("saving new tracks", 1, 2));
        assert_eq!(seen.last().unwrap().current, 2);
    }

    #[test]
    fn the_store_only_variant_answers_from_the_store_and_changes_nothing() {
        let store = MemoryLibraryStore::new();
        let track = |id: &str, root: &str, missing: bool| TrackRecord { file_id: id.into(), root_id: root.into(), relative_path: id.into(), size_bytes: 1, last_modified_ms: 0.0, missing };
        block_on(async {
            for t in [track("in-list.mp3", "r", false), track("loose-1.mp3", "r", false), track("loose-2.mp3", "r", false), track("gone.mp3", "r", true), track("other-root.mp3", "elsewhere", false)] {
                store.upsert_track(t).await.unwrap();
            }
            store.upsert_playlist(PlaylistRecord { id: "p".into(), root_id: "r".into(), file_id: "p".into(), name: "p".into(), track_file_ids: vec!["in-list.mp3".into(), "gone.mp3".into()] }).await.unwrap();
        });
        let mut ids: Vec<String> = block_on(find_unplaylisted_tracks_in_store(&store, "r")).unwrap().into_iter().map(|t| t.file_id).collect();
        ids.sort();
        assert_eq!(ids, ["loose-1.mp3", "loose-2.mp3"]);
        assert_eq!(store.snapshot_tracks().len(), 5);
    }
}
