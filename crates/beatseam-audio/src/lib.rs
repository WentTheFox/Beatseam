//! The audio engine: symphonia decoding and our own sample-accurate mixer behind `beatseam-core`'s `AudioEngine` trait.
//!
//! There is no output device in here. A host (the desktop app, the CLI) opens its platform's output stream and fills it from
//! [`MixerEngine::render`]; the same call renders offline for tests and `--render`.

mod decode;
mod engine;
mod mixer;

pub use decode::decode_audio;
pub use engine::{MixerEngine, MixerSource};
pub use mixer::{MixerCore, Param};
