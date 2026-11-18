use std::sync::Arc;

use super::{ANALYSIS_ALGORITHM_VERSION, analyze_track};
use crate::audio::DecodedAudio;
use crate::audio_engine::AudioEngine;
use crate::file_access::FileRef;
use crate::library_store::{AnalysisResult, LibraryStore, StoreError};

/// True if a stored analysis can still be trusted for the given file: same size/mtime as when it was analyzed, AND produced by
/// the current `ANALYSIS_ALGORITHM_VERSION` - a version bump makes every existing result stale even though the file itself
/// hasn't changed, the same way a changed size/mtime would. Shared with the library pass so its own pre-check (skip vs.
/// decode+analyze) can't drift out of sync with [`ensure_track_analyzed`]'s.
pub fn is_analysis_fresh(existing: Option<&AnalysisResult>, file: &FileRef) -> bool {
    existing.is_some_and(|e| e.size_bytes == file.size_bytes && e.last_modified_ms == file.last_modified_ms && e.algorithm_version == ANALYSIS_ALGORITHM_VERSION)
}

/// Returns the track's cached analysis if it's still fresh (see [`is_analysis_fresh`]), otherwise analyzes the given
/// already-decoded buffer and persists the result. The shared primitive behind both the library pass and the just-in-time
/// analysis triggered by playback/preload decoding a track for the first time - callers that already have the decoded audio
/// (they decoded it for playback or preload anyway) get analysis at no extra decode cost. `now_ms` stamps a new result.
///
/// This runs the shared analysis code, which is a few full passes over the samples: call it from a worker thread or executor
/// task. Where the platform's engine has a native implementation, use [`ensure_track_analyzed_with_engine`].
pub async fn ensure_track_analyzed<S: LibraryStore>(store: &S, file: &FileRef, decoded: &DecodedAudio, now_ms: f64) -> Result<AnalysisResult, StoreError> {
    ensure(store, file, now_ms, || async { analyze_track(decoded).normalization_gain }).await
}

/// Like [`ensure_track_analyzed`], but runs the analysis in the engine when it provides one (see
/// `AudioEngine::analyze_track` for why: UI stutter on platforms where app-side work blocks the interface).
pub async fn ensure_track_analyzed_with_engine<S: LibraryStore, E: AudioEngine>(store: &S, file: &FileRef, decoded: &Arc<DecodedAudio>, engine: &E, now_ms: f64) -> Result<AnalysisResult, StoreError> {
    ensure(store, file, now_ms, || async {
        match engine.analyze_track(decoded).await {
            Some(native) => native.normalization_gain,
            None => analyze_track(decoded).normalization_gain,
        }
    })
    .await
}

