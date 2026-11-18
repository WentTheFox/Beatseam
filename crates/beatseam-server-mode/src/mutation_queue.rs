use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use beatseam_api::mutations::{MutationDto, MutationStatus, MutationsResponseDto};
use beatseam_api::{ApiClient, ApiClientError};
use beatseam_core::http::HttpTransport;
use beatseam_core::library_store::{LibraryStore, StoreError};
use beatseam_core::tasks::Gate;

const QUEUE_KEY: &str = "serverMode.mutationQueue";
const BATCH_SIZE: usize = 50;
const MAX_REJECTIONS: usize = 20;

/// Where queued operations are sent: the signed-in server's client.
pub trait MutationSender {
    fn send_mutations(&self, mutations: Vec<MutationDto>) -> impl Future<Output = Result<MutationsResponseDto, ApiClientError>>;
}

impl<T: HttpTransport> MutationSender for ApiClient<T> {
    fn send_mutations(&self, mutations: Vec<MutationDto>) -> impl Future<Output = Result<MutationsResponseDto, ApiClientError>> {
        self.mutations(mutations)
    }
}

/// Runs a future to completion in the background (the host's executor; the core is runtime-free).
pub type Spawn = Box<dyn Fn(Pin<Box<dyn Future<Output = ()>>>)>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejection {
    pub op_id: String,
    pub error: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct MutationQueueState {
    /// Changes made here that the server hasn't acknowledged yet.
    pub pending: usize,
    pub flushing: bool,
    /// Operations the server refused for good (not yours, too large, ...), newest last.
    pub rejected: Vec<Rejection>,
}

fn set_op_id(mutation: &mut MutationDto, id: String) {
    match mutation {
        MutationDto::PlaylistCreate { op_id, .. }
        | MutationDto::PlaylistRename { op_id, .. }
        | MutationDto::PlaylistDelete { op_id, .. }
        | MutationDto::PlaylistAddTracks { op_id, .. }
        | MutationDto::PlaylistRemoveTracks { op_id, .. }
        | MutationDto::PlaylistSetTracks { op_id, .. }
        | MutationDto::PlaybackStateSet { op_id, .. }
        | MutationDto::SettingSet { op_id, .. }
        | MutationDto::SettingDelete { op_id, .. } => *op_id = id,
    }
}

fn setting_key(mutation: &MutationDto) -> Option<&str> {
    match mutation {
        MutationDto::SettingSet { key, .. } | MutationDto::SettingDelete { key, .. } => Some(key),
        _ => None,
    }
}

struct Inner {
    queue: Vec<MutationDto>,
    flushing: Option<Gate>,
    rejected: Vec<Rejection>,
    persist_tail: Gate,
    listeners: Vec<(u64, Arc<dyn Fn(&MutationQueueState)>)>,
    next_listener: u64,
}

struct Shared<S, A> {
    store: Arc<S>,
    get_api: Box<dyn Fn() -> Option<Arc<A>>>,
    new_id: Box<dyn Fn() -> String>,
    spawn: Spawn,
    inner: Mutex<Inner>,
}

/// Changes made on this device, kept until the server has them: written down first, sent later (right away if the server
/// can be reached, otherwise at the next check-in). Each carries its own id, so sending a batch again after a dropped
/// connection can't apply anything twice. Cloning shares it.
pub struct MutationQueue<S, A> {
    shared: Arc<Shared<S, A>>,
}

impl<S, A> Clone for MutationQueue<S, A> {
    fn clone(&self) -> Self {
        Self { shared: self.shared.clone() }
    }
}

struct PersistTurn(Gate);

impl Drop for PersistTurn {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// Marks a flush as over (and releases anyone joined to it) even if the flushing future is dropped.
struct FlushEnd<'a, S: LibraryStore + 'static, A: MutationSender + 'static> {
    queue: &'a MutationQueue<S, A>,
    gate: Gate,
}

impl<S: LibraryStore + 'static, A: MutationSender + 'static> Drop for FlushEnd<'_, S, A> {
    fn drop(&mut self) {
        self.queue.shared.inner.lock().unwrap().flushing = None;
        self.queue.publish();
        self.gate.open();
    }
}

