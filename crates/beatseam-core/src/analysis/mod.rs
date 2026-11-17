//! Port of `packages/core/src/analysis`: the pure signal-processing parts (BPM, loudness, silence, mono mix and the
//! per-track analysis). Synchronous on purpose: the TypeScript yields to the event loop between steps, which in Rust
//! is the caller's job (run it on a worker thread or an executor).

mod analyze_track;
mod bpm;
mod loudness;
mod mono;
mod silence;

pub use analyze_track::{ANALYSIS_ALGORITHM_VERSION, TrackAnalysis, analyze_track};
pub use bpm::{BpmEstimate, estimate_bpm};
pub use loudness::{compute_normalization_gain, measure_loudness_db};
pub use mono::mix_to_mono;
pub use silence::{ContentBounds, find_content_bounds};
