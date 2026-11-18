use beatseam_api::sync::PlaybackStateDto;
use beatseam_core::library_store::{LibraryStore, LoopMode, PlaybackState, StoreError};
use serde_json::{Map, Value};

pub const DEVICE_PLAYBACK_KEY: &str = "serverMode.devicePlayback";
/// A shuffle order longer than this isn't sent: it would be most of the server's size limit for a state, and the other
/// device can reshuffle.
const MAX_SENT_SHUFFLE_ORDER: usize = 1500;
/// A device's "playing" is believed this long after its last report (it reports at least every ~20s while playing).
const PLAYING_FRESH_MS: f64 = 45_000.0;
/// Another device's state older than this is not offered as something to continue.
const MAX_AGE_MS: f64 = 30.0 * 24.0 * 60.0 * 60.0 * 1000.0;

/// What another device was playing, as this device offers to continue it.
#[derive(Clone, Debug, PartialEq)]
pub struct RemotePlayback {
    pub device_id: String,
    /// What that device calls itself ("Pixel 8").
    pub device_name: String,
    pub updated_at_ms: f64,
    pub playlist_id: String,
    pub root_id: Option<String>,
    pub current_track_file_id: String,
    pub position_seconds: f64,
    pub loop_mode: LoopMode,
    pub shuffle_enabled: bool,
    pub shuffle_order: Option<Vec<String>>,
    /// Was playing (not paused) when it last reported. See [`is_playing_now`] for whether that can still be believed.
    pub playing: bool,
}

fn loop_mode_name(mode: LoopMode) -> &'static str {
    match mode {
        LoopMode::Off => "off",
        LoopMode::All => "all",
        LoopMode::One => "one",
    }
}

fn string_or_null(value: &Option<String>) -> Value {
    value.as_ref().map_or(Value::Null, |s| Value::String(s.clone()))
}

fn number(n: f64) -> Value {
    // JSON has no NaN/Infinity; JSON.stringify writes null for them.
    serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
}

/// The part of this device's playback state that is worth sending (the rest is local screen state and volume).
pub fn playback_payload(state: &PlaybackState, device_name: &str, now: f64, playing: bool) -> Map<String, Value> {
    let order = match &state.shuffle_order {
        Some(order) if state.shuffle_enabled && order.len() <= MAX_SENT_SHUFFLE_ORDER => Value::Array(order.iter().cloned().map(Value::String).collect()),
        _ => Value::Null,
    };
    let mut map = Map::new();
    map.insert("deviceName".into(), device_name.into());
    map.insert("updatedAtMs".into(), number(now));
    map.insert("playlistId".into(), string_or_null(&state.playlist_id));
    map.insert("rootId".into(), string_or_null(&state.root_id));
    map.insert("currentTrackFileId".into(), string_or_null(&state.current_track_file_id));
    map.insert("positionSeconds".into(), number(state.position_seconds));
    map.insert("loopMode".into(), loop_mode_name(state.loop_mode).into());
    map.insert("shuffleEnabled".into(), state.shuffle_enabled.into());
    map.insert("shuffleOrder".into(), order);
    map.insert("playing".into(), playing.into());
    map
}

fn parse_loop_mode(value: Option<&Value>) -> LoopMode {
    match value.and_then(Value::as_str) {
        Some("all") => LoopMode::All,
        Some("one") => LoopMode::One,
        _ => LoopMode::Off,
    }
}

/// Reads one device's DTO back into something usable, or `None` if it isn't a playable state.
pub fn to_remote_playback(dto: &PlaybackStateDto) -> Option<RemotePlayback> {
    let s = &dto.state;
    let playlist_id = s.get("playlistId")?.as_str()?.to_owned();
    let current_track_file_id = s.get("currentTrackFileId")?.as_str()?.to_owned();
    Some(RemotePlayback {
        device_id: dto.device_id.clone(),
        device_name: s.get("deviceName").and_then(Value::as_str).filter(|n| !n.is_empty()).unwrap_or("Another device").to_owned(),
        updated_at_ms: s.get("updatedAtMs").and_then(Value::as_f64).unwrap_or(dto.updated_at_ms),
        playlist_id,
        root_id: s.get("rootId").and_then(Value::as_str).map(str::to_owned),
        current_track_file_id,
        position_seconds: s.get("positionSeconds").and_then(Value::as_f64).filter(|p| *p >= 0.0).unwrap_or(0.0),
        loop_mode: parse_loop_mode(s.get("loopMode")),
        shuffle_enabled: s.get("shuffleEnabled") == Some(&Value::Bool(true)),
        shuffle_order: s.get("shuffleOrder").and_then(Value::as_array).and_then(|a| a.iter().map(|v| v.as_str().map(str::to_owned)).collect()),
        playing: s.get("playing") == Some(&Value::Bool(true)),
    })
}

