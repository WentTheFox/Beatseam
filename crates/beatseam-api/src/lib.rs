//! The Beatseam server's wire format and a client for it, ported from `packages/api` (TypeScript, zod).
//!
//! **Read `docs/api-compatibility.md` before changing anything here.** The server and the apps are released
//! separately, so every change must stay compatible in both directions: additive changes only (new response fields
//! optional with a fallback, never `deny_unknown_fields` on a response), expand-then-contract instead of renaming or
//! removing, and a breaking change needs an `API_VERSION` bump plus a deprecation window.
//!
//! The DTOs are plain serde structs whose JSON is identical to what the zod schemas accept and the TypeScript server
//! produces; `tests/dto_golden.rs` checks that against the zod schemas themselves.
#![allow(clippy::type_complexity, clippy::arc_with_non_send_sync, clippy::too_many_arguments)] // see beatseam-core's lib.rs

pub mod auth;
pub mod client;
pub mod compatibility;
pub mod encoding;
pub mod lastfm;
pub mod mutations;
pub mod schemas;
pub mod sync;
pub mod track_key;

pub use client::{ApiClient, ApiClientError, ApiClientOptions, ApiError};
pub use compatibility::{ApiCompatibility, CompatibilityProblem, check_api_compatibility, parse_server_api_versions};
pub use schemas::*;
pub use track_key::{decode_track_key, encode_track_key};