impl<S: LibraryStore + 'static, A: MutationSender + 'static> MutationQueue<S, A> {
    /// Loads what was left waiting by an earlier run.
    pub async fn open(store: Arc<S>, get_api: impl Fn() -> Option<Arc<A>> + 'static, new_id: impl Fn() -> String + 'static, spawn: Spawn) -> Result<Self, StoreError> {
        let raw = store.get_setting(QUEUE_KEY).await?;
        // A corrupt value is an empty queue; an entry that no longer parses is dropped on its own.
        let queue: Vec<MutationDto> = match raw.as_deref().filter(|r| !r.is_empty()).map(serde_json::from_str::<serde_json::Value>) {
            Some(Ok(serde_json::Value::Array(items))) => items.into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect(),
            _ => Vec::new(),
        };
        let open_gate = Gate::new();
        open_gate.open();
        let inner = Inner { queue, flushing: None, rejected: Vec::new(), persist_tail: open_gate, listeners: Vec::new(), next_listener: 0 };
        let queue = Self { shared: Arc::new(Shared { store, get_api: Box::new(get_api), new_id: Box::new(new_id), spawn, inner: Mutex::new(inner) }) };
        queue.publish();
        Ok(queue)
    }

    pub fn get_state(&self) -> MutationQueueState {
        Self::state_of(&self.shared.inner.lock().unwrap())
    }

    fn state_of(inner: &Inner) -> MutationQueueState {
        MutationQueueState { pending: inner.queue.len(), flushing: inner.flushing.is_some(), rejected: inner.rejected.clone() }
    }

