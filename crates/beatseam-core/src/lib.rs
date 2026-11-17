//! Platform-independent Beatseam logic, ported module by module from `packages/core` (TypeScript) together with
//! its tests, which are the spec. See docs/rust-gpui-rewrite-plan.md, "Phase 1".
//!
//! Layering rule: nothing here may depend on a UI toolkit, a platform crate or a specific async runtime.

pub mod crossfade;
pub mod lyrics;
pub mod metadata;
pub mod playlist;
