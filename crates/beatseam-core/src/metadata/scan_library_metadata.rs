use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use super::ensure_metadata::{CoverArtResizer, MetadataError, MetadataService, is_metadata_fresh};
use super::read_tags::TagReader;
use crate::file_access::{FileAccess, FileRef};
use crate::http::HttpTransport;
use crate::library_store::{LibraryStore, TrackRecord};
use crate::tasks::{PassRegistrar, TaskContext, TaskOptions, TaskPriority, TaskProgress, TaskQueue, yield_now};

/// Upper bound on how long a single scan run is allowed to go without the thread actually being idle before it's given a
/// slice anyway - the idle scheduler's own timeout. Without this, a screen kept continuously busy (animations, frequent
/// re-renders) could starve the scan indefinitely; this caps that to a bounded worst-case delay per chunk instead.
const IDLE_CALLBACK_TIMEOUT_MS: u32 = 2000;

/// The slice of idle time a scan chunk was granted (JavaScript's `IdleDeadline`).
pub struct IdleDeadline {
    pub did_timeout: bool,
    time_remaining: Box<dyn Fn() -> f64 + Send + Sync>,
}

impl IdleDeadline {
    pub fn new(did_timeout: bool, time_remaining: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        Self { did_timeout, time_remaining: Box::new(time_remaining) }
    }

    /// Milliseconds left in this slice.
    pub fn time_remaining_ms(&self) -> f64 {
        (self.time_remaining)()
    }
}

/// Schedules the scan's chunks for idle periods. The UI supplies the real thing (the frame loop, `requestIdleCallback` on
/// web); [`ImmediateIdle`] and [`YieldIdle`] cover tests and headless callers. The deadline-driven chunking pattern is the
/// same either way: as many tracks as fit in one idle deadline are processed back-to-back before yielding, so a long idle
/// window drains the scan faster, while a busy one still only takes a track at a time between real gaps.
pub trait IdleScheduler {
    fn wait_idle(&self, timeout_ms: u32) -> impl Future<Output = IdleDeadline>;
}

/// No idle scheduling at all: one endless slice, so the whole scan runs in a single chunk. For servers and tests.
pub struct ImmediateIdle;

impl IdleScheduler for ImmediateIdle {
    async fn wait_idle(&self, _timeout_ms: u32) -> IdleDeadline {
        IdleDeadline::new(false, || f64::INFINITY)
    }
}

/// The fallback used where there is no real idle detection: yields to the executor once, then grants a fixed 50 ms
/// budget measured by the injected clock. Not genuine idle scheduling, but it keeps the chunking pattern working.
pub struct YieldIdle {
    now_ms: Arc<dyn Fn() -> f64 + Send + Sync>,
}

/// Synthetic per-callback budget used by [`YieldIdle`].
const FALLBACK_BUDGET_MS: f64 = 50.0;

impl YieldIdle {
    pub fn new(now_ms: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        Self { now_ms: Arc::new(now_ms) }
    }
}

