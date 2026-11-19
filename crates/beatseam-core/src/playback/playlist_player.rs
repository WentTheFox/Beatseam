use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::sync::Arc;

use futures_util::future::{join, join3};

use super::preload_scheduler::{PreloadScheduler, PreloadSchedulerOptions, PreloadTickInfo};
use super::shuffle::fisher_yates_shuffle;
use super::track_player::{TrackPlayer, TrackPlayerCallbacks, TrackPlayerState, TrackPlayerStatus};
use crate::audio::DecodedAudio;
use crate::audio_engine::AudioEngine;
use crate::crossfade::{TransitionPlan, compute_transition_plan};
use crate::file_access::FileRef;
use crate::library_store::LoopMode;
use crate::tasks::{Gate, Runtime};

/// How many tracks ahead of the current one to keep decoded and ready - the currently playing track plus this many preloaded
/// is "2-3 songs" resident at once.
const PRELOAD_DEPTH: usize = 2;
/// Default crossfade duration (seconds) if the caller doesn't specify one.
const DEFAULT_CROSSFADE_SECONDS: f64 = 5.0;
/// How far ahead of the fixed fade-start point to start evaluating whether a crossfade can begin (taking the preloaded
/// buffer, calling into the engine) - gives that work a moment to land before the precise moment the transition should
/// audibly start, without meaningfully changing the transition's position within the track (it's re-derived from the plan
/// itself, not from when this check happened to run).
const CROSSFADE_LEAD_SECONDS: f64 = 1.0;
/// Duration (seconds) of the short crossfade a manual next/previous does into the target track, in place of an abrupt hard cut -
/// deliberately much shorter than the natural end-of-track crossfade, which is tuned for blending two tracks' outros/intros
/// together, not for a "just get me to the next song" skip.
const MANUAL_SKIP_CROSSFADE_SECONDS: f64 = 1.0;

type LocalFuture<T> = Pin<Box<dyn Future<Output = T>>>;

/// Looks up a track by file id (usually from the library store).
pub type ResolveTrack = Rc<dyn Fn(&str) -> LocalFuture<Result<FileRef, String>>>;
/// A track's normalization gain (usually from its stored analysis).
pub type ResolveGain = Rc<dyn Fn(&str) -> LocalFuture<Result<f64, String>>>;
/// Runs when a track is freshly decoded (see [`PlaylistPlayerOptions::on_decoded`]).
pub type OnDecoded = Rc<dyn Fn(FileRef, Arc<DecodedAudio>) -> LocalFuture<()>>;
/// `file_id`, when present, is the track the error actually happened for (a decode/read failure) - absent for errors that
/// aren't about any one specific track (e.g. a scheduling conflict).
pub type OnError = Rc<dyn Fn(&str, Option<&str>)>;

#[derive(Default)]
pub struct PlaylistPlayerOptions {
    /// Lets a caller track "this file couldn't be played" per track, e.g. to show a missing-file indicator in a track list.
    pub on_error: Option<OnError>,
    /// Normalization gain for a track - defaults to 1 (no change) if omitted or it fails.
    pub resolve_gain: Option<ResolveGain>,
    /// Crossfade duration (seconds) - the same value used for the transition plan, so what's scheduled matches what any
    /// preview UI shows. Defaults to 5.
    pub crossfade_seconds: Option<f64>,
    /// Run (in the background - never awaited, never blocks playback) whenever a track is freshly decoded, whether for
    /// immediate playback or preload lookahead. Lets a caller lazily analyze+cache a track the first time it's actually needed
    /// instead of an eager batch pass over the whole library - that pass turned out to starve the UI for as long as it ran, for
    /// tracks that might never even get played.
    pub on_decoded: Option<OnDecoded>,
    /// Fired right when playback position changes outside a manual UI action (a crossfade completing, or a track ending
    /// naturally and auto-advancing) - callers should refresh their own state immediately rather than relying solely on a
    /// periodic poll, which can otherwise leave a "now playing" display showing the outgoing track for as long as it takes the
    /// next poll tick to catch up.
    pub on_advance: Option<Rc<dyn Fn()>>,
    /// Random numbers in `[0, 1)` for shuffling (defaults to a small built-in generator seeded from the clock).
    pub random: Option<Rc<dyn Fn() -> f64>>,
}

#[derive(Clone, Debug, Default)]
pub struct PlaylistOptions {
    /// The source playlist's record id, remembered for [`PlaylistPlayer::reconcile_playlist`] to compare against later - omit
    /// it for anything that isn't really "opening a playlist".
    pub playlist_id: Option<String>,
    /// The persisted shuffle order (see [`PlaylistPlayer::get_shuffle_order`]) from last session, used instead of generating a
    /// brand-new random order when shuffle is enabled - without it, every relaunch would silently re-shuffle even though the
    /// person never touched the shuffle toggle. Any file id no longer in the playlist is dropped; any not in the restored
    /// order (added since) is appended in a fresh shuffled tail.
    pub shuffle_order: Option<Vec<String>>,
}

/// The file ids actually crossfading right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrossfadeFileIds {
    pub outgoing: String,
    pub incoming: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlaylistPlayerState {
    pub total_tracks: usize,
    /// Index into playback order (post-shuffle); -1 if nothing is loaded.
    pub position: i64,
    pub current_file_id: Option<String>,
    pub loop_mode: LoopMode,
    pub shuffle_enabled: bool,
    pub track: TrackPlayerState,
    /// The file ids actually crossfading right now (present exactly when `track.pending_incoming` is) - explicit rather than
    /// inferred from the current/next ids, since those two mean different things depending on which triggered the crossfade:
    /// the natural end-of-track path only advances `position` once the transition completes (the current id is still the
    /// outgoing track, the next id the incoming one throughout), while a manual skip advances `position` to the target
    /// immediately for a snappy UI, so the current id is already the incoming track and "next" means whatever comes after
    /// *that*. A caller that needs to label "outgoing"/"incoming" correctly regardless of which triggered the crossfade should
    /// use this instead of either of those.
    pub pending_crossfade_file_ids: Option<CrossfadeFileIds>,
    /// True while `track.status` is `Loading` AND that decode is actually heading toward playback (a person tapping a track,
    /// a manual/natural next/previous) - false for a [`load_playlist`](PlaylistPlayer::load_playlist) decode (e.g. the
    /// on-launch restore, which deliberately leaves the track paused). A UI showing a per-track loading spinner (as opposed
    /// to a play/pause glyph) should gate on this, not just the status - otherwise the restored track shows a spinner on
    /// launch even though nothing is about to play yet.
    pub is_loading_for_playback: bool,
}

