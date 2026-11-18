use super::ensure_analyzed::{ensure_track_analyzed_with_engine, is_analysis_fresh};
use crate::audio_engine::AudioEngine;
use crate::library_store::{LibraryStore, StoreError, TrackRecord};
use crate::metadata::track_to_file_ref;
use crate::tasks::yield_now;

pub struct AnalyzeProgress<'a> {
    pub track: &'a TrackRecord,
    pub index: usize,
    pub total: usize,
    pub skipped: bool,
    /// Why this track could not be analyzed, when it couldn't.
    pub error: Option<String>,
}

/// Eagerly analyzes every track in the given list that's new or whose stored analysis is stale (built on the same
/// [`ensure_track_analyzed_with_engine`] primitive the just-in-time path uses when a track is decoded for playback/preload).
/// Not auto-triggered by the apps - decoding and analyzing an entire library up front turned out to starve the UI for as long
/// as it ran, for tracks the person might never even play. Kept as an available utility (e.g. for an explicit "analyze
/// everything now" action) since the per-track logic and skip/retry semantics are still useful on their own.
///
/// Analysis failures (corrupt/unreadable file, decode error) are caught per track and reported through `on_progress` rather
/// than aborting the whole run - one bad file shouldn't block analyzing the rest of the library. A failure of the store itself
/// is returned. `now_ms` stamps new results.
pub async fn analyze_library<E: AudioEngine, S: LibraryStore>(engine: &E, store: &S, tracks: &[TrackRecord], now_ms: f64, mut on_progress: Option<&mut dyn FnMut(AnalyzeProgress<'_>)>) -> Result<(), StoreError> {
    for (index, track) in tracks.iter().enumerate() {
        let file = track_to_file_ref(track);
        let existing = store.get_analysis(&track.file_id).await?;
        let (skipped, error) = if is_analysis_fresh(existing.as_ref(), &file) {
            (true, None)
        } else {
            match engine.decode_file(&file).await {
                Err(error) => (false, Some(error.to_string())),
                Ok(decoded) => match ensure_track_analyzed_with_engine(store, &file, &decoded, engine, now_ms).await {
                    Ok(_) => (false, None),
                    Err(error) => (false, Some(error.to_string())),
                },
            }
        };
        if let Some(on_progress) = on_progress.as_mut() {
            on_progress(AnalyzeProgress { track, index, total: tracks.len(), skipped, error });
        }
        yield_now().await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::ANALYSIS_ALGORITHM_VERSION;
    use crate::audio_engine::fake::FakeEngine;
    use crate::http::block_on;
    use crate::library_store::{AnalysisResult, MemoryLibraryStore};

    fn track(id: &str) -> TrackRecord {
        TrackRecord { file_id: id.into(), root_id: "r".into(), relative_path: format!("Music/{id}.mp3"), size_bytes: 10, last_modified_ms: 1.0, missing: false }
    }

    fn run(engine: &FakeEngine, store: &MemoryLibraryStore, tracks: &[TrackRecord]) -> Vec<(String, bool, Option<String>, usize, usize)> {
        let mut seen = Vec::new();
        let mut record = |p: AnalyzeProgress<'_>| seen.push((p.track.file_id.clone(), p.skipped, p.error, p.index, p.total));
        block_on(analyze_library(engine, store, tracks, 99.0, Some(&mut record))).unwrap();
        seen
    }

    #[test]
    fn analyzes_new_tracks_skips_fresh_ones_and_reports_each() {
        let (engine, store) = (FakeEngine::default(), MemoryLibraryStore::new());
        block_on(store.put_analysis(AnalysisResult { file_id: "fresh".into(), normalization_gain: 0.5, analyzed_at_ms: 1.0, size_bytes: 10, last_modified_ms: 1.0, algorithm_version: ANALYSIS_ALGORITHM_VERSION })).unwrap();
        let seen = run(&engine, &store, &[track("fresh"), track("new")]);
        assert_eq!(seen, [("fresh".into(), true, None, 0, 2), ("new".into(), false, None, 1, 2)]);
        assert_eq!(engine.decode_count("fresh"), 0, "a fresh result is not decoded again");
        assert_eq!(block_on(store.get_analysis("new")).unwrap().map(|a| a.analyzed_at_ms), Some(99.0));
    }

    #[test]
    fn one_unreadable_file_does_not_stop_the_rest() {
        let (engine, store) = (FakeEngine::default(), MemoryLibraryStore::new());
        engine.fail_decoding("bad");
        let seen = run(&engine, &store, &[track("bad"), track("good")]);
        assert!(seen[0].2.as_deref().unwrap().contains("no such file"));
        assert_eq!(seen[1], ("good".into(), false, None, 1, 2));
        assert!(block_on(store.get_analysis("bad")).unwrap().is_none() && block_on(store.get_analysis("good")).unwrap().is_some());
    }

    #[test]
    fn an_empty_list_does_nothing() {
        assert!(run(&FakeEngine::default(), &MemoryLibraryStore::new(), &[]).is_empty());
    }
}