impl IdleScheduler for YieldIdle {
    async fn wait_idle(&self, _timeout_ms: u32) -> IdleDeadline {
        yield_now().await;
        let clock = self.now_ms.clone();
        let started = clock();
        IdleDeadline::new(false, move || (FALLBACK_BUDGET_MS - (clock() - started)).max(0.0))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScanMetadataProgress {
    pub track: TrackRecord,
    pub index: usize,
    pub total: usize,
    pub skipped: bool,
    /// Set when reading this track's metadata failed; the scan carries on with the rest.
    pub error: Option<MetadataError>,
}

#[derive(Default)]
pub struct ScanLibraryMetadataOptions<'a> {
    /// Called after each track's metadata read attempt (skipped, succeeded, or failed) - lets callers refresh the library
    /// screen as titles come in.
    pub on_progress: Option<&'a mut dyn FnMut(ScanMetadataProgress)>,
    /// Called before each track is scanned - return file ids (e.g. the currently-playing and up-next tracks) that should
    /// jump the queue if they haven't been scanned yet this pass. Evaluated fresh before every step (not just once up
    /// front), so a track that becomes "current" partway through an already-running scan still gets bumped up instead of
    /// waiting for the scan to reach it in its original list position - important on a large library, where a
    /// stale-parser-version rescan can take a while to reach whatever the user actually has on screen.
    pub get_priority_file_ids: Option<&'a dyn Fn() -> Vec<String>>,
    /// Skips the cheap size/mtime freshness check for every track in this pass and re-reads/re-hashes them
    /// unconditionally - the only way to catch a file whose content changed but whose size and mtime happen not to. Not
    /// set by the normal passive background scan; for an explicit, user-initiated "verify this" pass (the Rescan button).
    pub force_refresh: bool,
    /// Awaited before each track - a background task's context, so the pass pauses while a folder scan/Unplaylisted walk
    /// runs instead of competing with it.
    pub checkpoint: Option<&'a TaskContext>,
    /// Lets a later request join this pass while it's running instead of queueing a second full one (see
    /// [`scan_library_metadata_queued`]).
    pub extend: Option<PassRegistrar<TrackRecord>>,
}

pub fn track_to_file_ref(track: &TrackRecord) -> FileRef {
    FileRef {
        id: track.file_id.clone(),
        name: track.relative_path.rsplit('/').next().unwrap_or(&track.relative_path).to_owned(),
        relative_path: track.relative_path.clone(),
        size_bytes: track.size_bytes,
        last_modified_ms: track.last_modified_ms,
    }
}

struct Pass {
    remaining: Vec<TrackRecord>,
    total: usize,
    seen: HashSet<String>,
    finished: bool,
}

/// Reads tags for every track in the given list that's new or whose stored metadata is stale, persisting results as it
/// goes. Unlike library analysis (BPM/loudness, deliberately not auto-run because decoding a whole library starves the
/// UI thread), reading tag bytes is cheap - this is meant to be fired off in the background right after a scan, so the
/// library screen fills in real titles/artists in place of filenames as it completes. A bad/unreadable file is reported
/// through `on_progress` rather than aborting the run; only a failure of the store itself stops it.
///
/// Work is chunked against idle time ([`IdleScheduler`]): as many tracks as fit in one idle deadline are processed
/// back-to-back before yielding.
pub async fn scan_library_metadata<F, S, T, H, R, I>(service: &MetadataService<F, S, T, H, R>, idle: &I, tracks: Vec<TrackRecord>, mut options: ScanLibraryMetadataOptions<'_>) -> Result<(), MetadataError>
where
    F: FileAccess,
    S: LibraryStore,
    T: TagReader,
    H: HttpTransport,
    R: CoverArtResizer,
    I: IdleScheduler,
{
    let pass = Arc::new(Mutex::new(Pass { total: tracks.len(), seen: tracks.iter().map(|t| t.file_id.clone()).collect(), remaining: tracks, finished: false }));
    if let Some(registrar) = options.extend.take() {
        let shared = pass.clone();
        registrar.register(move |more: &[TrackRecord]| {
            let mut pass = shared.lock().unwrap();
            if pass.finished {
                return false;
            }
            for track in more {
                if pass.seen.insert(track.file_id.clone()) {
                    pass.remaining.push(track.clone());
                    pass.total += 1;
                }
            }
            true
        });
    }
    if pass.lock().unwrap().total == 0 {
        pass.lock().unwrap().finished = true;
        return Ok(());
    }

    let mut index = 0usize;
    loop {
        let deadline = idle.wait_idle(IDLE_CALLBACK_TIMEOUT_MS).await;
        // At least one track always runs per chunk (even with zero time left) - otherwise a deadline that reports 0
        // remaining right away would reschedule forever without ever making progress.
        loop {
            if let Some(checkpoint) = options.checkpoint {
                checkpoint.checkpoint().await;
            }
            process_one(service, &pass, &mut index, &mut options).await?;
            let total = pass.lock().unwrap().total;
            if !(index < total && (deadline.did_timeout || deadline.time_remaining_ms() > 0.0)) {
                break;
            }
        }
        let mut guard = pass.lock().unwrap();
        if index >= guard.total {
            guard.finished = true;
            return Ok(());
        }
    }
}

async fn process_one<F, S, T, H, R>(service: &MetadataService<F, S, T, H, R>, pass: &Arc<Mutex<Pass>>, index: &mut usize, options: &mut ScanLibraryMetadataOptions<'_>) -> Result<(), MetadataError>
where
    F: FileAccess,
    S: LibraryStore,
    T: TagReader,
    H: HttpTransport,
    R: CoverArtResizer,
{
    // Default to the next track in original order - only reach for a priority one if one's actually still pending, so
    // this stays a no-op (and no extra scan cost) for callers that don't pass the option.
    let priority_ids = options.get_priority_file_ids.map(|f| f()).unwrap_or_default();
    let (track, total) = {
        let mut pass = pass.lock().unwrap();
        let next = if priority_ids.is_empty() { 0 } else { pass.remaining.iter().position(|t| priority_ids.contains(&t.file_id)).unwrap_or(0) };
        (pass.remaining.remove(next), pass.total)
    };
    let current_index = *index;
    *index += 1;

    let mut report = |skipped: bool, error: Option<MetadataError>| {
        if let Some(on_progress) = options.on_progress.as_mut() {
            on_progress(ScanMetadataProgress { track: track.clone(), index: current_index, total, skipped, error });
        }
    };

    if !options.force_refresh {
        let existing = service.store().get_metadata(&track.file_id).await?;
        if is_metadata_fresh(existing.as_ref(), track.size_bytes, track.last_modified_ms) {
            report(true, None);
            return Ok(());
        }
    }
    match service.ensure_track_metadata(&track_to_file_ref(&track), options.force_refresh).await {
        Ok(_) => report(false, None),
        Err(error) => report(false, Some(error)),
    }
    Ok(())
}

/// What a queued scan is called, how it ranks, and what it may join.
pub struct MetadataTask {
    pub id: String,
    pub label: String,
    /// `Visible` for a small pass over tracks that are on screen right now, `Background` for a whole-library one.
    pub priority: TaskPriority,
    /// A request made while a pass with the same `id` is running joins it - unless the join key differs, in which case it
    /// can't join and instead supersedes it (the queue's latest-wins rule, which also makes an already-running superseded
    /// pass yield at its next checkpoint instead of blocking the new one). Opening a different playlist, for example,
    /// preempts rather than queues behind the previous one's still-running scan.
    pub join_key: String,
}

impl MetadataTask {
    pub fn background(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self { id: id.into(), label: label.into(), priority: TaskPriority::Background, join_key: String::new() }
    }
}

/// [`scan_library_metadata`] as a task on the shared queue: waits for other queued work, pauses between tracks while
/// higher-priority work (a folder scan, the Unplaylisted walk) is waiting, and reports its progress there (the
/// notification bell shows it). A request made while a pass with the same task id is running joins it (adding its tracks)
/// rather than queueing a second full pass.
///
/// A joining caller returns `Ok` when the pass it joined does. A store failure is reported to the caller that ran the pass.
pub async fn scan_library_metadata_queued<F, S, T, H, R, I>(
    queue: &TaskQueue,
    task: MetadataTask,
    service: Arc<MetadataService<F, S, T, H, R>>,
    idle: Arc<I>,
    tracks: Vec<TrackRecord>,
    force_refresh: bool,
    get_priority_file_ids: Option<Arc<dyn Fn() -> Vec<String> + Send + Sync>>,
    on_progress: Option<Arc<Mutex<dyn FnMut(ScanMetadataProgress) + Send>>>,
) -> Result<(), MetadataError>
where
    F: FileAccess + 'static,
    S: LibraryStore + 'static,
    T: TagReader + 'static,
    H: HttpTransport + 'static,
    R: CoverArtResizer + 'static,
    I: IdleScheduler + 'static,
{
    let failure: Arc<Mutex<Option<MetadataError>>> = Arc::new(Mutex::new(None));
    let record_failure = failure.clone();
    let options = TaskOptions::new(task.id, task.label, task.priority);
    queue
        .run_joinable_task(options, task.join_key, tracks.clone(), move |context, registrar| async move {
            let reporter = context.clone();
            let mut forward = move |p: ScanMetadataProgress| {
                reporter.report_progress(TaskProgress { detail: String::new(), current: p.index as u64 + 1, total: p.total as u64 });
                if let Some(user) = &on_progress {
                    (user.lock().unwrap())(p);
                }
            };
            let priority = get_priority_file_ids.as_ref().map(|f| &**f as &(dyn Fn() -> Vec<String> + Send + Sync));
            let options = ScanLibraryMetadataOptions { on_progress: Some(&mut forward), get_priority_file_ids: priority.map(|f| f as &dyn Fn() -> Vec<String>), force_refresh, checkpoint: Some(&context), extend: Some(registrar) };
            if let Err(error) = scan_library_metadata(&service, &*idle, tracks, options).await {
                *record_failure.lock().unwrap() = Some(error);
            }
        })
        .await;
    let result = failure.lock().unwrap().take();
    result.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::file_access::MemoryFileAccess;
    use crate::http::block_on;
    use crate::library_store::MemoryLibraryStore;
    use crate::metadata::{METADATA_PARSER_VERSION, ParsedTags, TrackMetadata};
    use crate::tasks::test_executor::Pool;

    type Tags = fn(&[u8]) -> Option<ParsedTags>;
    type Svc = MetadataService<MemoryFileAccess, MemoryLibraryStore, Tags>;

    fn tags(bytes: &[u8]) -> Option<ParsedTags> {
        let text = std::str::from_utf8(bytes).ok()?;
        text.strip_prefix("title=").map(|t| ParsedTags { title: Some(t.into()), ..Default::default() })
    }

    fn track(id: &str) -> TrackRecord {
        TrackRecord { file_id: id.into(), root_id: "root".into(), relative_path: format!("{id}.mp3"), size_bytes: 64, last_modified_ms: 1.0, missing: false }
    }

    fn service(files: &[(&str, &str)]) -> Arc<Svc> {
        Arc::new(MetadataService::new(Arc::new(MemoryFileAccess::new(files.iter().copied())), Arc::new(MemoryLibraryStore::new()), Arc::new(tags as Tags)))
    }

    fn files_for(ids: &[&str]) -> Vec<(String, String)> {
        ids.iter().map(|i| (format!("{i}.mp3"), format!("title=Title {i}"))).collect()
    }

    fn svc_for(ids: &[&str]) -> Arc<Svc> {
        let files = files_for(ids);
        service(&files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect::<Vec<_>>())
    }

    fn scan(service: &Svc, tracks: Vec<TrackRecord>, force_refresh: bool, priority: Option<&dyn Fn() -> Vec<String>>) -> Vec<ScanMetadataProgress> {
        let mut seen = Vec::new();
        let mut sink = |p: ScanMetadataProgress| seen.push(p);
        let options = ScanLibraryMetadataOptions { on_progress: Some(&mut sink), get_priority_file_ids: priority, force_refresh, ..Default::default() };
        block_on(scan_library_metadata(service, &ImmediateIdle, tracks, options)).unwrap();
        seen
    }

    #[test]
    fn scans_every_track_exactly_once_and_resolves() {
        let s = svc_for(&["a", "b", "c"]);
        let seen = scan(&s, vec![track("a"), track("b"), track("c")], false, None);
        let mut ids: Vec<_> = seen.iter().map(|p| p.track.file_id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, ["a", "b", "c"]);
        assert!(seen.iter().all(|p| !p.skipped && p.total == 3 && p.error.is_none()));
        for id in ["a", "b", "c"] {
            assert!(block_on(s.store().get_metadata(id)).unwrap().is_some());
        }
    }

    #[test]
    fn resolves_immediately_for_an_empty_track_list() {
        assert_eq!(scan(&svc_for(&[]), vec![], false, None), vec![]);
    }

    #[test]
    fn skips_a_track_whose_stored_metadata_is_already_fresh_without_reading_the_file() {
        let s = service(&[]); // reading would fail, which proves the skip really skipped
        block_on(s.store().put_metadata(TrackMetadata { file_id: "a".into(), title: Some("Cached".into()), artists: vec![], album: None, duration_seconds: None, size_bytes: 64, last_modified_ms: 1.0, parser_version: METADATA_PARSER_VERSION, content_hash: None })).unwrap();
        let seen = scan(&s, vec![track("a")], false, None);
        assert_eq!(seen.len(), 1);
        assert!(seen[0].skipped && seen[0].error.is_none());
    }

    #[test]
    fn force_refresh_re_reads_a_track_even_though_its_stored_metadata_still_looks_fresh() {
        let s = svc_for(&["a"]);
        block_on(s.store().put_metadata(TrackMetadata { file_id: "a".into(), title: Some("Cached".into()), artists: vec![], album: None, duration_seconds: None, size_bytes: 64, last_modified_ms: 1.0, parser_version: METADATA_PARSER_VERSION, content_hash: None })).unwrap();
        let seen = scan(&s, vec![track("a")], true, None);
        assert!(!seen[0].skipped);
        assert_eq!(block_on(s.store().get_metadata("a")).unwrap().unwrap().title.as_deref(), Some("Title a"));
    }

    #[test]
    fn bumps_a_priority_file_id_ahead_of_the_rest_re_evaluated_on_every_step() {
        let s = svc_for(&["a", "b", "c"]);
        let priority = || vec!["c".to_string()];
        let seen = scan(&s, vec![track("a"), track("b"), track("c")], false, Some(&priority));
        let order: Vec<_> = seen.iter().map(|p| p.track.file_id.as_str()).collect();
        assert_eq!(order[0], "c");
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(sorted, ["a", "b", "c"]);
    }

    #[test]
    fn reports_an_error_through_progress_instead_of_aborting_the_rest_of_the_scan() {
        let s = service(&[("b.mp3", "title=B")]); // 'a' has no file, so its read fails
        let seen = scan(&s, vec![track("a"), track("b")], false, None);
        assert_eq!(seen.len(), 2);
        assert!(seen.iter().find(|p| p.track.file_id == "a").unwrap().error.is_some());
        assert!(seen.iter().find(|p| p.track.file_id == "b").unwrap().error.is_none());
        assert!(block_on(s.store().get_metadata("b")).unwrap().is_some());
    }

    /// Counts how often chunks were requested, granting a generous 1000 ms slice each time.
    struct CountingIdle(AtomicUsize);

    impl IdleScheduler for CountingIdle {
        async fn wait_idle(&self, _timeout_ms: u32) -> IdleDeadline {
            self.0.fetch_add(1, Ordering::SeqCst);
            IdleDeadline::new(false, || 1000.0)
        }
    }

    /// Grants a slice with no time left: one track per chunk.
    struct BusyIdle(AtomicUsize);

    impl IdleScheduler for BusyIdle {
        async fn wait_idle(&self, _timeout_ms: u32) -> IdleDeadline {
            self.0.fetch_add(1, Ordering::SeqCst);
            IdleDeadline::new(false, || 0.0)
        }
    }

    #[test]
    fn drains_every_track_in_one_chunk_when_the_idle_window_is_long() {
        let s = svc_for(&["a", "b", "c"]);
        let idle = CountingIdle(AtomicUsize::new(0));
        block_on(scan_library_metadata(&s, &idle, vec![track("a"), track("b"), track("c")], Default::default())).unwrap();
        assert_eq!(idle.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn takes_one_track_per_chunk_when_there_is_never_any_idle_time_but_still_finishes() {
        let s = svc_for(&["a", "b", "c"]);
        let idle = BusyIdle(AtomicUsize::new(0));
        block_on(scan_library_metadata(&s, &idle, vec![track("a"), track("b"), track("c")], Default::default())).unwrap();
        assert_eq!(idle.0.load(Ordering::SeqCst), 3, "one chunk per track, never zero progress");
        assert!(block_on(s.store().get_metadata("c")).unwrap().is_some());
    }

    #[test]
    fn a_timed_out_deadline_keeps_going_like_a_deadline_with_time_left() {
        struct TimedOut;
        impl IdleScheduler for TimedOut {
            async fn wait_idle(&self, _t: u32) -> IdleDeadline {
                IdleDeadline::new(true, || 0.0)
            }
        }
        let s = svc_for(&["a", "b"]);
        block_on(scan_library_metadata(&s, &TimedOut, vec![track("a"), track("b")], Default::default())).unwrap();
        assert!(block_on(s.store().get_metadata("b")).unwrap().is_some());
    }

    // ---- the queued variant ----

    fn queue() -> TaskQueue {
        TaskQueue::new(|| 0)
    }

    #[test]
    fn the_queued_scan_reports_progress_to_the_task_and_to_the_caller() {
        let (q, s, pool) = (queue(), svc_for(&["a", "b"]), Pool::new());
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = got.clone();
        let on_progress: Arc<Mutex<dyn FnMut(ScanMetadataProgress) + Send>> = Arc::new(Mutex::new(move |p: ScanMetadataProgress| sink.lock().unwrap().push(p.track.file_id)));
        let q2 = q.clone();
        let result = pool.spawn(async move { scan_library_metadata_queued(&q2, MetadataTask::background("meta", "Metadata"), s, Arc::new(ImmediateIdle), vec![track("a"), track("b")], false, None, Some(on_progress)).await });
        pool.run_until_stalled();
        assert_eq!(result.take(), Some(Ok(())));
        assert_eq!(*got.lock().unwrap(), ["a", "b"]);
        assert!(q.tasks().is_empty(), "the task left the queue");
    }

    #[test]
    fn a_request_made_while_a_pass_runs_joins_it_instead_of_scanning_everything_twice() {
        // A scheduler that grants only one track per chunk and yields in between, so the second request arrives mid-pass.
        struct OneAtATime;
        impl IdleScheduler for OneAtATime {
            async fn wait_idle(&self, _t: u32) -> IdleDeadline {
                yield_now().await;
                IdleDeadline::new(false, || 0.0)
            }
        }
        let (q, s, pool) = (queue(), svc_for(&["a", "b", "c", "d"]), Pool::new());
        let order = Arc::new(Mutex::new(Vec::new()));
        let spawn_scan = |ids: &[&str]| {
            let (q, s, o) = (q.clone(), s.clone(), order.clone());
            let on_progress: Arc<Mutex<dyn FnMut(ScanMetadataProgress) + Send>> = Arc::new(Mutex::new(move |p: ScanMetadataProgress| o.lock().unwrap().push(p.track.file_id)));
            let tracks: Vec<_> = ids.iter().map(|i| track(i)).collect();
            pool.spawn(async move { scan_library_metadata_queued(&q, MetadataTask::background("meta", "Metadata"), s, Arc::new(OneAtATime), tracks, false, None, Some(on_progress)).await })
        };
        let first = spawn_scan(&["a", "b", "c"]);
        pool.step();
        pool.step();
        // Joins while the first pass still has tracks left (it would otherwise finish in the same executor round).
        let second = spawn_scan(&["a", "b", "c", "d"]);
        pool.run_until_stalled();
        assert_eq!((first.take(), second.take()), (Some(Ok(())), Some(Ok(()))));
        let mut seen = order.lock().unwrap().clone();
        seen.sort();
        assert_eq!(seen, ["a", "b", "c", "d"], "nothing scanned twice, and the late request's extra track was picked up");
    }
}