async fn ensure<S: LibraryStore, F, Fut>(store: &S, file: &FileRef, now_ms: f64, analyze: F) -> Result<AnalysisResult, StoreError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = f64>,
{
    let existing = store.get_analysis(&file.id).await?;
    if is_analysis_fresh(existing.as_ref(), file) {
        return Ok(existing.expect("fresh implies present"));
    }
    let normalization_gain = analyze().await;
    let result = AnalysisResult { file_id: file.id.clone(), normalization_gain, analyzed_at_ms: now_ms, size_bytes: file.size_bytes, last_modified_ms: file.last_modified_ms, algorithm_version: ANALYSIS_ALGORITHM_VERSION };
    store.put_analysis(result.clone()).await?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_engine::EngineTrackAnalysis;
    use crate::audio_engine::fake::FakeEngine;
    use crate::http::block_on;
    use crate::library_store::MemoryLibraryStore;

    fn file() -> FileRef {
        FileRef { id: "f".into(), name: "f.mp3".into(), relative_path: "f.mp3".into(), size_bytes: 100, last_modified_ms: 5.0 }
    }

    fn tone() -> DecodedAudio {
        let samples: Vec<f32> = (0..44_100 * 4).map(|i| (i as f32 * 0.05).sin() * 0.3).collect();
        DecodedAudio { sample_rate: 44_100, channel_data: vec![samples] }
    }

    fn stored(gain: f64, size: u64, mtime: f64, version: u32) -> AnalysisResult {
        AnalysisResult { file_id: "f".into(), normalization_gain: gain, analyzed_at_ms: 1.0, size_bytes: size, last_modified_ms: mtime, algorithm_version: version }
    }

    #[test]
    fn freshness_needs_the_same_size_mtime_and_algorithm_version() {
        let f = file();
        assert!(is_analysis_fresh(Some(&stored(1.0, 100, 5.0, ANALYSIS_ALGORITHM_VERSION)), &f));
        assert!(!is_analysis_fresh(Some(&stored(1.0, 101, 5.0, ANALYSIS_ALGORITHM_VERSION)), &f));
        assert!(!is_analysis_fresh(Some(&stored(1.0, 100, 6.0, ANALYSIS_ALGORITHM_VERSION)), &f));
        assert!(!is_analysis_fresh(Some(&stored(1.0, 100, 5.0, ANALYSIS_ALGORITHM_VERSION - 1)), &f));
        assert!(!is_analysis_fresh(None, &f));
    }

    #[test]
    fn analyzes_and_persists_a_new_track_stamped_with_the_given_time() {
        let store = MemoryLibraryStore::new();
        let result = block_on(ensure_track_analyzed(&store, &file(), &tone(), 1234.0)).unwrap();
        assert_eq!((result.file_id.as_str(), result.analyzed_at_ms, result.size_bytes, result.last_modified_ms, result.algorithm_version), ("f", 1234.0, 100, 5.0, ANALYSIS_ALGORITHM_VERSION));
        assert!(result.normalization_gain > 0.0 && result.normalization_gain.is_finite());
        assert_eq!(block_on(store.get_analysis("f")).unwrap(), Some(result));
    }

    #[test]
    fn a_fresh_cached_result_is_returned_without_analyzing_again() {
        let store = MemoryLibraryStore::new();
        block_on(store.put_analysis(stored(0.123, 100, 5.0, ANALYSIS_ALGORITHM_VERSION))).unwrap();
        let result = block_on(ensure_track_analyzed(&store, &file(), &tone(), 9.0)).unwrap();
        assert_eq!((result.normalization_gain, result.analyzed_at_ms), (0.123, 1.0), "untouched");
    }

    #[test]
    fn a_stale_result_is_replaced() {
        let store = MemoryLibraryStore::new();
        block_on(store.put_analysis(stored(0.123, 99, 5.0, ANALYSIS_ALGORITHM_VERSION))).unwrap();
        let result = block_on(ensure_track_analyzed(&store, &file(), &tone(), 9.0)).unwrap();
        assert_ne!(result.normalization_gain, 0.123);
        assert_eq!((result.size_bytes, result.analyzed_at_ms), (100, 9.0));
    }

    #[test]
    fn an_engine_with_its_own_analysis_is_used_and_one_without_falls_back_to_the_shared_code() {
        struct Native(FakeEngine);
        impl AudioEngine for Native {
            type Source = <FakeEngine as AudioEngine>::Source;
            async fn decode_file(&self, f: &FileRef) -> Result<Arc<DecodedAudio>, crate::audio_engine::DecodeError> {
                self.0.decode_file(f).await
            }
            async fn analyze_track(&self, _a: &DecodedAudio) -> Option<EngineTrackAnalysis> {
                Some(EngineTrackAnalysis { normalization_gain: 0.777 })
            }
            fn create_source(&self, a: &Arc<DecodedAudio>, e: Option<crate::audio_engine::OnEnded>) -> Self::Source {
                self.0.create_source(a, e)
            }
            fn schedule_start(&self, s: &Self::Source, w: f64, o: f64) -> Result<(), crate::audio_engine::EngineError> {
                self.0.schedule_start(s, w, o)
            }
            fn now(&self) -> f64 {
                self.0.now()
            }
        }
        let audio = Arc::new(tone());
        let store = MemoryLibraryStore::new();
        assert_eq!(block_on(ensure_track_analyzed_with_engine(&store, &file(), &audio, &Native(FakeEngine::default()), 1.0)).unwrap().normalization_gain, 0.777);

        let shared = block_on(ensure_track_analyzed(&MemoryLibraryStore::new(), &file(), &audio, 1.0)).unwrap().normalization_gain;
        assert_eq!(block_on(ensure_track_analyzed_with_engine(&MemoryLibraryStore::new(), &file(), &audio, &FakeEngine::default(), 1.0)).unwrap().normalization_gain, shared);
    }
}
