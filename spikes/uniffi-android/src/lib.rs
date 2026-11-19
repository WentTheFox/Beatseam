//! Phase 0 spike, item 5 of docs/rust-gpui-rewrite-plan.md: the binding shape a Compose UI would use over a Rust
//! view model, if Android ends up on Compose instead of GPUI.
//!
//! The shape under test: the UI holds one `NowPlayingViewModel` object, reads an immutable `NowPlayingState`
//! snapshot, calls action methods, and gets every state change pushed through a callback interface, from whatever
//! thread Rust changed it on. Also covered: errors across the boundary, a `suspend` function, and what a large
//! list costs to cross.

use std::sync::{Arc, Mutex};
use std::time::Duration;

uniffi::setup_scaffolding!();

#[derive(Clone, uniffi::Record)]
pub struct Row {
    pub id: u32,
    pub title: String,
    pub artist: String,
    pub duration: String,
    pub current: bool,
}

#[derive(Clone, uniffi::Record)]
pub struct NowPlayingState {
    pub title: String,
    pub artist: String,
    pub playing: bool,
    /// 0.0..=1.0
    pub progress: f32,
    pub elapsed: String,
    pub rows: Vec<Row>,
    /// Bumped on every change, so the UI side can tell snapshots apart.
    pub revision: u64,
}

#[derive(Debug, uniffi::Error)]
pub enum VmError {
    NoSuchTrack { id: u32 },
}

impl std::fmt::Display for VmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmError::NoSuchTrack { id } => write!(f, "no track with id {id}"),
        }
    }
}

/// Implemented in Kotlin; called from Rust threads.
#[uniffi::export(callback_interface)]
pub trait StateListener: Send + Sync {
    fn on_state(&self, state: NowPlayingState);
}

struct Track {
    title: String,
    artist: String,
    secs: u32,
}

struct Inner {
    tracks: Vec<Track>,
    current: u32,
    playing: bool,
    position_ms: u64,
    revision: u64,
    listener: Option<Arc<dyn StateListener>>,
}

impl Inner {
    fn snapshot(&self) -> NowPlayingState {
        let t = &self.tracks[self.current as usize];
        let elapsed = self.position_ms / 1000;
        NowPlayingState {
            title: t.title.clone(),
            artist: t.artist.clone(),
            playing: self.playing,
            progress: (self.position_ms as f32 / 1000.0 / t.secs as f32).min(1.0),
            elapsed: format!("{}:{:02}", elapsed / 60, elapsed % 60),
            rows: self.rows(0, 200),
            revision: self.revision,
        }
    }

    fn rows(&self, offset: u32, limit: u32) -> Vec<Row> {
        self.tracks
            .iter()
            .enumerate()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|(i, t)| Row {
                id: i as u32,
                title: t.title.clone(),
                artist: t.artist.clone(),
                duration: format!("{}:{:02}", t.secs / 60, t.secs % 60),
                current: i as u32 == self.current,
            })
            .collect()
    }
}

#[derive(uniffi::Object)]
pub struct NowPlayingViewModel {
    inner: Arc<Mutex<Inner>>,
}

/// Push the current snapshot to the listener *outside* the lock, so a listener that calls back into the view
/// model cannot deadlock.
fn notify(inner: &Arc<Mutex<Inner>>) {
    let (listener, state) = {
        let mut g = inner.lock().unwrap();
        g.revision += 1;
        (g.listener.clone(), g.snapshot())
    };
    if let Some(l) = listener {
        l.on_state(state);
    }
}

#[uniffi::export]
impl NowPlayingViewModel {
    #[uniffi::constructor]
    pub fn new(library_size: u32) -> Arc<Self> {
        let tracks = (0..library_size.max(1))
            .map(|i| Track { title: format!("Track {i}"), artist: format!("Artist {}", i % 40), secs: 150 + i % 200 })
            .collect();
        let inner = Arc::new(Mutex::new(Inner { tracks, current: 0, playing: false, position_ms: 0, revision: 0, listener: None }));

        // A Rust-side clock: advances the position while playing and pushes state from this thread.
        let weak = Arc::downgrade(&inner);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(250));
                let Some(inner) = weak.upgrade() else { return };
                let advanced = {
                    let mut g = inner.lock().unwrap();
                    if g.playing {
                        g.position_ms += 250;
                        let secs = g.tracks[g.current as usize].secs as u64 * 1000;
                        if g.position_ms >= secs {
                            g.position_ms = 0;
                            g.current = (g.current + 1) % g.tracks.len() as u32;
                        }
                    }
                    g.playing
                };
                if advanced {
                    notify(&inner);
                }
            }
        });
        Arc::new(Self { inner })
    }

    /// Registers the listener and immediately delivers the current state, so the UI never starts empty.
    pub fn set_listener(&self, listener: Box<dyn StateListener>) {
        self.inner.lock().unwrap().listener = Some(Arc::from(listener));
        notify(&self.inner);
    }

    pub fn state(&self) -> NowPlayingState {
        self.inner.lock().unwrap().snapshot()
    }

    pub fn play_pause(&self) {
        {
            let mut g = self.inner.lock().unwrap();
            g.playing = !g.playing;
        }
        notify(&self.inner);
    }

    pub fn select(&self, id: u32) -> Result<(), VmError> {
        {
            let mut g = self.inner.lock().unwrap();
            if id as usize >= g.tracks.len() {
                return Err(VmError::NoSuchTrack { id });
            }
            g.current = id;
            g.position_ms = 0;
            g.playing = true;
        }
        notify(&self.inner);
        Ok(())
    }

    pub fn seek(&self, progress: f32) {
        {
            let mut g = self.inner.lock().unwrap();
            let secs = g.tracks[g.current as usize].secs as f32;
            g.position_ms = (progress.clamp(0.0, 1.0) * secs * 1000.0) as u64;
        }
        notify(&self.inner);
    }

    /// A page of the library, for lists too large to ship inside every state snapshot.
    pub fn page(&self, offset: u32, limit: u32) -> Vec<Row> {
        self.inner.lock().unwrap().rows(offset, limit)
    }

    pub fn library_size(&self) -> u32 {
        self.inner.lock().unwrap().tracks.len() as u32
    }

    /// Does nothing; exists to time the cost of one call across the boundary.
    pub fn ping(&self) -> u32 {
        1
    }

    /// A `suspend fun` on the Kotlin side: pretend to scan, return how many tracks were found.
    pub async fn rescan(&self) -> u32 {
        // No runtime needed: a plain future that completes on a poll after a short wait.
        async_wait(Duration::from_millis(300)).await;
        self.library_size()
    }
}

/// Minimal timer future without an async runtime (a helper thread wakes the task).
async fn async_wait(d: Duration) {
    use std::future::poll_fn;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Poll;

    let done = Arc::new(AtomicBool::new(false));
    let waker_slot: Arc<Mutex<Option<std::task::Waker>>> = Arc::new(Mutex::new(None));
    let (d2, w2) = (done.clone(), waker_slot.clone());
    std::thread::spawn(move || {
        std::thread::sleep(d);
        d2.store(true, Ordering::SeqCst);
        if let Some(w) = w2.lock().unwrap().take() {
            w.wake();
        }
    });
    poll_fn(move |cx| {
        if done.load(Ordering::SeqCst) {
            Poll::Ready(())
        } else {
            *waker_slot.lock().unwrap() = Some(cx.waker().clone());
            Poll::Pending
        }
    })
    .await
}