/// Whether that device can be taken to be playing right now: it said so, recently enough (a closed app can't say it
/// stopped).
pub fn is_playing_now(remote: &RemotePlayback, now: f64) -> bool {
    remote.playing && now - remote.updated_at_ms < PLAYING_FRESH_MS
}

/// The other devices' states as last synced, keyed by device id. Anything unreadable is "none".
async fn read_all<S: LibraryStore>(store: &S) -> Result<Map<String, Value>, StoreError> {
    let raw = store.get_setting(DEVICE_PLAYBACK_KEY).await?;
    Ok(match raw.as_deref().filter(|r| !r.is_empty()).map(serde_json::from_str::<Value>) {
        Some(Ok(Value::Object(map))) => map,
        _ => Map::new(),
    })
}

/// Records (or, with `None`, forgets) another device's state. This device's own is never kept: it already has it.
pub async fn store_device_playback<S: LibraryStore>(store: &S, own_device_id: &str, device_id: &str, dto: Option<&PlaybackStateDto>) -> Result<(), StoreError> {
    if device_id == own_device_id {
        return Ok(());
    }
    let mut all = read_all(store).await?;
    match dto {
        Some(dto) => {
            all.insert(device_id.to_owned(), serde_json::to_value(dto).expect("a DTO always serializes"));
        }
        None => {
            all.remove(device_id);
        }
    }
    store.put_setting(DEVICE_PLAYBACK_KEY, &Value::Object(all).to_string()).await
}

/// What could be continued here, most recently played first.
pub async fn list_device_playback<S: LibraryStore>(store: &S, now: f64) -> Result<Vec<RemotePlayback>, StoreError> {
    let all = read_all(store).await?;
    let mut list: Vec<RemotePlayback> = all
        .values()
        .filter_map(|v| serde_json::from_value::<PlaybackStateDto>(v.clone()).ok())
        .filter_map(|dto| to_remote_playback(&dto))
        .filter(|remote| now - remote.updated_at_ms < MAX_AGE_MS)
        .collect();
    list.sort_by(|a, b| b.updated_at_ms.total_cmp(&a.updated_at_ms));
    Ok(list)
}

#[cfg(test)]
mod tests {
    use beatseam_core::http::block_on;
    use beatseam_core::library_store::{MemoryLibraryStore, OpenedPlaylist};

    use super::*;

    fn state(shuffle: bool, order: Option<Vec<String>>) -> PlaybackState {
        PlaybackState {
            playlist_id: Some("p1".into()),
            current_track_file_id: Some("t1".into()),
            root_id: Some("r".into()),
            opened_playlist_id: OpenedPlaylist::default(),
            opened_root_id: None,
            position_seconds: 12.5,
            loop_mode: LoopMode::All,
            shuffle_enabled: shuffle,
            shuffle_order: order,
            volume: 0.5,
            now_playing_open: true,
        }
    }

    fn dto(device: &str, updated: f64, mut extra: Map<String, Value>) -> PlaybackStateDto {
        extra.entry("playlistId").or_insert("p1".into());
        extra.entry("currentTrackFileId").or_insert("t1".into());
        PlaybackStateDto { device_id: device.into(), state: extra, updated_at_ms: updated }
    }

    #[test]
    fn the_payload_has_exactly_the_sendable_fields() {
        let payload = playback_payload(&state(true, Some(vec!["a".into(), "b".into()])), "Pixel", 1000.0, true);
        assert_eq!(
            Value::Object(payload).to_string(),
            r#"{"currentTrackFileId":"t1","deviceName":"Pixel","loopMode":"all","playing":true,"playlistId":"p1","positionSeconds":12.5,"rootId":"r","shuffleEnabled":true,"shuffleOrder":["a","b"],"updatedAtMs":1000.0}"#
        );
    }