/// One decode in progress, which later callers for the same file join.
struct DecodeFlight {
    done: Gate,
    result: RefCell<Option<Result<Arc<DecodedAudio>, String>>>,
}

struct State {
    track_file_ids: Vec<String>,
    /// The source playlist the currently loaded track ids came from - see `reconcile_playlist`.
    current_playlist_id: Option<String>,
    order: Vec<usize>,
    position: i64,
    loop_mode: LoopMode,
    shuffle_enabled: bool,
    play_token: u64,
    loading_for_playback: bool,
    crossfade_seconds: f64,
    /// The playback position a crossfade has already been triggered (or ruled out as impossible) for - guards against
    /// re-triggering every ~200 ms tick for the remainder of the same track.
    crossfade_triggered_for_position: Option<i64>,
    /// True while the crossfade attempt's lookups are in flight - guards against a second tick re-entering and double-triggering
    /// before the first attempt has committed or bailed.
    crossfade_in_flight: bool,
    /// True when the crossfade currently pending was started by a manual next/previous, not the natural end-of-track one:
    /// the completion handler needs to know which, since a manual skip already advances `position` to the target up front
    /// (so preload lookahead/UI reflect the skip immediately), whereas the natural path hasn't touched it yet.
    crossfade_is_manual_skip: bool,
    pending_crossfade_file_ids: Option<CrossfadeFileIds>,
    decoding: HashMap<String, Rc<DecodeFlight>>,
    /// Caches the gain lookup per file id, warmed in the background after a decode rather than only ever being looked up live at
    /// the moment a track actually starts. On a device where every database query is a full serialized transaction, a live
    /// lookup made right as a manual skip needs it can queue for *seconds* behind whatever concurrent analysis-write
    /// transactions the preload scheduler's own decodes are triggering (measured: ~7 s in exactly this situation). Warming this
    /// during preload moves that queueing delay off the interactive path entirely.
    gain_cache: HashMap<String, f64>,
    /// Open once the real persisted volume is known - see [`PlaylistPlayer::await_initial_volume`].
    initial_volume_ready: Gate,
}

struct Inner<E: AudioEngine + 'static, R: Runtime + 'static> {
    engine: Rc<E>,
    runtime: Arc<R>,
    resolve_track: ResolveTrack,
    resolve_gain: Option<ResolveGain>,
    on_decoded: Option<OnDecoded>,
    on_error: Option<OnError>,
    on_advance: Option<Rc<dyn Fn()>>,
    random: Rc<dyn Fn() -> f64>,
    track_player: TrackPlayer<E>,
    preload: PreloadScheduler<R>,
    state: RefCell<State>,
}

/// Sequences a playlist's tracks through a single [`TrackPlayer`]: play order (sequential or shuffled), loop mode, next/prev,
/// the natural end-of-track crossfade and the preload lookahead.
///
/// Manual next/previous respect loop mode: `All` wraps at the playlist boundary and `Off` just clamps at the boundary. `One`
/// only affects natural end-of-track (the same track restarts instead of advancing) and previous (which still restarts the
/// current track rather than changing tracks, since there's no natural "previous" while repeating one song) - a manual next
/// press always advances to the actual next track regardless of loop mode, since "repeat this song" shouldn't require an
/// extra tap just to move on from it. Pass `force: true` to always move to the literal next/previous track regardless of loop
/// mode (wrapping at the boundary too) - the UI's double-tap gesture uses this to escape the one-track loop or the clamp.
pub struct PlaylistPlayer<E: AudioEngine + 'static, R: Runtime + 'static> {
    inner: Rc<Inner<E, R>>,
}

impl<E: AudioEngine + 'static, R: Runtime + 'static> Clone for PlaylistPlayer<E, R> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

struct FlightEnd<'a> {
    state: &'a RefCell<State>,
    file_id: String,
    flight: Rc<DecodeFlight>,
}

impl Drop for FlightEnd<'_> {
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        if state.decoding.get(&self.file_id).is_some_and(|f| Rc::ptr_eq(f, &self.flight)) {
            state.decoding.remove(&self.file_id);
        }
        drop(state);
        self.flight.done.open();
    }
}

fn default_random() -> Rc<dyn Fn() -> f64> {
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0x9e37_79b9_7f4a_7c15, |d| d.as_nanos() as u64);
    let state = std::cell::Cell::new(seed | 1);
    Rc::new(move || {
        // xorshift64*
        let mut x = state.get();
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        state.set(x);
        (x.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
    })
}

fn identity_order(len: usize) -> Vec<usize> {
    (0..len).collect()
}

