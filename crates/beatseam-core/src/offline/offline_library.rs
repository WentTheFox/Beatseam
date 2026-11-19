use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::types::*;
use crate::js_compat::js_number;
use crate::library_store::{LibraryStore, StoreError};
use crate::tasks::{AbortHandle, Gate};

const INDEX_KEY: &str = "offline.index";
const BUDGET_KEY: &str = "offline.budgetBytes";
const CACHE_ON_PLAY_KEY: &str = "offline.cacheOnPlay";
const MAX_FAILURES: usize = 50;

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Tuning, with the TypeScript's defaults.
pub struct OfflineOptions {
    /// Simultaneous downloads.
    pub concurrency: usize,
    pub max_attempts: u32,
    /// Delay before attempt number `attempt` + 1 (attempt counts from 1).
    pub retry_delay_ms: Box<dyn Fn(u32) -> u64>,
}

impl Default for OfflineOptions {
    fn default() -> Self {
        Self { concurrency: 2, max_attempts: 3, retry_delay_ms: Box::new(|attempt| 2000 * 4u64.pow(attempt - 1)) }
    }
}

/// `JSON.stringify` of the TypeScript's `OfflineRecord[]` (the same setting is read by both implementations).
pub fn encode_offline_index(records: &[OfflineRecord]) -> String {
    let items: Vec<String> = records
        .iter()
        .map(|r| {
            format!(
                r#"{{"fileId":{},"sizeBytes":{},"sha256":{},"downloadedAtMs":{},"lastUsedAtMs":{},"pinned":{}}}"#,
                serde_json::to_string(&r.file_id).expect("string"),
                r.size_bytes,
                serde_json::to_string(&r.sha256).expect("string"),
                js_number(r.downloaded_at_ms as f64),
                js_number(r.last_used_at_ms as f64),
                r.pinned,
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// The inverse. A corrupt value is "no index" (the TypeScript treated unparseable JSON as empty); entries that lack a
/// field are dropped rather than trusted.
pub fn decode_offline_index(raw: &str) -> Vec<OfflineRecord> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(raw) else { return Vec::new() };
    items
        .iter()
        .filter_map(|v| {
            Some(OfflineRecord {
                file_id: v.get("fileId")?.as_str()?.to_owned(),
                size_bytes: v.get("sizeBytes")?.as_f64()? as u64,
                sha256: v.get("sha256")?.as_str()?.to_owned(),
                downloaded_at_ms: v.get("downloadedAtMs")?.as_f64()? as u64,
                last_used_at_ms: v.get("lastUsedAtMs")?.as_f64()? as u64,
                pinned: v.get("pinned")?.as_bool()?,
            })
        })
        .collect()
}

struct Entry {
    seq: u64,
    record: OfflineRecord,
}

/// One playback fetch in progress, which later callers for the same track join.
struct PlayFlight {
    done: Gate,
    result: Mutex<Option<bool>>,
}

struct Inner {
    records: HashMap<String, Entry>,
    next_seq: u64,
    queue: VecDeque<String>,
    attempts: HashMap<String, u32>,
    active: HashMap<String, AbortHandle>,
    progress: HashMap<String, u64>,
    /// Whether each queued track was asked for to keep (pinned) or only to have at hand (evictable).
    wants_pin: HashMap<String, bool>,
    removal_total: usize,
    removal_done: usize,
    failures: Vec<OfflineFailure>,
    budget_bytes: u64,
    cache_on_play: bool,
    paused: bool,
    last_server_offline: bool,
    play_fetches: HashMap<String, Arc<PlayFlight>>,
    /// Open once the latest index save has finished; the next save waits on it, so saves never interleave.
    persist_tail: Gate,
    /// Bumped by `cancel_all`, which makes retry timers still sleeping give up.
    timer_epoch: u64,
    listeners: Vec<(u64, Arc<dyn Fn(&OfflineState)>)>,
    next_listener: u64,
}

struct Shared<B, S, V, R> {
    backend: B,
    store: Arc<S>,
    source: V,
    runtime: R,
    options: OfflineOptions,
    inner: Mutex<Inner>,
}

/// Keeps chosen tracks on the device so they play with no connection: a download queue (a couple at a time, with retries),
/// checking each file against the hash the server gave, forgetting a copy once the file changes on the server, and evicting
/// the least recently played unpinned copies when over a storage budget. Nothing here touches the network itself - the
/// platform backend does the transfer. Cloning shares it.
pub struct OfflineLibrary<B, S, V, R> {
    shared: Arc<Shared<B, S, V, R>>,
}

impl<B, S, V, R> Clone for OfflineLibrary<B, S, V, R> {
    fn clone(&self) -> Self {
        Self { shared: self.shared.clone() }
    }
}

/// Opens the persist gate for the next saver even if the saving future is dropped.
struct PersistTurn(Gate);

impl Drop for PersistTurn {
    fn drop(&mut self) {
        self.0.open();
    }
}

struct FlightEnd<'a> {
    inner: &'a Mutex<Inner>,
    file_id: String,
    flight: Arc<PlayFlight>,
}

impl Drop for FlightEnd<'_> {
    fn drop(&mut self) {
        self.inner.lock().unwrap().play_fetches.remove(&self.file_id);
        self.flight.done.open();
    }
}

impl<B, S, V, R> OfflineLibrary<B, S, V, R>
where
    B: OfflineBackend + 'static,
    S: LibraryStore + 'static,
    V: OfflineSource + 'static,
    R: OfflineRuntime + 'static,
{
    /// Reads the saved index (local storage only), then starts reconciling it with what is actually on disk in the background.
    pub async fn open(backend: B, store: Arc<S>, source: V, runtime: R, options: OfflineOptions) -> Result<Self, StoreError> {
        let raw_index = store.get_setting(INDEX_KEY).await?;
        let raw_budget = store.get_setting(BUDGET_KEY).await?;
        let raw_cache = store.get_setting(CACHE_ON_PLAY_KEY).await?;
        let open_gate = Gate::new();
        open_gate.open();
        let mut inner = Inner {
            records: HashMap::new(),
            next_seq: 0,
            queue: VecDeque::new(),
            attempts: HashMap::new(),
            active: HashMap::new(),
            progress: HashMap::new(),
            wants_pin: HashMap::new(),
            removal_total: 0,
            removal_done: 0,
            failures: Vec::new(),
            budget_bytes: raw_budget.and_then(|b| b.parse::<f64>().ok()).filter(|b| b.is_finite() && *b > 0.0).map_or(0, |b| b as u64),
            cache_on_play: raw_cache.as_deref() == Some("1"),
            paused: false,
            last_server_offline: false,
            play_fetches: HashMap::new(),
            persist_tail: open_gate,
            timer_epoch: 0,
            listeners: Vec::new(),
            next_listener: 0,
        };
        for record in raw_index.as_deref().map(decode_offline_index).unwrap_or_default() {
            inner.next_seq += 1;
            let seq = inner.next_seq;
            inner.records.insert(record.file_id.clone(), Entry { seq, record });
        }
        let library = Self { shared: Arc::new(Shared { backend, store, source, runtime, options, inner: Mutex::new(inner) }) };
        let background = library.clone();
        library.shared.runtime.spawn(Box::pin(async move { background.reconcile().await }));
        Ok(library)
    }

    // ---------------------------------------------------------------- state

    fn compute_state(inner: &Inner) -> OfflineState {
        let requested = inner.queue.iter().chain(inner.active.keys()).filter(|id| inner.wants_pin.get(*id) == Some(&true)).count();
        OfflineState {
            downloaded_count: inner.records.len(),
            downloaded_bytes: inner.records.values().map(|e| e.record.size_bytes).sum(),
            queued_count: inner.queue.len(),
            active_count: inner.active.len(),
            requested_count: requested,
            removing: (inner.removal_total > 0).then_some((inner.removal_done, inner.removal_total)),
            progress: inner.progress.iter().map(|(k, v)| (k.clone(), *v)).collect::<BTreeMap<_, _>>(),
            budget_bytes: inner.budget_bytes,
            cache_on_play: inner.cache_on_play,
            paused: inner.paused,
            failures: inner.failures.clone(),
        }
    }

    pub fn get_state(&self) -> OfflineState {
        Self::compute_state(&self.shared.inner.lock().unwrap())
    }

    /// Calls `listener` on every change. Returns an id for [`unsubscribe`](Self::unsubscribe).
    pub fn subscribe(&self, listener: impl Fn(&OfflineState) + 'static) -> u64 {
        let mut inner = self.shared.inner.lock().unwrap();
        inner.next_listener += 1;
        let id = inner.next_listener;
        inner.listeners.push((id, Arc::new(listener)));
        id
    }

    pub fn unsubscribe(&self, id: u64) {
        self.shared.inner.lock().unwrap().listeners.retain(|(i, _)| *i != id);
    }

    fn publish(&self) {
        let (state, listeners) = {
            let inner = self.shared.inner.lock().unwrap();
            (Self::compute_state(&inner), inner.listeners.iter().map(|(_, l)| l.clone()).collect::<Vec<_>>())
        };
        for listener in listeners {
            listener(&state);
        }
    }

    /// Whether a (possibly stale - see [`read`](Self::read)) copy of this track is stored. Cheap enough to call per row.
    pub fn is_available(&self, file_id: &str) -> bool {
        self.shared.inner.lock().unwrap().records.contains_key(file_id)
    }

    pub fn is_pinned(&self, file_id: &str) -> bool {
        self.shared.inner.lock().unwrap().records.get(file_id).is_some_and(|e| e.record.pinned)
    }

    // ---------------------------------------------------------------- loading and saving

    /// Drops index entries whose file is gone, and files nothing in the index owns (left by an interrupted run). Background work.
    async fn reconcile(&self) {
        // Storage we can't list is storage we can't reconcile; the index stays as it was.
        let Ok(listed) = self.shared.backend.list().await else { return };
        let on_disk: HashMap<String, u64> = listed.into_iter().collect();
        let mut changed = false;
        {
            let mut inner = self.shared.inner.lock().unwrap();
            let gone: Vec<String> = inner.records.keys().filter(|id| !on_disk.contains_key(*id)).cloned().collect();
            for id in gone {
                inner.records.remove(&id);
                changed = true;
            }
        }
        for file_id in on_disk.keys() {
            let owned = {
                let inner = self.shared.inner.lock().unwrap();
                inner.records.contains_key(file_id) || inner.active.contains_key(file_id)
            };
            if !owned {
                let _ = self.shared.backend.remove(file_id).await;
            }
        }
        if changed {
            self.persist().await;
        }
        self.publish();
    }

    /// Serialised, so overlapping saves can't interleave and leave an older index last. Each save writes the index as it is
    /// when its turn comes.
    async fn persist(&self) {
        let (previous, mine) = {
            let mut inner = self.shared.inner.lock().unwrap();
            let mine = Gate::new();
            (std::mem::replace(&mut inner.persist_tail, mine.clone()), mine)
        };
        let _turn = PersistTurn(mine);
        previous.wait().await;
        let snapshot = {
            let inner = self.shared.inner.lock().unwrap();
            let mut entries: Vec<&Entry> = inner.records.values().collect();
            entries.sort_by_key(|e| e.seq);
            encode_offline_index(&entries.iter().map(|e| e.record.clone()).collect::<Vec<_>>())
        };
        let _ = self.shared.store.put_setting(INDEX_KEY, &snapshot).await;
    }

    fn persist_in_background(&self) {
        let this = self.clone();
        self.shared.runtime.spawn(Box::pin(async move { this.persist().await }));
    }

    // ---------------------------------------------------------------- reading

    /// The stored bytes, or `None` if none can be trusted: not downloaded, the file is gone from disk, or the server has
    /// since replaced the file (its hash no longer matches what was downloaded), in which case the stale copy is dropped
    /// (and fetched again, if it was one the person asked to keep).
    pub async fn read(&self, file_id: &str) -> Result<Option<Vec<u8>>, BackendError> {
        let Some((sha256, pinned)) = self.shared.inner.lock().unwrap().records.get(file_id).map(|e| (e.record.sha256.clone(), e.record.pinned)) else { return Ok(None) };
        let current = self.shared.store.get_metadata(file_id).await.ok().flatten().and_then(|m| m.content_hash);
        if current.as_deref().is_some_and(|c| is_sha256_hex(c) && c != sha256) {
            self.forget(&[file_id.to_owned()]).await;
            if pinned {
                self.request(&[file_id.to_owned()], true);
            }
            return Ok(None);
        }
        let Some(bytes) = self.shared.backend.read(file_id).await? else {
            self.shared.inner.lock().unwrap().records.remove(file_id);
            self.persist_in_background();
            self.publish();
            return Ok(None);
        };
        let now = self.shared.runtime.now_ms();
        if let Some(entry) = self.shared.inner.lock().unwrap().records.get_mut(file_id) {
            entry.record.last_used_at_ms = now;
        }
        Ok(Some(bytes))
    }

    // ---------------------------------------------------------------- choosing what to keep

    /// Makes these tracks available offline: queues whichever aren't stored (and fresh) yet. `pin` (the TypeScript's
    /// default is `true`) is whether they are kept for good or only until the budget needs the space.
    pub fn request(&self, file_ids: &[String], pin: bool) {
        {
            let mut inner = self.shared.inner.lock().unwrap();
            for file_id in file_ids {
                if let Some(entry) = inner.records.get_mut(file_id) {
                    if pin {
                        entry.record.pinned = true;
                    }
                    continue;
                }
                inner.failures.retain(|f| &f.file_id != file_id);
                inner.attempts.remove(file_id);
                let wanted = pin || inner.wants_pin.get(file_id) == Some(&true);
                inner.wants_pin.insert(file_id.clone(), wanted);
                if !inner.queue.contains(file_id) && !inner.active.contains_key(file_id) {
                    inner.queue.push_back(file_id.clone());
                }
            }
        }
        self.persist_in_background();
        self.publish();
        self.pump();
    }

    /// Stops keeping these (and cancels any in flight), freeing their space.
    pub async fn remove(&self, file_ids: &[String]) {
        self.cancel(file_ids);
        self.forget(file_ids).await;
    }

    /// Forgets everything downloaded and cancels everything queued.
    pub async fn clear(&self) {
        self.cancel_all();
        let mut all: Vec<(u64, String)> = self.shared.inner.lock().unwrap().records.iter().map(|(id, e)| (e.seq, id.clone())).collect();
        all.sort();
        self.forget(&all.into_iter().map(|(_, id)| id).collect::<Vec<_>>()).await;
    }

    async fn forget(&self, file_ids: &[String]) {
        self.shared.inner.lock().unwrap().removal_total += file_ids.len();
        self.publish();
        for file_id in file_ids {
            self.shared.inner.lock().unwrap().records.remove(file_id);
            let _ = self.shared.backend.remove(file_id).await;
            self.shared.inner.lock().unwrap().removal_done += 1;
            self.publish();
        }
        self.persist().await;
        {
            // Overlapping removals share one run: it is over only when everything asked for is gone.
            let mut inner = self.shared.inner.lock().unwrap();
            if inner.removal_done >= inner.removal_total {
                inner.removal_done = 0;
                inner.removal_total = 0;
            }
        }
        self.publish();
    }

    /// Takes tracks out of the queue and aborts any transfer in flight. Already-downloaded copies stay.
    pub fn cancel(&self, file_ids: &[String]) {
        let aborts: Vec<AbortHandle> = {
            let mut inner = self.shared.inner.lock().unwrap();
            inner.queue.retain(|id| !file_ids.contains(id));
            for id in file_ids {
                inner.wants_pin.remove(id);
            }
            file_ids.iter().filter_map(|id| inner.active.get(id).cloned()).collect()
        };
        for abort in aborts {
            abort.abort();
        }
        self.publish();
    }

    pub fn cancel_all(&self) {
        let aborts: Vec<AbortHandle> = {
            let mut inner = self.shared.inner.lock().unwrap();
            inner.queue.clear();
            inner.timer_epoch += 1;
            inner.active.values().cloned().collect()
        };
        for abort in aborts {
            abort.abort();
        }
        self.publish();
    }

    /// Limit on what unpinned downloads may occupy. 0 means no limit.
    pub async fn set_budget_bytes(&self, bytes: u64) -> Result<(), StoreError> {
        self.shared.inner.lock().unwrap().budget_bytes = bytes;
        self.shared.store.put_setting(BUDGET_KEY, &bytes.to_string()).await?;
        self.evict(&[]).await;
        self.publish();
        Ok(())
    }

    /// Whether tracks played from the server are kept (unpinned, so the budget can evict them).
    pub async fn set_cache_on_play(&self, on: bool) -> Result<(), StoreError> {
        self.shared.inner.lock().unwrap().cache_on_play = on;
        self.shared.store.put_setting(CACHE_ON_PLAY_KEY, if on { "1" } else { "0" }).await?;
        self.publish();
        Ok(())
    }

    /// For playback with cache-on-play: fetches the track into storage (unpinned) so one transfer both plays it and keeps
    /// it, instead of streaming it now and downloading it again. Resolves `true` when a copy is stored; any failure leaves
    /// no trace (no retry, no pause) and the caller streams it as it would have. Two readers of one track share a single
    /// transfer.
    pub async fn fetch_for_playback(&self, file_id: &str) -> bool {
        let running = self.shared.inner.lock().unwrap().play_fetches.get(file_id).cloned();
        if let Some(flight) = running {
            flight.done.wait().await;
            return flight.result.lock().unwrap().unwrap_or(false);
        }
        let flight = Arc::new(PlayFlight { done: Gate::new(), result: Mutex::new(None) });
        {
            let mut inner = self.shared.inner.lock().unwrap();
            if !inner.cache_on_play {
                return false;
            }
            if inner.records.contains_key(file_id) {
                return true;
            }
            // Already being fetched on purpose (queued or active as a download): leave that alone.
            if inner.queue.iter().any(|id| id == file_id) || inner.active.contains_key(file_id) {
                return false;
            }
            inner.play_fetches.insert(file_id.to_owned(), flight.clone());
        }
        let _end = FlightEnd { inner: &self.shared.inner, file_id: file_id.to_owned(), flight: flight.clone() };
        let abort = AbortHandle::new();
        {
            let mut inner = self.shared.inner.lock().unwrap();
            inner.active.insert(file_id.to_owned(), abort.clone());
            inner.progress.insert(file_id.to_owned(), 0);
            inner.wants_pin.insert(file_id.to_owned(), false);
        }
        self.publish();
        let stored = self.download(file_id, &abort).await.is_ok();
        if !stored {
            self.shared.inner.lock().unwrap().wants_pin.remove(file_id);
        }
        {
            let mut inner = self.shared.inner.lock().unwrap();
            inner.active.remove(file_id);
            inner.progress.remove(file_id);
        }
        *flight.result.lock().unwrap() = Some(stored);
        self.publish();
        self.pump();
        stored
    }

    /// Removes the least recently played unpinned copies until the total fits the budget. Pinned copies are never evicted.
    async fn evict(&self, protect: &[&str]) {
        let evicted: Vec<String> = {
            let inner = self.shared.inner.lock().unwrap();
            if inner.budget_bytes == 0 {
                return;
            }
            let mut total: u64 = inner.records.values().map(|e| e.record.size_bytes).sum();
            if total <= inner.budget_bytes {
                return;
            }
            let mut candidates: Vec<&Entry> = inner.records.values().filter(|e| !e.record.pinned && !protect.contains(&e.record.file_id.as_str())).collect();
            candidates.sort_by_key(|e| (e.record.last_used_at_ms, e.seq));
            let mut evicted = Vec::new();
            for entry in candidates {
                if total <= inner.budget_bytes {
                    break;
                }
                total -= entry.record.size_bytes;
                evicted.push(entry.record.file_id.clone());
            }
            evicted
        };
        if !evicted.is_empty() {
            self.forget(&evicted).await;
        }
    }

    // ---------------------------------------------------------------- the queue

    pub fn pause(&self) {
        self.shared.inner.lock().unwrap().paused = true;
        self.publish();
    }

    pub fn resume(&self) {
        {
            let mut inner = self.shared.inner.lock().unwrap();
            if !inner.paused {
                return;
            }
            inner.paused = false;
        }
        self.publish();
        self.pump();
    }

    /// A check-in that reached the server is the cue to pick downloads back up. Call on every server status change.
    pub fn on_server_status(&self, status: ServerStatusSummary) {
        let resume = {
            let mut inner = self.shared.inner.lock().unwrap();
            let recovered = inner.last_server_offline && !status.offline;
            inner.last_server_offline = status.offline;
            recovered || (inner.paused && !status.offline && status.active && status.last_synced_at_ms.is_some())
        };
        if resume {
            self.resume();
        }
    }

    fn pump(&self) {
        loop {
            let next = {
                let mut inner = self.shared.inner.lock().unwrap();
                if inner.paused || inner.active.len() >= self.shared.options.concurrency {
                    return;
                }
                let Some(file_id) = inner.queue.pop_front() else { return };
                // Claimed here, not when the spawned task first runs, so a second pump can't hand out more than the limit.
                let abort = AbortHandle::new();
                inner.active.insert(file_id.clone(), abort.clone());
                inner.progress.insert(file_id.clone(), 0);
                (file_id, abort)
            };
            self.publish();
            let this = self.clone();
            self.shared.runtime.spawn(Box::pin(async move { this.run(next.0, next.1).await }));
        }
    }

    async fn run(&self, file_id: String, abort: AbortHandle) {
        match self.download(&file_id, &abort).await {
            Ok(()) => {
                self.shared.inner.lock().unwrap().attempts.remove(&file_id);
            }
            Err(error) => self.handle_failure(&file_id, error, abort.signal().is_aborted()),
        }
        {
            let mut inner = self.shared.inner.lock().unwrap();
            inner.active.remove(&file_id);
            inner.progress.remove(&file_id);
        }
        self.publish();
        self.pump();
    }

    async fn download(&self, file_id: &str, abort: &AbortHandle) -> Result<(), DownloadError> {
        let Some(target) = self.shared.source.download_target(file_id) else { return Err(DownloadError::Network) };
        let expected = self.shared.store.get_metadata(file_id).await.ok().flatten().and_then(|m| m.content_hash);
        let on_progress = |bytes: u64| {
            self.shared.inner.lock().unwrap().progress.insert(file_id.to_owned(), bytes);
            self.publish();
        };
        let result = self.shared.backend.download(DownloadRequest { file_id, url: target.url, headers: target.headers, signal: abort.signal(), on_progress: &on_progress }).await?;
        if expected.as_deref().is_some_and(|e| is_sha256_hex(e) && e != result.sha256) {
            let _ = self.shared.backend.remove(file_id).await;
            return Err(DownloadError::Other("The downloaded file does not match what the server has".into()));
        }
        let at = self.shared.runtime.now_ms();
        {
            let mut inner = self.shared.inner.lock().unwrap();
            let pinned = inner.wants_pin.remove(file_id).unwrap_or(true);
            inner.next_seq += 1;
            let seq = inner.next_seq;
            inner.records.insert(file_id.to_owned(), Entry { seq, record: OfflineRecord { file_id: file_id.to_owned(), size_bytes: result.size_bytes, sha256: result.sha256, downloaded_at_ms: at, last_used_at_ms: at, pinned } });
        }
        self.persist().await;
        self.evict(&[file_id]).await;
        Ok(())
    }

    fn handle_failure(&self, file_id: &str, error: DownloadError, cancelled: bool) {
        if cancelled {
            return;
        }
        let mut inner = self.shared.inner.lock().unwrap();
        let add_failure = |inner: &mut Inner, message: String| {
            inner.failures.retain(|f| f.file_id != file_id);
            inner.failures.push(OfflineFailure { file_id: file_id.to_owned(), message });
            let excess = inner.failures.len().saturating_sub(MAX_FAILURES);
            inner.failures.drain(..excess);
        };
        match error {
            DownloadError::Network => {
                // Not this track's fault: keep its place, stop trying until the server can be reached again.
                inner.queue.push_front(file_id.to_owned());
                inner.paused = true;
            }
            DownloadError::Unauthorized => {
                inner.queue.push_front(file_id.to_owned());
                inner.paused = true;
                add_failure(&mut inner, "Signed out by the server".into());
            }
            DownloadError::NotFound => add_failure(&mut inner, "No longer on the server".into()),
            DownloadError::Other(message) => {
                let attempt = inner.attempts.get(file_id).copied().unwrap_or(0) + 1;
                inner.attempts.insert(file_id.to_owned(), attempt);
                if attempt >= self.shared.options.max_attempts {
                    add_failure(&mut inner, message);
                    return;
                }
                let epoch = inner.timer_epoch;
                drop(inner);
                let delay = (self.shared.options.retry_delay_ms)(attempt);
                let this = self.clone();
                let file_id = file_id.to_owned();
                self.shared.runtime.spawn(Box::pin(async move {
                    this.shared.runtime.sleep(delay).await;
                    {
                        let mut inner = this.shared.inner.lock().unwrap();
                        if inner.timer_epoch != epoch {
                            return;
                        }
                        inner.queue.push_back(file_id);
                    }
                    this.publish();
                    this.pump();
                }));
            }
        }
    }
}
