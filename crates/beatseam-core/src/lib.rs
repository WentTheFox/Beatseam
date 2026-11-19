//! Platform-independent Beatseam logic, ported module by module from `packages/core` (TypeScript) together with
//! its tests, which are the spec. See docs/rust-gpui-rewrite-plan.md, "Phase 1".
//!
//! Layering rule: nothing here may depend on a UI toolkit, a platform crate or a specific async runtime.

// Deliberate, crate-wide: the seams are callback- and future-typed (spelling every boxed closure as a `type` alias would
// only add indirection), everything runs on one thread behind `Rc`-like `Arc`s (the `Runtime` trait has no `Send`
// bound so the same code runs on wasm), and a few ports keep the TypeScript signature's argument list.
#![allow(clippy::type_complexity, clippy::arc_with_non_send_sync, clippy::too_many_arguments)]

pub mod analysis;
pub mod audio_engine;
pub mod app_update;
pub mod audio;
pub mod crossfade;
pub mod discord;
pub mod file_access;
pub mod http;
pub mod lyrics;
pub mod metadata;
pub mod offline;
pub mod playback;
pub mod playlist;
pub mod scrobble;
pub mod tasks;

pub mod js_compat;
pub mod library_scan;
pub mod library_store;