impl<E: AudioEngine + 'static, R: Runtime + 'static> PlaylistPlayer<E, R> {
    pub fn new(engine: Rc<E>, runtime: Arc<R>, resolve_track: ResolveTrack, options: PlaylistPlayerOptions) -> Self {
        let inner = Rc::new_cyclic(|weak: &Weak<Inner<E, R>>| {
            let (ended, completed) = (weak.clone(), weak.clone());
            let (rt_ended, rt_completed) = (runtime.clone(), runtime.clone());
            let track_player = TrackPlayer::new(
                engine.clone(),
                TrackPlayerCallbacks {
                    on_ended: Some(Rc::new(move || {
                        if let Some(inner) = ended.upgrade() {
                            let player = PlaylistPlayer { inner };
                            rt_ended.spawn(Box::pin(async move { player.handle_track_ended().await }));
                        }
                    })),
                    on_crossfade_completed: Some(Rc::new(move || {
                        let _ = &rt_completed;
                        if let Some(inner) = completed.upgrade() {
                            PlaylistPlayer { inner }.handle_crossfade_completed();
                        }
                    })),
                },
            );
            let decode_weak = weak.clone();
            let give_up_weak = weak.clone();
            let mut preload_options = PreloadSchedulerOptions::new(Arc::new(move |file_id: &str| {
                let weak = decode_weak.clone();
                let file_id = file_id.to_owned();
                Box::pin(async move {
                    match weak.upgrade() {
                        Some(inner) => PlaylistPlayer { inner }.decode_deduped(&file_id).await,
                        None => Err("the player is gone".to_owned()),
                    }
                })
            }));
            // Playback itself is unaffected: starting that track just decodes cold (its existing fallback) when it gets there,
            // since an empty preload cache is a no-op, not a special case.
            preload_options.on_give_up = Some(Arc::new(move |file_id: &str| {
                if let Some(inner) = give_up_weak.upgrade() {
                    inner.report(&format!("Preload failed for track {file_id} after every retry - it'll load normally when reached."), None);
                }
            }));
            let state = State {
                track_file_ids: Vec::new(),
                current_playlist_id: None,
                order: Vec::new(),
                position: -1,
                loop_mode: LoopMode::Off,
                shuffle_enabled: false,
                play_token: 0,
                loading_for_playback: false,
                crossfade_seconds: options.crossfade_seconds.unwrap_or(DEFAULT_CROSSFADE_SECONDS),
                crossfade_triggered_for_position: None,
                crossfade_in_flight: false,
                crossfade_is_manual_skip: false,
                pending_crossfade_file_ids: None,
                decoding: HashMap::new(),
                gain_cache: HashMap::new(),
                initial_volume_ready: {
                    let open = Gate::new();
                    open.open();
                    open
                },
            };
            Inner {
                engine: engine.clone(),
                runtime: runtime.clone(),
                resolve_track,
                resolve_gain: options.resolve_gain,
                on_decoded: options.on_decoded,
                on_error: options.on_error,
                on_advance: options.on_advance,
                random: options.random.unwrap_or_else(default_random),
                track_player,
                preload: PreloadScheduler::new(runtime.clone(), preload_options),
                state: RefCell::new(state),
            }
        });
        Self { inner }
    }

