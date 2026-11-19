//! The offline mutation queue's wire format (`packages/api/src/mutations.ts`).
//!
//! Changes a client makes while offline are queued and sent here later. Each carries a client-chosen `opId`, so a retry
//! after a dropped connection is recognised and not applied twice; each is applied on its own, in order.
//!
//! Merge rules, so two devices editing offline converge instead of fighting:
//!  - adding/removing tracks are set-like (adding something already there, or removing something already gone, is a
//!    no-op), so concurrent adds and removes combine;
//!  - renaming, replacing a track list, settings and playback state are last-write-wins by the order the server
//!    receives them.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const MAX_PLAYBACK_STATE_BYTES: usize = 256 * 1024;
pub const MAX_SETTING_VALUE_BYTES: usize = 16 * 1024;
pub const MAX_MUTATIONS_PER_REQUEST: usize = 100;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum MutationDto {
    #[serde(rename = "playlist.create")]
    PlaylistCreate {
        op_id: String,
        id: String,
        name: String,
        #[serde(default)]
        track_ids: Vec<String>,
    },
    #[serde(rename = "playlist.rename")]
    PlaylistRename { op_id: String, id: String, name: String },
    #[serde(rename = "playlist.delete")]
    PlaylistDelete { op_id: String, id: String },
    #[serde(rename = "playlist.addTracks")]
    PlaylistAddTracks { op_id: String, id: String, track_ids: Vec<String> },
    #[serde(rename = "playlist.removeTracks")]
    PlaylistRemoveTracks { op_id: String, id: String, track_ids: Vec<String> },
    #[serde(rename = "playlist.setTracks")]
    PlaylistSetTracks { op_id: String, id: String, track_ids: Vec<String> },
    #[serde(rename = "playbackState.set")]
    PlaybackStateSet { op_id: String, device_id: String, state: Map<String, Value> },
    #[serde(rename = "setting.set")]
    SettingSet { op_id: String, key: String, value: String },
    #[serde(rename = "setting.delete")]
    SettingDelete { op_id: String, key: String },
}

impl MutationDto {
    pub fn op_id(&self) -> &str {
        match self {
            Self::PlaylistCreate { op_id, .. }
            | Self::PlaylistRename { op_id, .. }
            | Self::PlaylistDelete { op_id, .. }
            | Self::PlaylistAddTracks { op_id, .. }
            | Self::PlaylistRemoveTracks { op_id, .. }
            | Self::PlaylistSetTracks { op_id, .. }
            | Self::PlaybackStateSet { op_id, .. }
            | Self::SettingSet { op_id, .. }
            | Self::SettingDelete { op_id, .. } => op_id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MutationsRequestDto {
    pub mutations: Vec<MutationDto>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MutationStatus {
    /// Done.
    Applied,
    /// This opId was already applied (safe to drop from the queue).
    Duplicate,
    /// Will never succeed (not yours, too large, no such playlist) - drop it and tell the user.
    Rejected,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MutationResultDto {
    pub op_id: String,
    pub status: MutationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationsResponseDto {
    pub results: Vec<MutationResultDto>,
}