    /// Calls `listener` on every change. Returns an id for [`unsubscribe`](Self::unsubscribe).
    pub fn subscribe(&self, listener: impl Fn(&MutationQueueState) + 'static) -> u64 {
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
            (Self::state_of(&inner), inner.listeners.iter().map(|(_, l)| l.clone()).collect::<Vec<_>>())
        };
        for listener in listeners {
            listener(&state);
        }
    }

    /// Serialised, so overlapping saves can't interleave and leave an older queue last; each writes the queue as it is when
    /// its turn comes.
    async fn persist(&self) {
        let (previous, mine) = {
            let mut inner = self.shared.inner.lock().unwrap();
            let mine = Gate::new();
            (std::mem::replace(&mut inner.persist_tail, mine.clone()), mine)
        };
        let _turn = PersistTurn(mine);
        previous.wait().await;
        let snapshot = serde_json::to_string(&self.shared.inner.lock().unwrap().queue).expect("mutations always serialize");
        let _ = self.shared.store.put_setting(QUEUE_KEY, &snapshot).await;
    }

    /// Writes the change down, then tries to send it without making the caller wait. Whatever `op_id` the mutation carries
    /// is replaced by a fresh one.
    pub async fn enqueue(&self, mut mutation: MutationDto) {
        {
            let mut inner = self.shared.inner.lock().unwrap();
            // A newer value for a setting makes any waiting older one pointless (last write wins anyway).
            if let Some(key) = setting_key(&mutation).map(str::to_owned) {
                inner.queue.retain(|m| setting_key(m) != Some(key.as_str()));
            }
            // Likewise only the newest playback state of a device matters.
            if let MutationDto::PlaybackStateSet { device_id, .. } = &mutation {
                inner.queue.retain(|m| !matches!(m, MutationDto::PlaybackStateSet { device_id: d, .. } if d == device_id));
            }
            set_op_id(&mut mutation, (self.shared.new_id)());
            inner.queue.push(mutation);
        }
        self.persist().await;
        self.publish();
        let this = self.clone();
        (self.shared.spawn)(Box::pin(async move { this.flush().await }));
    }

    /// Sends everything waiting, oldest first, in batches. What could not be sent stays queued (a failed send - offline,
    /// signed out, the server unwell - is retried at the next check-in). A flush already running is joined, not repeated.
    pub async fn flush(&self) {
        let running = self.shared.inner.lock().unwrap().flushing.clone();
        if let Some(gate) = running {
            gate.wait().await;
            return;
        }
        let gate = Gate::new();
        self.shared.inner.lock().unwrap().flushing = Some(gate.clone());
        self.publish();
        let _end = FlushEnd { queue: self, gate };
        self.drain().await;
    }

    async fn drain(&self) {
        let Some(api) = (self.shared.get_api)() else { return };
        loop {
            let batch: Vec<MutationDto> = {
                let inner = self.shared.inner.lock().unwrap();
                inner.queue.iter().take(BATCH_SIZE).cloned().collect()
            };
            if batch.is_empty() {
                return;
            }
            let Ok(response) = api.send_mutations(batch).await else { return };
            let done: HashSet<&str> = response.results.iter().map(|r| r.op_id.as_str()).collect();
            {
                let mut inner = self.shared.inner.lock().unwrap();
                for result in &response.results {
                    if result.status == MutationStatus::Rejected {
                        inner.rejected.push(Rejection { op_id: result.op_id.clone(), error: result.error.clone().unwrap_or_else(|| "Rejected".into()) });
                    }
                }
                let excess = inner.rejected.len().saturating_sub(MAX_REJECTIONS);
                inner.rejected.drain(..excess);
                // Anything the server answered for is finished with (applied, already applied, or refused for good).
                inner.queue.retain(|m| !done.contains(m.op_id()));
            }
            self.persist().await;
            self.publish();
            if response.results.is_empty() {
                return; // a server that answers nothing would loop forever
            }
        }
    }

    /// Forgets everything waiting (signing out: those changes belonged to that account).
    pub async fn clear(&self) {
        {
            let mut inner = self.shared.inner.lock().unwrap();
            inner.queue.clear();
            inner.rejected.clear();
        }
        self.persist().await;
        self.publish();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;

    use beatseam_api::mutations::MutationResultDto;
    use beatseam_core::library_store::MemoryLibraryStore;
    use beatseam_core::tasks::test_executor::Pool;

    use super::*;

    /// Answers each call from a script; an empty script applies everything it is sent.
    #[derive(Default)]
    struct Scripted {
        calls: RefCell<Vec<Vec<MutationDto>>>,
        script: RefCell<VecDeque<Result<Vec<(MutationStatus, Option<&'static str>)>, ApiClientError>>>,
        hold: RefCell<Option<Gate>>,
    }

    impl MutationSender for Scripted {
        async fn send_mutations(&self, mutations: Vec<MutationDto>) -> Result<MutationsResponseDto, ApiClientError> {
            self.calls.borrow_mut().push(mutations.clone());
            let hold = self.hold.borrow().clone();
            if let Some(gate) = hold {
                gate.wait().await;
            }
            let next = self.script.borrow_mut().pop_front();
            match next {
                Some(Err(e)) => Err(e),
                Some(Ok(statuses)) => Ok(MutationsResponseDto { results: mutations.iter().zip(statuses).map(|(m, (status, error))| MutationResultDto { op_id: m.op_id().into(), status, error: error.map(str::to_owned) }).collect() }),
                None => Ok(MutationsResponseDto { results: mutations.iter().map(|m| MutationResultDto { op_id: m.op_id().into(), status: MutationStatus::Applied, error: None }).collect() }),
            }
        }
    }

    struct Fixture {
        pool: Rc<Pool>,
        sender: Rc<Scripted>,
        store: Arc<MemoryLibraryStore>,
        queue: MutationQueue<MemoryLibraryStore, RcSender>,
        signed_in: Rc<Cell<bool>>,
    }

    /// `Arc` needs `Send` only when asked for it; the sender is shared with the test through an `Rc` inside.
    struct RcSender(Rc<Scripted>);

    impl MutationSender for RcSender {
        fn send_mutations(&self, mutations: Vec<MutationDto>) -> impl Future<Output = Result<MutationsResponseDto, ApiClientError>> {
            self.0.send_mutations(mutations)
        }
    }

    fn open_on(pool: &Rc<Pool>, sender: &Rc<Scripted>, store: &Arc<MemoryLibraryStore>, signed_in: &Rc<Cell<bool>>) -> MutationQueue<MemoryLibraryStore, RcSender> {
        let counter = Rc::new(Cell::new(0));
        let (s, st, si, p) = (sender.clone(), store.clone(), signed_in.clone(), pool.clone());
        let opened = pool.spawn(async move {
            MutationQueue::open(
                st,
                move || si.get().then(|| Arc::new(RcSender(s.clone()))),
                move || {
                    counter.set(counter.get() + 1);
                    format!("op{}", counter.get())
                },
                Box::new(move |f| {
                    p.spawn(f);
                }),
            )
            .await
        });
        pool.run_until_stalled();
        opened.take().unwrap().unwrap()
    }

    fn fixture() -> Fixture {
        let pool = Rc::new(Pool::new());
        let sender = Rc::new(Scripted::default());
        let store = Arc::new(MemoryLibraryStore::new());
        let signed_in = Rc::new(Cell::new(true));
        let queue = open_on(&pool, &sender, &store, &signed_in);
        Fixture { pool, sender, store, queue, signed_in }
    }

    impl Fixture {
        fn run<T: 'static>(&self, f: impl Future<Output = T> + 'static) -> T {
            let h = self.pool.spawn(f);
            self.pool.run_until_stalled();
            h.take().expect("finished")
        }

        fn enqueue(&self, m: MutationDto) {
            let q = self.queue.clone();
            self.run(async move { q.enqueue(m).await });
        }
    }

    fn setting(key: &str, value: &str) -> MutationDto {
        MutationDto::SettingSet { op_id: String::new(), key: key.into(), value: value.into() }
    }

    fn rename(id: &str) -> MutationDto {
        MutationDto::PlaylistRename { op_id: String::new(), id: id.into(), name: "n".into() }
    }

    fn playback(device: &str) -> MutationDto {
        MutationDto::PlaybackStateSet { op_id: String::new(), device_id: device.into(), state: Default::default() }
    }

    #[test]
    fn a_change_is_written_down_given_an_id_and_sent_right_away() {
        let f = fixture();
        f.enqueue(rename("p"));
        let calls = f.sender.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][0].op_id(), "op1");
        drop(calls);
        assert_eq!(f.queue.get_state(), MutationQueueState::default());
    }

    #[test]
    fn offline_changes_wait_in_a_persisted_queue_and_go_out_at_the_next_flush_in_a_new_session() {
        let f = fixture();
        f.signed_in.set(false);
        f.enqueue(rename("a"));
        f.enqueue(rename("b"));
        assert_eq!(f.queue.get_state().pending, 2);
        assert!(f.sender.calls.borrow().is_empty());

        let reopened = open_on(&f.pool, &f.sender, &f.store, &f.signed_in);
        assert_eq!(reopened.get_state().pending, 2, "survived a restart");
        f.signed_in.set(true);
        let q = reopened.clone();
        f.run(async move { q.flush().await });
        assert_eq!(f.sender.calls.borrow()[0].iter().map(|m| m.op_id()).collect::<Vec<_>>(), ["op1", "op2"]);
        assert_eq!(reopened.get_state().pending, 0);
        let stored = f.run({ let s = f.store.clone(); async move { s.get_setting("serverMode.mutationQueue").await.unwrap() } });
        assert_eq!(stored.as_deref(), Some("[]"));
    }

    #[test]
    fn a_newer_setting_or_playback_state_replaces_the_waiting_older_one() {
        let f = fixture();
        f.signed_in.set(false);
        f.enqueue(setting("theme", "dark"));
        f.enqueue(setting("accent", "red"));
        f.enqueue(setting("theme", "light"));
        f.enqueue(MutationDto::SettingDelete { op_id: String::new(), key: "accent".into() });
        f.enqueue(playback("d1"));
        f.enqueue(playback("d2"));
        f.enqueue(playback("d1"));
        f.enqueue(rename("p"));
        f.enqueue(rename("p"));
        let q = f.queue.clone();
        f.signed_in.set(true);
        f.run(async move { q.flush().await });
        let sent = f.sender.calls.borrow()[0].clone();
        let summary: Vec<String> = sent
            .iter()
            .map(|m| match m {
                MutationDto::SettingSet { key, value, .. } => format!("set {key}={value}"),
                MutationDto::SettingDelete { key, .. } => format!("delete {key}"),
                MutationDto::PlaybackStateSet { device_id, .. } => format!("playback {device_id}"),
                MutationDto::PlaylistRename { .. } => "rename".into(),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(summary, ["set theme=light", "delete accent", "playback d2", "playback d1", "rename", "rename"], "playlist renames are all kept");
    }

    #[test]
    fn a_long_queue_goes_out_in_batches_of_fifty() {
        let f = fixture();
        f.signed_in.set(false);
        for i in 0..120 {
            f.enqueue(rename(&i.to_string()));
        }
        f.signed_in.set(true);
        let q = f.queue.clone();
        f.run(async move { q.flush().await });
        assert_eq!(f.sender.calls.borrow().iter().map(Vec::len).collect::<Vec<_>>(), [50, 50, 20]);
    }

    #[test]
    fn a_failed_send_keeps_everything_for_the_next_check_in() {
        let f = fixture();
        f.sender.script.borrow_mut().push_back(Err(ApiClientError::Network("offline".into())));
        f.enqueue(rename("a"));
        assert_eq!(f.queue.get_state().pending, 1);
        let q = f.queue.clone();
        f.run(async move { q.flush().await });
        assert_eq!(f.queue.get_state().pending, 0, "the next attempt got through");
        assert_eq!(f.sender.calls.borrow().len(), 2);
        assert_eq!(f.sender.calls.borrow()[0][0].op_id(), f.sender.calls.borrow()[1][0].op_id(), "resent under the same id, so it can't apply twice");
    }

    #[test]
    fn rejected_operations_are_dropped_and_remembered_up_to_twenty() {
        let f = fixture();
        f.signed_in.set(false);
        for i in 0..25 {
            f.enqueue(rename(&i.to_string()));
        }
        f.sender.script.borrow_mut().push_back(Ok((0..25).map(|_| (MutationStatus::Rejected, Some("not yours"))).collect()));
        f.signed_in.set(true);
        let q = f.queue.clone();
        f.run(async move { q.flush().await });
        let state = f.queue.get_state();
        assert_eq!(state.pending, 0);
        assert_eq!(state.rejected.len(), 20, "25 refused, the cap keeps the newest 20");
        assert_eq!(state.rejected.last().unwrap(), &Rejection { op_id: "op25".into(), error: "not yours".into() });
        assert_eq!(state.rejected[0].op_id, "op6");
    }

    #[test]
    fn a_rejection_without_a_message_says_rejected() {
        let f = fixture();
        f.sender.script.borrow_mut().push_back(Ok(vec![(MutationStatus::Rejected, None)]));
        f.enqueue(rename("a"));
        assert_eq!(f.queue.get_state().rejected[0].error, "Rejected");
    }

    #[test]
    fn a_server_that_answers_nothing_does_not_loop_forever() {
        let f = fixture();
        f.sender.script.borrow_mut().push_back(Ok(vec![]));
        f.enqueue(rename("a"));
        assert_eq!(f.sender.calls.borrow().len(), 1);
        assert_eq!(f.queue.get_state().pending, 1);
    }

    #[test]
    fn overlapping_flushes_share_one_run_and_the_state_says_so() {
        let f = fixture();
        f.signed_in.set(false);
        f.enqueue(rename("a"));
        f.signed_in.set(true);
        *f.sender.hold.borrow_mut() = Some(Gate::new());
        let heard = Rc::new(RefCell::new(Vec::new()));
        let h = heard.clone();
        f.queue.subscribe(move |s| h.borrow_mut().push((s.pending, s.flushing)));
        let (q1, q2) = (f.queue.clone(), f.queue.clone());
        let first = f.pool.spawn(async move { q1.flush().await });
        let second = f.pool.spawn(async move { q2.flush().await });
        f.pool.run_until_stalled();
        assert_eq!(f.sender.calls.borrow().len(), 1);
        assert!(f.queue.get_state().flushing);
        f.sender.hold.borrow().clone().unwrap().open();
        f.pool.run_until_stalled();
        assert!(first.is_done() && second.is_done());
        assert_eq!(f.sender.calls.borrow().len(), 1, "the second caller joined");
        assert!(!f.queue.get_state().flushing);
        assert_eq!(heard.borrow().first(), Some(&(1, true)));
        assert_eq!(heard.borrow().last(), Some(&(0, false)));
    }

    #[test]
    fn clearing_forgets_the_queue_and_the_rejections() {
        let f = fixture();
        f.signed_in.set(false);
        f.enqueue(rename("a"));
        let q = f.queue.clone();
        f.run(async move { q.clear().await });
        assert_eq!(f.queue.get_state(), MutationQueueState::default());
        let reopened = open_on(&f.pool, &f.sender, &f.store, &f.signed_in);
        assert_eq!(reopened.get_state().pending, 0);
    }

    #[test]
    fn a_corrupt_saved_queue_is_empty_and_an_unreadable_entry_is_dropped_alone() {
        let f = fixture();
        for raw in ["not json", "{}", ""] {
            f.run({ let s = f.store.clone(); async move { s.put_setting("serverMode.mutationQueue", raw).await.unwrap() } });
            assert_eq!(open_on(&f.pool, &f.sender, &f.store, &f.signed_in).get_state().pending, 0, "{raw}");
        }
        let mixed = r#"[{"type":"playlist.delete","opId":"x","id":"p"},{"type":"nonsense"}]"#;
        f.run({ let s = f.store.clone(); async move { s.put_setting("serverMode.mutationQueue", mixed).await.unwrap() } });
        assert_eq!(open_on(&f.pool, &f.sender, &f.store, &f.signed_in).get_state().pending, 1);
    }
}