    fn state(&self) -> std::cell::Ref<'_, State> {
        self.inner.state.borrow()
    }

    fn state_mut(&self) -> std::cell::RefMut<'_, State> {
        self.inner.state.borrow_mut()
    }

    fn shuffle(&self, indices: &[usize]) -> Vec<usize> {
        let random = self.inner.random.clone();
        fisher_yates_shuffle(indices, &mut || random())
    }

    fn spawn(&self, future: impl Future<Output = ()> + 'static) {
        self.inner.runtime.spawn(Box::pin(future));
    }

    // ---------------------------------------------------------------- decoding

    /// Decodes a track and runs `on_decoded` - the single decode path shared by playing's cache-miss case and the preload
    /// scheduler, so both feed the same just-in-time analysis hook.
    async fn decode_and_notify(&self, file_id: &str) -> Result<Arc<DecodedAudio>, String> {
        let file = (self.inner.resolve_track)(file_id).await?;
        let decoded = self.inner.engine.decode_file(&file).await.map_err(|e| e.to_string())?;
        // Does the engine's otherwise-lazy native-buffer setup now. For the preload scheduler (this decode finishing well ahead
        // of when the track is actually needed), that moves a real synchronous stall off the track-switch/crossfade moment; for
        // a cache-miss cold decode it's no worse (creating the source would do the identical work moments later regardless).
        self.inner.engine.prepare_buffer(&decoded);
        // Warms the gain cache in the background too - see its doc for why this (not just the audio buffer) needs to happen
        // ahead of time as well. Nothing here needs to block returning the decoded audio.
        let (this, id) = (self.clone(), file_id.to_owned());
        self.spawn(async move {
            this.resolve_gain_for(&id).await;
        });
        // Never awaited by the caller (the decoded audio is returned for playback immediately regardless), but an engine whose
        // decode can resolve before the samples are real (Windows) needs this to wait for them before `on_decoded` reads the
        // data for analysis.
        if let Some(on_decoded) = self.inner.on_decoded.clone() {
            let (engine, audio) = (self.inner.engine.clone(), decoded.clone());
            self.spawn(async move {
                engine.await_analysis_ready(&audio).await;
                on_decoded(file, audio).await;
            });
        }
        Ok(decoded)
    }

    /// Same as [`decode_and_notify`](Self::decode_and_notify), but de-duplicated across every caller (the preload scheduler,
    /// playing's cache-miss fallback, and a manual skip's) - without this, the preload scheduler could already be mid-decode for
    /// a file (e.g. it's next up in the lookahead window) at the exact moment a manual skip or natural advance reaches that
    /// same file before the preload finishes; nothing is ready yet, and a second decode of the identical file starts
    /// concurrently with the first. On Windows the native decode opens the file itself (not a shared handle), so two
    /// overlapping decodes of the same file threw a real "the file is in use" error. Every caller now shares whatever decode
    /// for that file is already in flight instead.
    async fn decode_deduped(&self, file_id: &str) -> Result<Arc<DecodedAudio>, String> {
        let existing = self.state().decoding.get(file_id).cloned();
        if let Some(flight) = existing {
            flight.done.wait().await;
            return flight.result.borrow().clone().unwrap_or_else(|| Err("the decode was abandoned".into()));
        }
        let flight = Rc::new(DecodeFlight { done: Gate::new(), result: RefCell::new(None) });
        self.state_mut().decoding.insert(file_id.to_owned(), flight.clone());
        let _end = FlightEnd { state: &self.inner.state, file_id: file_id.to_owned(), flight: flight.clone() };
        let result = self.decode_and_notify(file_id).await;
        *flight.result.borrow_mut() = Some(result.clone());
        result
    }

    /// Falls back to 1 (no change) if no gain lookup was given, or it fails - a missing/failed lookup shouldn't block playback.
    /// Cached per file id - a lookup that already ran once (typically warmed ahead of time) never re-queries.
    async fn resolve_gain_for(&self, file_id: &str) -> f64 {
        if let Some(cached) = self.state().gain_cache.get(file_id) {
            return *cached;
        }
        let Some(resolve) = self.inner.resolve_gain.clone() else { return 1.0 };
        match resolve(file_id).await {
            Ok(gain) => {
                self.state_mut().gain_cache.insert(file_id.to_owned(), gain);
                gain
            }
            Err(_) => 1.0,
        }
    }

    // ---------------------------------------------------------------- loading a playlist

    /// Loads a new playlist and starts playing at the given track (default: the first).
    pub async fn set_playlist(&self, track_file_ids: Vec<String>, start_file_id: Option<&str>, options: PlaylistOptions) {
        self.load_playlist_at(track_file_ids, start_file_id, true, options).await;
    }

    /// Same as [`set_playlist`](Self::set_playlist), but leaves the track loaded-and-paused instead of starting playback - for
    /// restoring a saved position on launch, where autoplaying audio before the UI has rendered any controls would be a
    /// surprise. (Browsers' autoplay policy silently no-ops on web until a real user gesture; native platforms have no such
    /// protection - audio would genuinely start playing.)
    pub async fn load_playlist(&self, track_file_ids: Vec<String>, start_file_id: Option<&str>, options: PlaylistOptions) {
        self.load_playlist_at(track_file_ids, start_file_id, false, options).await;
    }

    /// The source playlist id passed to the most recent set/load call, or `None` if none was given.
    pub fn get_current_playlist_id(&self) -> Option<String> {
        self.state().current_playlist_id.clone()
    }

    async fn load_playlist_at(&self, track_file_ids: Vec<String>, start_file_id: Option<&str>, autoplay: bool, options: PlaylistOptions) {
        let start_position = {
            let pinned = start_file_id.and_then(|id| track_file_ids.iter().position(|t| t == id));
            let (shuffle_enabled, len) = (self.state().shuffle_enabled, track_file_ids.len());
            let order = if !shuffle_enabled {
                identity_order(len)
            } else {
                match options.shuffle_order.as_deref().filter(|o| !o.is_empty()) {
                    Some(restored) => self.build_order_from_restored_shuffle(&track_file_ids, restored, pinned),
                    None => self.build_shuffled_order(len, pinned),
                }
            };
            let start = pinned.and_then(|p| order.iter().position(|o| *o == p)).unwrap_or(0);
            let mut state = self.state_mut();
            state.track_file_ids = track_file_ids;
            state.current_playlist_id = options.playlist_id;
            state.order = order;
            start
        };
        self.play_at(start_position as i64, autoplay).await;
    }

    /// A fresh Fisher-Yates order over every track - `pinned`, when given, is placed first (the "current song goes to the top"
    /// rule) with the rest shuffled below it, rather than landing wherever the shuffle happens to put it.
    fn build_shuffled_order(&self, len: usize, pinned: Option<usize>) -> Vec<usize> {
        let indices = identity_order(len);
        let Some(pinned) = pinned else { return self.shuffle(&indices) };
        let rest: Vec<usize> = indices.into_iter().filter(|i| *i != pinned).collect();
        let mut order = vec![pinned];
        order.extend(self.shuffle(&rest));
        order
    }

    /// Maps a persisted shuffle order (file ids) back onto index space against the current track ids - see
    /// [`PlaylistOptions::shuffle_order`] for the add/remove-since-last-session handling.
    fn build_order_from_restored_shuffle(&self, track_file_ids: &[String], order_file_ids: &[String], pinned: Option<usize>) -> Vec<usize> {
        let mut index_by_file_id: HashMap<&str, usize> = HashMap::new();
        for (i, id) in track_file_ids.iter().enumerate() {
            index_by_file_id.insert(id.as_str(), i); // the last of a duplicated id wins, like a JS Map
        }
        let mut seen = vec![false; track_file_ids.len()];
        let mut restored = Vec::new();
        for file_id in order_file_ids {
            if let Some(&index) = index_by_file_id.get(file_id.as_str())
                && !seen[index] {
                    seen[index] = true;
                    restored.push(index);
                }
        }
        let missing: Vec<usize> = (0..track_file_ids.len()).filter(|i| !seen[*i]).collect();
        let mut order = restored;
        order.extend(self.shuffle(&missing));
        if let Some(pinned) = pinned
            && let Some(at) = order.iter().position(|o| *o == pinned).filter(|at| *at > 0) {
                order.remove(at);
                order.insert(0, pinned);
            }
        order
    }

    pub fn set_loop_mode(&self, mode: LoopMode) {
        self.state_mut().loop_mode = mode;
    }

    /// Changes the crossfade duration used for any future transition - takes effect the next time the natural crossfade is
    /// evaluated, not retroactively on one already in flight.
    pub fn set_crossfade_seconds(&self, seconds: f64) {
        self.state_mut().crossfade_seconds = seconds;
    }

    pub fn get_crossfade_seconds(&self) -> f64 {
        self.state().crossfade_seconds
    }

    /// Sets the user-facing master volume [0,1] (e.g. a UI slider) - independent of per-track normalization gain, applies
    /// immediately.
    pub fn set_volume(&self, value: f64) {
        self.inner.track_player.set_volume(value);
    }

    pub fn get_volume(&self) -> f64 {
        self.inner.track_player.get_volume()
    }

    /// Makes starting a track wait for `volume` to resolve (and be applied) before computing or applying any gain for that
    /// call - call this once, as early as possible (before any component has even mounted), with a future that resolves to
    /// the real persisted volume.
    ///
    /// This exists because the master volume otherwise defaults to a fixed constant until something explicitly corrects it -
    /// loading that correction asynchronously, after the fact, left a real window where the first track of a session could
    /// start audible at the wrong volume and audibly jump once the correction landed (confirmed live as a loud/quiet/normal
    /// fluctuation at playback start, worsened when more than one such correction raced). Gating playback itself on this
    /// closes that window for good, for every caller (UI, media-session/lock-screen controls, anything) rather than relying
    /// on no caller ever running before some other, separately-timed fetch resolves. Only the very first start actually waits
    /// on anything real: once the gate opens, every later start proceeds with no added delay.
    pub fn await_initial_volume(&self, volume: impl Future<Output = f64> + 'static) {
        let gate = Gate::new();
        self.state_mut().initial_volume_ready = gate.clone();
        let this = self.clone();
        self.spawn(async move {
            let volume = volume.await;
            this.set_volume(volume);
            gate.open();
        });
    }

    /// Re-shuffles (or restores original order) without interrupting the currently playing track. Enabling shuffle pins the
    /// currently playing track at the top of the new order (position 0) with the rest shuffled below it, rather than letting
    /// it land anywhere at random. Disabling restores the original playlist order, keeping the current track's position
    /// intact.
    pub fn set_shuffle(&self, enabled: bool) {
        let (len, current) = {
            let state = self.state();
            if state.shuffle_enabled == enabled || state.track_file_ids.is_empty() {
                drop(state);
                self.state_mut().shuffle_enabled = enabled;
                return;
            }
            (state.track_file_ids.len(), usize::try_from(state.position).ok().and_then(|p| state.order.get(p).copied()))
        };
        let order = if enabled { self.build_shuffled_order(len, current) } else { identity_order(len) };
        let mut state = self.state_mut();
        state.shuffle_enabled = enabled;
        if let Some(current) = current {
            state.position = order.iter().position(|o| *o == current).map_or(0, |p| p as i64);
        }
        state.order = order;
    }

    /// The current shuffle order as file ids (nearest-first from position 0), for persisting across reloads - see
    /// [`PlaylistOptions::shuffle_order`]. `None` when shuffle is off, since sequential order needs no persisting.
    pub fn get_shuffle_order(&self) -> Option<Vec<String>> {
        let state = self.state();
        state.shuffle_enabled.then(|| state.order.iter().filter_map(|i| state.track_file_ids.get(*i).cloned()).collect())
    }

    /// Call this when the source playlist this instance is currently playing from - see
    /// [`get_current_playlist_id`](Self::get_current_playlist_id) - has been rescanned and may have gained or lost tracks, so a
    /// caller should check that id matches the rescanned playlist's before bothering to call this at all. Diffs the new ids
    /// against what's currently loaded rather than doing a fresh load (which would restart shuffle/position bookkeeping from
    /// scratch): removed tracks drop out of the running order; added tracks are appended in their playlist position when not
    /// shuffling, or spliced into a random position among the existing order when shuffling ("shuffled in", not necessarily
    /// played next) - existing tracks' relative order is otherwise left untouched, so an in-progress shuffle or a listener's
    /// spot in the sequential order survives a rescan instead of jumping around.
    ///
    /// A no-op if nothing is loaded yet. If the currently playing track was itself removed from the playlist, it's kept in the
    /// running order anyway (appended at the end) rather than cutting the audio out from under the listener or leaving the
    /// position pointing at nothing - it naturally drops out on the next reconcile once a different track becomes current.
    ///
    /// Also a no-op while a crossfade is actually in flight at the audio engine level (the track player's own pending
    /// incoming, not this class's own pending ids - that field is set slightly earlier, before the engine call, and can be left
    /// stale if the call fails without ever starting a real transition): the completion handler advances the position by
    /// simple arithmetic once the transition finishes, trusting that position/order still mean what they did when the
    /// crossfade was scheduled - rewriting them out from under it here would advance to the wrong track, or wrongly
    /// wrap/not-wrap, once it completes. Crossfades are short-lived (a few seconds); the next refresh picks up the reconcile
    /// once it settles.
    pub fn reconcile_playlist(&self, new_track_file_ids: Vec<String>) {
        if self.state().track_file_ids.is_empty() || self.inner.track_player.get_state().pending_incoming.is_some() {
            return;
        }
        let (old_ids, old_order, position, shuffle_enabled) = {
            let state = self.state();
            (state.track_file_ids.clone(), state.order.clone(), state.position, state.shuffle_enabled)
        };
        let current_file_id: Option<String> = usize::try_from(position).ok().and_then(|p| old_order.get(p)).and_then(|i| old_ids.get(*i)).cloned();

        let new_set: std::collections::HashSet<&str> = new_track_file_ids.iter().map(String::as_str).collect();
        let old_set: std::collections::HashSet<&str> = old_ids.iter().map(String::as_str).collect();
        let surviving: Vec<String> = old_order.iter().filter_map(|i| old_ids.get(*i)).filter(|id| new_set.contains(id.as_str())).cloned().collect();
        let added: Vec<String> = new_track_file_ids.iter().filter(|id| !old_set.contains(id.as_str())).cloned().collect();
        let keep_current_alive = current_file_id.as_ref().is_some_and(|c| !new_set.contains(c.as_str()));

        let mut effective = new_track_file_ids.clone();
        if let (true, Some(current)) = (keep_current_alive, &current_file_id) {
            effective.push(current.clone());
        }

        let file_id_order: Vec<String> = if !shuffle_enabled {
            effective.clone()
        } else {
            let mut order = surviving;
            if let (true, Some(current)) = (keep_current_alive, &current_file_id)
                && !order.contains(current) {
                    order.push(current.clone());
                }
            for file_id in added {
                let insert_at = ((self.inner.random)() * (order.len() + 1) as f64).floor() as usize;
                order.insert(insert_at.min(order.len()), file_id);
            }
            order
        };

        let order: Vec<usize> = file_id_order.iter().map(|id| effective.iter().position(|e| e == id).unwrap_or(usize::MAX)).collect();
        let mut state = self.state_mut();
        if let Some(current) = &current_file_id {
            state.position = order.iter().position(|i| effective.get(*i) == Some(current)).map_or(0, |p| p as i64);
        }
        state.track_file_ids = effective;
        state.order = order;
        // The position is a plain array index, not a stable id - it can come out pointing at the same track under a different
        // number than before (an insertion ahead of it, a reshuffle of survivors' indices). A stale triggered-for position could
        // then either wrongly suppress a crossfade evaluation that's never actually run for this "new" position, or wrongly
        // match and skip one that's now due - clearing it forces the next tick to decide fresh.
        state.crossfade_triggered_for_position = None;
    }

    /// Once shuffle + loop-all reach the last track in the current order, this immediately regenerates the shuffle for the next
    /// lap - re-labeled as position 0 of the new order with the just-reached last track pinned at its front - rather than
    /// waiting until that track actually ends. Doing this at entry (before any crossfade lookahead has decided what plays
    /// next), instead of at end-time, means the ordinary position+1/upcoming/preload machinery just treats the rest of the loop
    /// as a normal continuation - no special-casing needed anywhere else for the wrap-around, and no risk of the crossfade
    /// having already committed to a track picked from the pre-reshuffle order.
    fn maybe_reshuffle_for_loop_continuation(&self) {
        let (last, len) = {
            let state = self.state();
            if !state.shuffle_enabled || state.loop_mode != LoopMode::All || state.track_file_ids.len() <= 1 || state.position != state.order.len() as i64 - 1 {
                return;
            }
            let Some(last) = usize::try_from(state.position).ok().and_then(|p| state.order.get(p).copied()) else { return };
            (last, state.track_file_ids.len())
        };
        let rest: Vec<usize> = (0..len).filter(|i| *i != last).collect();
        let mut order = vec![last];
        order.extend(self.shuffle(&rest));
        let mut state = self.state_mut();
        state.order = order;
        state.position = 0;
    }

    pub fn play(&self) {
        self.inner.track_player.play();
    }

    pub fn pause(&self) {
        self.inner.track_player.pause();
    }

    pub fn seek(&self, position_seconds: f64) {
        self.inner.track_player.seek(position_seconds);
    }

    /// Current track's file id, or `None` if nothing is loaded.
    fn current_file_id(&self) -> Option<String> {
        let state = self.state();
        usize::try_from(state.position).ok().and_then(|p| state.order.get(p)).and_then(|i| state.track_file_ids.get(*i)).cloned()
    }

    fn file_id_at(&self, position: i64) -> Option<String> {
        let state = self.state();
        usize::try_from(position).ok().and_then(|p| state.order.get(p)).and_then(|i| state.track_file_ids.get(*i)).cloned()
    }

    pub async fn next(&self, force: bool) {
        let (len, position, loop_mode) = {
            let state = self.state();
            (state.order.len() as i64, state.position, state.loop_mode)
        };
        if len == 0 {
            return;
        }
        // Unlike previous() below, a manual next always advances regardless of the one-track loop - see this class's own doc for
        // why. (The one-track loop still governs natural end-of-track, via `handle_track_ended`.)
        if position >= len - 1 {
            if force || loop_mode == LoopMode::All {
                self.crossfade_to_position(0).await;
            }
            return;
        }
        self.crossfade_to_position(position + 1).await;
    }

    pub async fn previous(&self, force: bool) {
        let (len, position, loop_mode) = {
            let state = self.state();
            (state.order.len() as i64, state.position, state.loop_mode)
        };
        if len == 0 {
            return;
        }
        if !force && loop_mode == LoopMode::One {
            self.inner.track_player.seek(0.0);
            return;
        }
        if position <= 0 {
            if force || loop_mode == LoopMode::All {
                self.crossfade_to_position(len - 1).await;
            }
            return;
        }
        self.crossfade_to_position(position - 1).await;
    }

    /// Drives the lookahead preload scheduler - call this regularly (both apps do it from their existing ~200 ms UI poll
    /// interval, rather than adding a second timer) while a track is playing or paused.
    pub fn check_preload(&self) {
        if self.state().order.is_empty() {
            return;
        }
        let track_state = self.inner.track_player.get_state();
        if !matches!(track_state.status, TrackPlayerStatus::Playing | TrackPlayerStatus::Paused) {
            return;
        }
        self.inner.preload.tick(&PreloadTickInfo { remaining_seconds: track_state.duration_seconds - track_state.position_seconds, upcoming_file_ids: self.compute_upcoming_file_ids(PRELOAD_DEPTH) });
        self.maybe_start_crossfade(track_state);
    }

    /// Once the current track is within the crossfade duration (plus a small lead-in) of ending, and the next track's buffer is
    /// already preloaded, computes a fixed-lead-time volume-only transition plan (no BPM/analysis lookup needed at all this
    /// round) and hands it to the track player so the transition is actually scheduled on the engine - not just previewed. If
    /// the preload isn't ready in time, this simply does nothing and the track proceeds to its existing hard-cut natural end.
    fn maybe_start_crossfade(&self, track_state: TrackPlayerState) {
        let (next_file_id, position, token, crossfade_seconds) = {
            let state = self.state();
            if state.crossfade_in_flight || track_state.status != TrackPlayerStatus::Playing || state.crossfade_triggered_for_position == Some(state.position) {
                return;
            }
            // A manual skip's own short crossfade is already in flight - defer to it rather than potentially racing a second
            // crossfade on top of it (the track player would cancel the manual one for this new one, but by then the position
            // already points past the manual skip's target, so the next id would resolve to the wrong track entirely).
            if track_state.pending_incoming.is_some() && state.crossfade_is_manual_skip {
                return;
            }
            if track_state.duration_seconds - track_state.position_seconds > state.crossfade_seconds + CROSSFADE_LEAD_SECONDS {
                return;
            }
            drop(state);
            let Some(next) = self.get_next_file_id() else { return };
            if !self.inner.preload.has_preloaded(&next) {
                return;
            }
            let mut state = self.state_mut();
            state.crossfade_in_flight = true;
            (next, state.position, state.play_token, state.crossfade_seconds)
        };
        let this = self.clone();
        self.spawn(async move {
            this.start_crossfade(track_state, next_file_id, position, token, crossfade_seconds).await;
            this.state_mut().crossfade_in_flight = false;
        });
    }

    async fn start_crossfade(&self, track_state: TrackPlayerState, next_file_id: String, position: i64, token: u64, crossfade_seconds: f64) {
        let incoming_gain = self.resolve_gain_for(&next_file_id).await;
        // A manual skip/track change landed while the lookup above was in flight - the transition this was computing for no
        // longer applies.
        {
            let state = self.state();
            if token != state.play_token || position != state.position {
                return;
            }
        }
        let Some(decoded) = self.inner.preload.take_preloaded(&next_file_id) else { return }; // taken or expired since the check - rare; falls back to the natural hard-cut end
        let current = self.current_file_id();
        let plan = compute_transition_plan(track_state.duration_seconds, crossfade_seconds);
        if let Some(outgoing) = current {
            self.state_mut().pending_crossfade_file_ids = Some(CrossfadeFileIds { outgoing, incoming: next_file_id.clone() });
        }
        let result = self.inner.track_player.crossfade_to(decoded, &plan, incoming_gain);
        // Mark this position as handled whether the crossfade actually started a transition or declined (e.g. status changed
        // underneath us) - either way, retrying on the very next ~200 ms tick would just re-evaluate the same already-decided
        // outcome. This also covers a crossfade that partially scheduled something before failing - without this, a genuinely
        // repeatable failure would retry every tick, each attempt scheduling another conflicting gain automation on top of
        // whatever the previous attempt already got applied to the outgoing source.
        self.state_mut().crossfade_triggered_for_position = Some(position);
        if let Err(error) = result {
            self.inner.report(&error.to_string(), None);
        }
    }

    /// The transition scheduled by the natural crossfade has completed and the track player has already swapped over to the
    /// incoming track's audio - this just catches this class's own position bookkeeping up to match (mirroring the same advance
    /// a natural end would have made), so the state and preload lookahead reflect what's actually playing now. Nothing is
    /// started here - the audio is already playing. Fires `on_advance` immediately rather than leaving callers to notice on
    /// their next poll.
    fn handle_crossfade_completed(&self) {
        let manual = self.state().crossfade_is_manual_skip;
        // A manual skip already advanced the position to its target up front; only the natural end-of-track path still needs it
        // advanced here.
        if !manual {
            {
                let mut state = self.state_mut();
                let is_last = state.position >= state.order.len() as i64 - 1;
                state.position = if is_last { 0 } else { state.position + 1 };
            }
            self.maybe_reshuffle_for_loop_continuation();
        }
        {
            let mut state = self.state_mut();
            state.crossfade_is_manual_skip = false;
            state.pending_crossfade_file_ids = None;
            state.crossfade_triggered_for_position = None;
        }
        if let Some(on_advance) = &self.inner.on_advance {
            on_advance();
        }
    }

    /// The track that would play next (respecting loop mode/shuffle order), or `None` if there isn't one.
    pub fn get_next_file_id(&self) -> Option<String> {
        self.compute_upcoming_file_ids(1).into_iter().next()
    }

    /// The next `depth` tracks' file ids after the current position, nearest first, respecting loop mode.
    fn compute_upcoming_file_ids(&self, depth: usize) -> Vec<String> {
        let state = self.state();
        if state.loop_mode == LoopMode::One || state.order.len() <= 1 {
            return Vec::new();
        }
        let mut result = Vec::new();
        for i in 1..=depth as i64 {
            let mut position = state.position + i;
            if position >= state.order.len() as i64 {
                if state.loop_mode != LoopMode::All {
                    break; // loop off - nothing past the end
                }
                position %= state.order.len() as i64;
            }
            let Some(file_id) = usize::try_from(position).ok().and_then(|p| state.order.get(p)).and_then(|i| state.track_file_ids.get(*i)) else { break };
            result.push(file_id.clone());
        }
        result
    }

    pub fn get_state(&self) -> PlaylistPlayerState {
        let track = self.inner.track_player.get_state();
        let state = self.state();
        PlaylistPlayerState {
            total_tracks: state.track_file_ids.len(),
            position: state.position,
            current_file_id: usize::try_from(state.position).ok().and_then(|p| state.order.get(p)).and_then(|i| state.track_file_ids.get(*i)).cloned(),
            loop_mode: state.loop_mode,
            shuffle_enabled: state.shuffle_enabled,
            track,
            pending_crossfade_file_ids: state.pending_crossfade_file_ids.clone(),
            is_loading_for_playback: track.status == TrackPlayerStatus::Loading && state.loading_for_playback,
        }
    }

    /// A manual next/previous target - crossfades into the target track over a second instead of a plain hard cut, as long as
    /// something's actually playing to crossfade away from (otherwise there's nothing to blend and a hard cut is already correct
    /// - e.g. the very first track, or skipping while paused/stopped).
    async fn crossfade_to_position(&self, position: i64) {
        let Some(file_id) = self.file_id_at(position) else { return };
        let track_state = self.inner.track_player.get_state();
        if track_state.status != TrackPlayerStatus::Playing {
            self.play_at(position, true).await;
            return;
        }

        // Captured before the position is reassigned below - see `pending_crossfade_file_ids`' doc for why this can't just be
        // re-derived from the current id afterward.
        let outgoing = self.current_file_id();
        self.state_mut().position = position;
        self.maybe_reshuffle_for_loop_continuation();
        let token = {
            let mut state = self.state_mut();
            state.play_token += 1;
            state.play_token
        };
        let preloaded = self.inner.preload.take_preloaded(&file_id);
        let decode = async {
            match preloaded {
                Some(preloaded) => Ok(preloaded),
                None => self.decode_deduped(&file_id).await,
            }
        };
        let (decoded, gain) = join(decode, self.resolve_gain_for(&file_id)).await;
        if token != self.state().play_token {
            return;
        }
        let decoded = match decoded {
            Ok(decoded) => decoded,
            Err(error) => {
                self.inner.report(&error, Some(&file_id));
                return;
            }
        };
        let plan = TransitionPlan { fade_start_seconds: track_state.position_seconds, fade_duration_seconds: MANUAL_SKIP_CROSSFADE_SECONDS, incoming_start_seconds: 0.0 };
        {
            let mut state = self.state_mut();
            state.crossfade_is_manual_skip = true;
            if let Some(outgoing) = outgoing {
                state.pending_crossfade_file_ids = Some(CrossfadeFileIds { outgoing, incoming: file_id.clone() });
            }
        }
        match self.inner.track_player.crossfade_to(decoded.clone(), &plan, gain) {
            Ok(true) => {}
            Ok(false) => {
                // The outgoing track stopped being "playing" while we were decoding (e.g. a pause landed in the meantime) -
                // nothing left to crossfade away from, fall back to the same hard cut a plain play would have done.
                {
                    let mut state = self.state_mut();
                    state.crossfade_is_manual_skip = false;
                    state.pending_crossfade_file_ids = None;
                }
                self.inner.track_player.set_gain(gain);
                self.inner.track_player.load_decoded(decoded);
                self.inner.track_player.play();
            }
            Err(error) => {
                if token == self.state().play_token {
                    self.inner.report(&error.to_string(), Some(&file_id));
                }
            }
        }
    }

    async fn play_at(&self, position: i64, autoplay: bool) {
        let Some(file_id) = self.file_id_at(position) else { return };
        let preloaded = self.inner.preload.take_preloaded(&file_id);
        self.state_mut().position = position;
        self.maybe_reshuffle_for_loop_continuation();
        let (token, initial_volume) = {
            let mut state = self.state_mut();
            state.loading_for_playback = autoplay;
            // If a newer play request (from a rapid manual skip, or a duplicate/spurious ended callback) starts before this one
            // finishes decoding, this call's eventual play must not run - two overlapping loads racing to start playback was the
            // second half of what caused a native crash this guards against (the first half was the track player's own
            // stop-vs-ended ordering bug).
            state.play_token += 1;
            (state.play_token, state.initial_volume_ready.clone())
        };
        self.inner.track_player.mark_loading();
        // A track the preload scheduler already finished decoding skips straight to loading the decoded buffer - no redundant
        // decode round trip, and effectively instant since there's nothing left to await there. The initial-volume gate rides
        // along in the same join rather than a separate serial await before this one - almost always already open by the time
        // any real play request reaches here, so it adds no measurable delay.
        let decode = async {
            match preloaded {
                Some(preloaded) => Ok(preloaded),
                None => self.decode_deduped(&file_id).await,
            }
        };
        let (decoded, gain, ()) = join3(decode, self.resolve_gain_for(&file_id), initial_volume.wait()).await;
        if token != self.state().play_token {
            return;
        }
        match decoded {
            Ok(decoded) => {
                self.inner.track_player.set_gain(gain);
                self.inner.track_player.load_decoded(decoded);
                if autoplay {
                    self.inner.track_player.play();
                }
            }
            Err(error) => {
                // The player was put in `Loading` for the whole decode window - without resetting it back here, a genuine
                // decode failure (missing/unreadable file, corrupt data) left the UI's loading spinner running forever with no
                // way to tell playback had actually given up rather than still being in flight. The error report alone isn't
                // enough for that: it's a one-shot event, while the status is what the UI continuously polls.
                self.inner.track_player.mark_load_failed();
                self.inner.report(&error, Some(&file_id));
            }
        }
    }

    /// Auto-advance on natural end (as opposed to a manual next/previous/set call) - fires `on_advance` once the new track is
    /// actually loaded, same as the crossfade completion, for the same immediate-refresh reason.
    async fn handle_track_ended(&self) {
        let (loop_mode, position, len) = {
            let state = self.state();
            (state.loop_mode, state.position, state.order.len() as i64)
        };
        let target = if loop_mode == LoopMode::One {
            Some(position)
        } else if position >= len - 1 {
            (loop_mode == LoopMode::All).then_some(0)
        } else {
            Some(position + 1)
        };
        if let Some(target) = target {
            self.play_at(target, true).await;
            if let Some(on_advance) = &self.inner.on_advance {
                on_advance();
            }
        }
    }
}

impl<E: AudioEngine + 'static, R: Runtime + 'static> Inner<E, R> {
    fn report(&self, message: &str, file_id: Option<&str>) {
        if let Some(on_error) = &self.on_error {
            on_error(message, file_id);
        }
    }
}

#[cfg(test)]
#[path = "playlist_player_tests.rs"]
mod tests;
