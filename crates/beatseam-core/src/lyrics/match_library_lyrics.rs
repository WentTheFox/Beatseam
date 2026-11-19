use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use super::lyrics_files::{LyricsLoader, encode_lyrics_file_index, lyrics_file_index_setting_key};
use super::match_lyrics::{LyricsCandidate, find_auto_lyrics_match};
use crate::file_access::{FileAccess, FileAccessError};
use crate::library_scan::LibraryError;
use crate::library_store::{LibraryStore, LyricsScope, TrackRecord};
use crate::metadata::IdleScheduler;
use crate::tasks::{PassRegistrar, TaskContext, TaskOptions, TaskPriority, TaskProgress, TaskQueue};

/// Same reasoning as the metadata scan's - bounds how long a chunk can go without real idle time before it's given a slice
/// anyway.
const IDLE_CALLBACK_TIMEOUT_MS: u32 = 2000;

/// The store setting under which the last known matched count is kept, for an optimistic "N of M matched" display on the
/// next launch.
pub const LYRICS_MATCHED_COUNT_SETTING_KEY: &str = "lyricsMatchedCount";

#[derive(Default)]
pub struct MatchLibraryLyricsOptions<'a> {
    /// Called after each track's auto-match attempt with the running matched-so-far count, how many tracks this pass has
    /// processed, and the current total (which can grow, see `extend`) - so a caller can update a live "N of M matched"
    /// display.
    pub on_progress: Option<&'a mut dyn FnMut(usize, usize, usize)>,
    /// Called each time a processed track turns out to have no lyrics match - whether it never had one, or (rarer)
    /// previously did but doesn't anymore. Meant for a caller showing an optimistic "last known total" count immediately
    /// on launch rather than starting from zero/unknown: decrement that optimistic total once per call instead of waiting
    /// for the whole pass to finish before showing any number.
    pub on_anomaly: Option<&'a mut dyn FnMut()>,
    /// Called once, when the whole pass finishes, with the exact final matched/total counts (over every track actually
    /// processed - excludes `skip_file_ids`) - a caller should persist the matched count as the next launch's optimistic
    /// starting total and snap its live display to this exact value.
    pub on_settled: Option<&'a mut dyn FnMut(usize, usize)>,
    /// Bumps specific tracks (e.g. now-playing/up-next) ahead of scan order, re-evaluated fresh each step.
    pub get_priority_file_ids: Option<&'a dyn Fn() -> Vec<String>>,
    /// Tracks to skip entirely (already resolved elsewhere, e.g. the restoring track on the critical path) - avoids redoing
    /// that work here.
    pub skip_file_ids: Vec<String>,
    /// Awaited before each track so the pass can pause for foreground work.
    pub checkpoint: Option<&'a TaskContext>,
    /// Lets a later request with the same scopes add its tracks to this pass while it's running instead of queueing a
    /// second full pass.
    pub extend: Option<PassRegistrar<TrackRecord>>,
}

struct Pass {
    remaining: Vec<TrackRecord>,
    total: usize,
    seen: HashSet<String>,
    finished: bool,
}

