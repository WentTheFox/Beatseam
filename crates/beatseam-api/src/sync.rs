//! The sync change feed (`packages/api/src/sync.ts`).
//!
//! Sync is a check-in, not a connection: the client remembers a cursor, asks for what changed since, applies it, and
//! stores the new cursor. Every change carries the entity's *current* state (or `None` if it no longer exists), so
//! applying one is an idempotent upsert-or-delete and a repeated or out-of-order delivery of the same entity can't
//! corrupt anything.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::schemas::{PlaylistDto, RootDto, TrackDto};

/// A playlist the user built in the app (as opposed to a shared, m3u8-backed library playlist).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserPlaylistDto {
    /// The id the client chose when it created the playlist.
    pub id: String,
    pub name: String,
    /// Track keys in play order.
    pub track_ids: Vec<String>,
    pub updated_at_ms: f64,
}

/// What one device was last playing; opaque to the server (it is the client's own state).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackStateDto {
    pub device_id: String,
    pub state: Map<String, Value>,
    pub updated_at_ms: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserSettingDto {
    pub key: String,
    pub value: String,
    pub updated_at_ms: f64,
}

/// One change. The payload key is named after the entity (`track`, `playlist`, `userPlaylist`, `playbackState`,
/// `setting`); `None` there means the entity was removed. Everything but `track` and `playlist` is the signed-in user's
/// own data - nobody else's ever appears in their feed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "entity", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum SyncChangeDto {
    /// A library track, with its tags, analysis and flags.
    Track { id: String, track: Option<TrackDto> },
    /// A shared (m3u8-backed) library playlist.
    Playlist { id: String, playlist: Option<PlaylistDto> },
    UserPlaylist { id: String, user_playlist: Option<UserPlaylistDto> },
    PlaybackState { id: String, playback_state: Option<PlaybackStateDto> },
    Setting { id: String, setting: Option<UserSettingDto> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncTotalsDto {
    pub tracks: u64,
    pub playlists: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncChangesResponseDto {
    pub changes: Vec<SyncChangeDto>,
    /// Opaque revision to send as `since` next time. Always store it, even when `changes` is empty.
    pub cursor: String,
    /// More changes are waiting beyond this page: call again with `cursor` straight away.
    pub has_more: bool,
    /// Library roots (tiny, so sent with every check-in rather than tracked by revision).
    pub roots: Vec<RootDto>,
    /// How many tracks and shared playlists exist on the server right now. A client compares them with what it holds
    /// after applying the changes: a difference means it has lost (or gained) rows some way a change feed can't repair,
    /// and it should take a snapshot instead.
    pub totals: SyncTotalsDto,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncSnapshotResponseDto {
    /// Every entity that exists, as upserts.
    pub entities: Vec<SyncChangeDto>,
    /// Pass as `after` for the next page; `None` when this was the last.
    pub next: Option<String>,
    /// The revision to start `changes` from once the snapshot is complete. Take it from the FIRST page: anything that
    /// changes while the snapshot is still being fetched is then picked up by the first `changes` call, so nothing is
    /// missed.
    pub cursor: String,
    pub roots: Vec<RootDto>,
}

/// Body of the 410 answer: the cursor is too old (history was trimmed) or unknown; fetch a snapshot instead.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResyncRequiredDto {
    pub code: ResyncRequiredCode,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResyncRequiredCode {
    ResyncRequired,
}