    #[test]
    fn the_shuffle_order_is_only_sent_when_shuffling_and_short_enough() {
        let long: Vec<String> = (0..1501).map(|i| i.to_string()).collect();
        assert_eq!(playback_payload(&state(true, Some(long)), "d", 1.0, false)["shuffleOrder"], Value::Null);
        assert_eq!(playback_payload(&state(false, Some(vec!["a".into()])), "d", 1.0, false)["shuffleOrder"], Value::Null);
        let exactly: Vec<String> = (0..1500).map(|i| i.to_string()).collect();
        assert_eq!(playback_payload(&state(true, Some(exactly)), "d", 1.0, false)["shuffleOrder"].as_array().map(Vec::len), Some(1500));
    }

    #[test]
    fn reading_back_applies_the_typescripts_fallbacks() {
        let full = to_remote_playback(&dto("d1", 5.0, serde_json::from_str(r#"{"deviceName":"Pixel","updatedAtMs":9,"rootId":"r","positionSeconds":3,"loopMode":"one","shuffleEnabled":true,"shuffleOrder":["a"],"playing":true}"#).unwrap())).unwrap();
        assert_eq!((full.device_name.as_str(), full.updated_at_ms, full.root_id.as_deref(), full.position_seconds, full.loop_mode, full.shuffle_enabled, full.shuffle_order.clone(), full.playing), ("Pixel", 9.0, Some("r"), 3.0, LoopMode::One, true, Some(vec!["a".into()]), true));

        let bare = to_remote_playback(&dto("d2", 5.0, Map::new())).unwrap();
        assert_eq!((bare.device_name.as_str(), bare.updated_at_ms, bare.root_id, bare.position_seconds, bare.loop_mode, bare.shuffle_enabled, bare.shuffle_order, bare.playing), ("Another device", 5.0, None, 0.0, LoopMode::Off, false, None, false));

        let odd = to_remote_playback(&dto("d3", 5.0, serde_json::from_str(r#"{"deviceName":"","positionSeconds":-4,"loopMode":"sideways","shuffleOrder":["a",1],"playing":"yes"}"#).unwrap())).unwrap();
        assert_eq!((odd.device_name.as_str(), odd.position_seconds, odd.loop_mode, odd.shuffle_order, odd.playing), ("Another device", 0.0, LoopMode::Off, None, false));

        let no_track = PlaybackStateDto { device_id: "d".into(), state: Map::new(), updated_at_ms: 1.0 };
        assert_eq!(to_remote_playback(&no_track), None);
    }

    #[test]
    fn playing_is_believed_for_45_seconds() {
        let mut r = to_remote_playback(&dto("d", 0.0, serde_json::from_str(r#"{"playing":true,"updatedAtMs":1000}"#).unwrap())).unwrap();
        assert!(is_playing_now(&r, 45_999.0));
        assert!(!is_playing_now(&r, 46_000.0));
        r.playing = false;
        assert!(!is_playing_now(&r, 1001.0));
    }

    #[test]
    fn stores_other_devices_never_its_own_and_lists_recent_ones_newest_first() {
        let store = MemoryLibraryStore::new();
        let now = 100_000_000_000.0;
        let put = |device: &str, updated: f64| {
            let d = dto(device, updated, serde_json::from_str(&format!(r#"{{"updatedAtMs":{updated}}}"#)).unwrap());
            block_on(store_device_playback(&store, "me", device, Some(&d))).unwrap();
        };
        put("old", now - 1000.0);
        put("new", now - 10.0);
        put("me", now);
        put("ancient", now - MAX_AGE_MS - 1.0);
        let listed = block_on(list_device_playback(&store, now)).unwrap();
        assert_eq!(listed.iter().map(|r| r.device_id.as_str()).collect::<Vec<_>>(), ["new", "old"]);

        block_on(store_device_playback(&store, "me", "new", None)).unwrap();
        assert_eq!(block_on(list_device_playback(&store, now)).unwrap().len(), 1);
    }

    #[test]
    fn a_corrupt_stored_value_reads_as_nothing_and_unreadable_entries_are_skipped() {
        let store = MemoryLibraryStore::new();
        for raw in ["not json", "[]", "3", ""] {
            block_on(store.put_setting(DEVICE_PLAYBACK_KEY, raw)).unwrap();
            assert_eq!(block_on(list_device_playback(&store, 1.0)).unwrap(), vec![], "{raw}");
        }
        block_on(store.put_setting(DEVICE_PLAYBACK_KEY, r#"{"x":{"nonsense":true}}"#)).unwrap();
        assert_eq!(block_on(list_device_playback(&store, 1.0)).unwrap(), vec![]);
    }
}