/// Idle-chunked whole-library lyrics auto-match, meant to run in the background (see
/// [`LyricsLoader::ensure_lyrics_assignment`] for the single-track, critical-path version used during startup restore).
/// Never overwrites an existing assignment - re-checks the assignment immediately before each write, not just once at the
/// start of a track's turn, since an idle-chunked pass over a large library can run for several seconds or more, widening
/// the window in which a user could manually (re)assign a track mid-run.
///
/// A failure past the start (a lyrics root that vanished, a store write error) is returned. A lapsed browser grant, which
/// can't be re-requested from this background pass, just skips the run quietly; it picks back up once something
/// gesture-adjacent re-grants access.
pub async fn match_library_lyrics<F, S, I>(loader: &LyricsLoader<F, S>, idle: &I, scopes: &[LyricsScope], tracks: Vec<TrackRecord>, mut options: MatchLibraryLyricsOptions<'_>) -> Result<(), LibraryError>
where
    F: FileAccess,
    S: LibraryStore,
    I: IdleScheduler,
{
    let skip: HashSet<&str> = options.skip_file_ids.iter().map(String::as_str).collect();
    let remaining: Vec<TrackRecord> = tracks.into_iter().filter(|t| !skip.contains(t.file_id.as_str())).collect();
    let pass = Arc::new(Mutex::new(Pass { total: remaining.len(), seen: remaining.iter().map(|t| t.file_id.clone()).collect(), remaining, finished: false }));
    if let Some(registrar) = options.extend.take() {
        let shared = pass.clone();
        let skipped: HashSet<String> = options.skip_file_ids.iter().cloned().collect();
        registrar.register(move |more: &[TrackRecord]| {
            let mut pass = shared.lock().unwrap();
            if pass.finished {
                return false;
            }
            for track in more {
                if skipped.contains(&track.file_id) || !pass.seen.insert(track.file_id.clone()) {
                    continue;
                }
                pass.remaining.push(track.clone());
                pass.total += 1;
            }
            true
        });
    }
    if scopes.is_empty() || pass.lock().unwrap().total == 0 {
        pass.lock().unwrap().finished = true;
        return Ok(());
    }

    let files = match loader.scan_all_lyrics_scopes(scopes).await {
        Ok(files) => files,
        Err(LibraryError::FileAccess(FileAccessError::PermissionPending { .. })) => {
            pass.lock().unwrap().finished = true;
            return Ok(());
        }
        Err(e) => {
            pass.lock().unwrap().finished = true;
            return Err(e);
        }
    };
    // Persist this pass's own authoritative walk for the index readers (load, resolve, ensure, the picker) - without this
    // they each re-walked the same lyrics folder from scratch on every track switch/sync-save, right after this exact pass
    // had already walked it at launch or on an explicit Rescan.
    loader.store().put_setting(&lyrics_file_index_setting_key(scopes), &encode_lyrics_file_index(&files)).await?;
    let candidates: Vec<LyricsCandidate> = files.iter().map(|f| LyricsCandidate { file_id: f.file.id.clone(), name: f.file.name.clone() }).collect();

    let (mut matched, mut processed) = (0usize, 0usize);
    loop {
        let deadline = idle.wait_idle(IDLE_CALLBACK_TIMEOUT_MS).await;
        // At least one track always runs per chunk, or a deadline reporting no time would reschedule forever.
        loop {
            if let Some(checkpoint) = options.checkpoint {
                checkpoint.checkpoint().await;
            }
            let priority = options.get_priority_file_ids.map(|f| f()).unwrap_or_default();
            let (track, total) = {
                let mut pass = pass.lock().unwrap();
                let next = if priority.is_empty() { 0 } else { pass.remaining.iter().position(|t| priority.contains(&t.file_id)).unwrap_or(0) };
                (pass.remaining.remove(next), pass.total)
            };

            let mut is_matched = loader.store().get_lyrics_assignment(&track.file_id).await?.is_some();
            if !is_matched {
                let track_name = track.relative_path.rsplit('/').next().unwrap_or(&track.relative_path);
                if let Some(found) = find_auto_lyrics_match(track_name, &candidates) {
                    // Re-check right before writing, not just via the read above - a manual assignment could have landed
                    // in the meantime. Either way it ends up matched; only the write differs.
                    if loader.store().get_lyrics_assignment(&track.file_id).await?.is_none() {
                        loader.store().put_lyrics_assignment(&track.file_id, Some(found.file_id.clone())).await?;
                    }
                    is_matched = true;
                }
            }
            if is_matched {
                matched += 1;
            } else if let Some(on_anomaly) = options.on_anomaly.as_mut() {
                on_anomaly();
            }
            processed += 1;
            if let Some(on_progress) = options.on_progress.as_mut() {
                on_progress(matched, processed, total);
            }

            let (left, total) = {
                let pass = pass.lock().unwrap();
                (!pass.remaining.is_empty(), pass.total)
            };
            let _ = total;
            if !(left && (deadline.did_timeout || deadline.time_remaining_ms() > 0.0)) {
                break;
            }
        }
        let mut guard = pass.lock().unwrap();
        if guard.remaining.is_empty() {
            guard.finished = true;
            let total = guard.total;
            drop(guard);
            if let Some(on_settled) = options.on_settled.as_mut() {
                on_settled(matched, total);
            }
            return Ok(());
        }
    }
}

/// [`match_library_lyrics`] as a `Background` task on the shared queue - the same idea as the queued metadata scan: takes
/// its turn, pauses for higher-priority work between tracks, reports progress to the notification bell, and a request over
/// the same lyrics folders made while a pass runs joins it instead of queueing a second full pass.
pub async fn match_library_lyrics_queued<F, S, I>(
    queue: &TaskQueue,
    loader: Arc<LyricsLoader<F, S>>,
    idle: Arc<I>,
    scopes: Vec<LyricsScope>,
    tracks: Vec<TrackRecord>,
    skip_file_ids: Vec<String>,
    on_progress: Option<Arc<Mutex<dyn FnMut(usize, usize, usize) + Send>>>,
) -> Result<(), LibraryError>
where
    F: FileAccess + 'static,
    S: LibraryStore + 'static,
    I: IdleScheduler + 'static,
{
    // Only a request over the same lyrics folders can join a running pass - one with a folder added/removed has to
    // re-match everything. (Joined with no separator, exactly as the TypeScript did.)
    let mut keys: Vec<String> = scopes.iter().map(|s| format!("{} {}", s.root_id, s.relative_path)).collect();
    keys.sort_by(|a, b| crate::js_compat::js_string_cmp(a, b));
    let join_key = keys.concat();

    let failure: Arc<Mutex<Option<LibraryError>>> = Arc::new(Mutex::new(None));
    let record = failure.clone();
    queue
        .run_joinable_task(TaskOptions::new("lyrics-match", "Matching lyrics", TaskPriority::Background), join_key, tracks.clone(), move |context, registrar| async move {
            let reporter = context.clone();
            let mut forward = move |matched: usize, processed: usize, total: usize| {
                reporter.report_progress(TaskProgress { detail: String::new(), current: processed as u64, total: total as u64 });
                if let Some(user) = &on_progress {
                    (user.lock().unwrap())(matched, processed, total);
                }
            };
            let options = MatchLibraryLyricsOptions { on_progress: Some(&mut forward), skip_file_ids, checkpoint: Some(&context), extend: Some(registrar), ..Default::default() };
            if let Err(error) = match_library_lyrics(&loader, &*idle, &scopes, tracks, options).await {
                *record.lock().unwrap() = Some(error);
            }
        })
        .await;
    let result = failure.lock().unwrap().take();
    result.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::file_access::{CallOptions, DirectoryEntry, FileRef, GrantedRoot, MemoryFileAccess, RootKind};
    use crate::http::block_on;
    use crate::library_store::{MemoryLibraryStore, StoreError};
    use crate::metadata::ImmediateIdle;

    fn scope() -> Vec<LyricsScope> {
        vec![LyricsScope { root_id: "L".into(), relative_path: String::new() }]
    }

    fn track(id: &str, path: &str) -> TrackRecord {
        TrackRecord { file_id: id.into(), root_id: "r".into(), relative_path: path.into(), size_bytes: 1, last_modified_ms: 1.0, missing: false }
    }

    fn loader(files: &[(&str, &str)]) -> Arc<LyricsLoader<MemoryFileAccess, MemoryLibraryStore>> {
        Arc::new(LyricsLoader::new(Arc::new(MemoryFileAccess::new(files.iter().copied())), Arc::new(MemoryLibraryStore::new()), || 0))
    }

    fn run(l: &LyricsLoader<MemoryFileAccess, MemoryLibraryStore>, tracks: Vec<TrackRecord>, options: MatchLibraryLyricsOptions<'_>) -> Result<(), LibraryError> {
        block_on(match_library_lyrics(l, &ImmediateIdle, &scope(), tracks, options))
    }

    #[test]
    fn matches_tracks_to_same_stemmed_files_persists_the_index_and_reports_counts() {
        let l = loader(&[("A.lrc", "x"), ("B.lrc", "x")]);
        let tracks = vec![track("t1", "Music/A.mp3"), track("t2", "Music/B.flac"), track("t3", "Music/NoLyrics.mp3")];
        let (mut progress, mut anomalies, mut settled) = (Vec::new(), 0, None);
        let mut on_progress = |m: usize, p: usize, t: usize| progress.push((m, p, t));
        let mut on_anomaly = || anomalies += 1;
        let mut on_settled = |m: usize, t: usize| settled = Some((m, t));
        run(&l, tracks, MatchLibraryLyricsOptions { on_progress: Some(&mut on_progress), on_anomaly: Some(&mut on_anomaly), on_settled: Some(&mut on_settled), ..Default::default() }).unwrap();

        assert_eq!(block_on(l.store().get_lyrics_assignment("t1")).unwrap().as_deref(), Some("A.lrc"));
        assert_eq!(block_on(l.store().get_lyrics_assignment("t2")).unwrap().as_deref(), Some("B.lrc"));
        assert_eq!(block_on(l.store().get_lyrics_assignment("t3")).unwrap(), None);
        assert_eq!(progress, [(1, 1, 3), (2, 2, 3), (2, 3, 3)]);
        assert_eq!((anomalies, settled), (1, Some((2, 3))));
        assert!(block_on(l.store().get_setting("lyricsFileIndex:L:")).unwrap().is_some(), "the walk's result is persisted for everyone else");
    }

    #[test]
    fn never_overwrites_an_existing_assignment_and_counts_it_as_matched() {
        let l = loader(&[("A.lrc", "x"), ("Manual.lrc", "x")]);
        block_on(l.store().put_lyrics_assignment("t1", Some("Manual.lrc".into()))).unwrap();
        let mut settled = None;
        let mut on_settled = |m: usize, t: usize| settled = Some((m, t));
        run(&l, vec![track("t1", "A.mp3")], MatchLibraryLyricsOptions { on_settled: Some(&mut on_settled), ..Default::default() }).unwrap();
        assert_eq!(block_on(l.store().get_lyrics_assignment("t1")).unwrap().as_deref(), Some("Manual.lrc"));
        assert_eq!(settled, Some((1, 1)));
    }

    #[test]
    fn skips_tracks_that_were_resolved_elsewhere_and_bumps_priority_tracks_first() {
        let l = loader(&[("A.lrc", "x"), ("B.lrc", "x"), ("C.lrc", "x")]);
        let mut order = Vec::new();
        let mut on_progress = |_m: usize, p: usize, _t: usize| order.push(p);
        let priority = || vec!["t3".to_string()];
        run(
            &l,
            vec![track("t1", "A.mp3"), track("t2", "B.mp3"), track("t3", "C.mp3")],
            MatchLibraryLyricsOptions { on_progress: Some(&mut on_progress), get_priority_file_ids: Some(&priority), skip_file_ids: vec!["t2".into()], ..Default::default() },
        )
        .unwrap();
        assert_eq!(block_on(l.store().get_lyrics_assignment("t3")).unwrap().as_deref(), Some("C.lrc"));
        assert_eq!(block_on(l.store().get_lyrics_assignment("t2")).unwrap(), None, "skipped");
        assert_eq!(order, [1, 2], "two tracks processed, not three");
    }

    #[test]
    fn resolves_immediately_with_nothing_to_match_or_no_scopes() {
        let l = loader(&[("A.lrc", "x")]);
        assert_eq!(run(&l, vec![], Default::default()), Ok(()));
        assert_eq!(block_on(match_library_lyrics(&*l, &ImmediateIdle, &[], vec![track("t", "A.mp3")], Default::default())), Ok(()));
        assert_eq!(block_on(l.store().get_lyrics_assignment("t")).unwrap(), None);
    }

    /// A file access that fails listings, and a store that fails assignment writes.
    struct BrokenListing(FileAccessError);
    impl FileAccess for BrokenListing {
        async fn request_root(&self, _k: RootKind) -> Result<Option<GrantedRoot>, FileAccessError> {
            Ok(None)
        }
        async fn list_granted_roots(&self) -> Result<Vec<GrantedRoot>, FileAccessError> {
            Ok(vec![])
        }
        async fn revoke_root(&self, _r: &str) -> Result<(), FileAccessError> {
            Ok(())
        }
        async fn list_directory(&self, _r: &str, _p: Option<&str>, _o: CallOptions) -> Result<Vec<DirectoryEntry>, FileAccessError> {
            Err(self.0.clone())
        }
        async fn read_file_bytes(&self, _f: &FileRef, _o: CallOptions) -> Result<Vec<u8>, FileAccessError> {
            Ok(vec![])
        }
        async fn read_file_text(&self, _f: &FileRef, _o: CallOptions) -> Result<String, FileAccessError> {
            Ok(String::new())
        }
        async fn write_file_text(&self, _r: &str, _p: &str, _c: &str) -> Result<(), FileAccessError> {
            Ok(())
        }
    }

    #[test]
    fn fails_instead_of_hanging_when_a_lyrics_scope_cannot_be_listed() {
        let l = LyricsLoader::new(Arc::new(BrokenListing(FileAccessError::UnknownRoot("Lyrics".into()))), Arc::new(MemoryLibraryStore::new()), || 0);
        let result = block_on(match_library_lyrics(&l, &ImmediateIdle, &scope(), vec![track("t", "a.mp3")], Default::default()));
        assert_eq!(result, Err(LibraryError::FileAccess(FileAccessError::UnknownRoot("Lyrics".into()))));
        assert_eq!(result.unwrap_err().to_string(), "No library root \"Lyrics\"");
    }

    #[test]
    fn a_lapsed_browser_grant_skips_the_run_quietly() {
        let l = LyricsLoader::new(Arc::new(BrokenListing(FileAccessError::PermissionPending { root_display_name: "Lyrics".into() })), Arc::new(MemoryLibraryStore::new()), || 0);
        assert_eq!(block_on(match_library_lyrics(&l, &ImmediateIdle, &scope(), vec![track("t", "a.mp3")], Default::default())), Ok(()));
    }

    #[test]
    fn fails_when_persisting_a_match_fails() {
        struct FailingStore(MemoryLibraryStore);
        macro_rules! forward {
            ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {
                $(async fn $name(&self, $($arg: $ty),*) -> $ret { self.0.$name($($arg),*).await })*
            };
        }
        impl LibraryStore for FailingStore {
            forward! {
                upsert_track(t: TrackRecord) -> Result<(), StoreError>;
                delete_track(i: &str) -> Result<(), StoreError>;
                upsert_playlist(p: crate::library_store::PlaylistRecord) -> Result<(), StoreError>;
                delete_playlist(i: &str) -> Result<(), StoreError>;
                list_tracks(r: &str) -> Result<Vec<TrackRecord>, StoreError>;
                list_playlists(r: &str) -> Result<Vec<crate::library_store::PlaylistRecord>, StoreError>;
                list_all_tracks() -> Result<Vec<TrackRecord>, StoreError>;
                list_all_playlists() -> Result<Vec<crate::library_store::PlaylistRecord>, StoreError>;
                get_analysis(i: &str) -> Result<Option<crate::library_store::AnalysisResult>, StoreError>;
                put_analysis(a: crate::library_store::AnalysisResult) -> Result<(), StoreError>;
                get_metadata(i: &str) -> Result<Option<crate::metadata::TrackMetadata>, StoreError>;
                put_metadata(m: crate::metadata::TrackMetadata) -> Result<(), StoreError>;
                get_cover_art(i: &str) -> Result<Option<String>, StoreError>;
                put_cover_art(i: &str, a: Option<crate::metadata::CoverArtBytes>) -> Result<(), StoreError>;
                get_playback_state() -> Result<Option<crate::library_store::PlaybackState>, StoreError>;
                put_playback_state(s: crate::library_store::PlaybackState) -> Result<(), StoreError>;
                get_lyrics_scopes() -> Result<Vec<LyricsScope>, StoreError>;
                add_lyrics_scope(s: LyricsScope) -> Result<(), StoreError>;
                remove_lyrics_scope(r: &str, p: &str) -> Result<(), StoreError>;
                get_lyrics_assignment(i: &str) -> Result<Option<String>, StoreError>;
                get_setting(k: &str) -> Result<Option<String>, StoreError>;
                put_setting(k: &str, v: &str) -> Result<(), StoreError>;
            }
            async fn put_lyrics_assignment(&self, _i: &str, _l: Option<String>) -> Result<(), StoreError> {
                Err(StoreError("disk on fire".into()))
            }
        }
        let l = LyricsLoader::new(Arc::new(MemoryFileAccess::new([("a.lrc", "x")])), Arc::new(FailingStore(MemoryLibraryStore::new())), || 0);
        let result = block_on(match_library_lyrics(&l, &ImmediateIdle, &scope(), vec![track("t", "a.mp3")], Default::default()));
        assert_eq!(result, Err(LibraryError::Store(StoreError("disk on fire".into()))));
    }

    #[test]
    fn the_queued_pass_runs_as_a_background_task_and_reports_to_the_queue() {
        use crate::tasks::test_executor::Pool;
        let queue = TaskQueue::new(|| 0);
        let l = loader(&[("A.lrc", "x")]);
        let pool = Pool::new();
        let calls = Arc::new(AtomicU64::new(0));
        let c = calls.clone();
        let on_progress: Arc<Mutex<dyn FnMut(usize, usize, usize) + Send>> = Arc::new(Mutex::new(move |_: usize, _: usize, _: usize| {
            c.fetch_add(1, Ordering::SeqCst);
        }));
        let q = queue.clone();
        let l2 = l.clone();
        let result = pool.spawn(async move { match_library_lyrics_queued(&q, l2, Arc::new(ImmediateIdle), scope(), vec![track("t", "A.mp3")], vec![], Some(on_progress)).await });
        pool.run_until_stalled();
        assert_eq!(result.take(), Some(Ok(())));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(block_on(l.store().get_lyrics_assignment("t")).unwrap().as_deref(), Some("A.lrc"));
        assert!(queue.tasks().is_empty());
    }
}
