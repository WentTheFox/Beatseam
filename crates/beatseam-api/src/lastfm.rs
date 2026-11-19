//! Last.fm through the server (`packages/api/src/lastfm.ts`): the server holds the app credentials and the user's
//! session; the app never sees either.

use serde::{Deserialize, Serialize};

/// Last.fm takes at most 50 per call.
pub const MAX_SCROBBLES_PER_REQUEST: usize = 50;

/// Where one user's Last.fm link stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastFmStatusDto {
    pub connected: bool,
    pub username: Option<String>,
}

/// Step 1 of connecting: open this page in a browser and approve Beatseam, then ask the server to complete.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastFmConnectResultDto {
    pub auth_url: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastFmNowPlayingRequestDto {
    pub artist: String,
    pub track: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastFmScrobbleEntryDto {
    pub artist: String,
    pub track: String,
    /// Unix seconds the track STARTED playing.
    pub timestamp: i64,
    pub album: Option<String>,
    pub duration_seconds: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LastFmScrobblesRequestDto {
    pub entries: Vec<LastFmScrobbleEntryDto>,
}
