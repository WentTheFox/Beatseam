use beatseam_api::sync::SyncChangeDto;
use beatseam_api::{TrackDto, decode_track_key};
use beatseam_core::analysis::ANALYSIS_ALGORITHM_VERSION;
use beatseam_core::library_store::{AnalysisResult, LibraryStore, PlaylistRecord, StoreError, TrackRecord};
use beatseam_core::metadata::{METADATA_PARSER_VERSION, TrackMetadata};

use crate::device_playback::store_device_playback;

/// Local playlist ids for playlists the user built in the app (as opposed to a library m3u8, whose id is its file id).
pub const USER_PLAYLIST_ID_PREFIX: &str = "user:";

/// Stands in for the .lrc file the server has assigned to a track: the app only needs to know there is one.
pub const SERVER_LYRICS_ASSIGNMENT: &str = "server:lyrics";

/// The account-wide preferences that sync between devices (`SYNCED_SETTING_KEYS`): theme, accent, normalization, crossfade,
/// lyrics, volume button. Last.fm/Discord stay per-device on purpose, and the app only accepts these keys from a server.
/// (Moves next to the settings storage when that is ported.)
pub const SYNCED_SETTING_KEYS: [&str; 6] = [
    "settings.themeMode",
    "settings.accentColor",
    "settings.volumeNormalizationEnabled",
    "settings.crossfadeSeconds",
    "settings.lyricsEnabled",
    "settings.showVolumeButtonOnNowPlaying",
];

pub struct ApplyContext<'a, S> {
    pub store: &'a S,
    /// Where playlists that don't belong to a root (the user's own) are filed, so the existing per-root library screens
    /// show them.
    pub default_root_id: &'a dyn Fn() -> Option<String>,
    pub now: &'a dyn Fn() -> f64,
    /// This device's own id, so its own playback state coming back from the server is not mistaken for another device's.
    pub device_id: &'a str,
}

/// A key that doesn't decode is skipped rather than failing the whole sync.
fn file_id_of(key: &str) -> Option<String> {
    decode_track_key(key).ok()
}

fn file_ids_of(keys: &[String]) -> Vec<String> {
    keys.iter().filter_map(|k| file_id_of(k)).collect()
}

/// Turns the server's view of a track into what the rest of the app already reads. A track's tags, duration and loudness
/// arrive with it, so the app never reads a file or decodes audio just to show a title: the records are written as if a
/// local scan had produced them.
pub async fn apply_track<S: LibraryStore>(ctx: &ApplyContext<'_, S>, file_id: &str, dto: &TrackDto) -> Result<(), StoreError> {
    ctx.store.upsert_track(TrackRecord { file_id: file_id.to_owned(), root_id: dto.root_id.clone(), relative_path: dto.relative_path.clone(), size_bytes: dto.size_bytes, last_modified_ms: dto.last_modified_ms, missing: dto.missing }).await?;
    if dto.missing {
        return Ok(()); // a placeholder for a playlist entry whose file isn't there: nothing to read
    }

    // The file changed under the same id (the content hash differs): whatever art was cached is stale.
    if let Some(previous) = ctx.store.get_metadata(file_id).await?
        && let (Some(new), Some(old)) = (&dto.content_hash, &previous.content_hash)
            && old != new {
                ctx.store.put_cover_art(file_id, None).await?;
            }

    ctx.store
        .put_metadata(TrackMetadata {
            file_id: file_id.to_owned(),
            title: dto.title.clone(),
            artists: dto.artists.clone(),
            album: dto.album.clone(),
            duration_seconds: dto.duration_seconds,
            size_bytes: dto.size_bytes,
            last_modified_ms: dto.last_modified_ms,
            // Current, so the UI treats it as scanned and never polls for it - see `is_metadata_current`.
            parser_version: METADATA_PARSER_VERSION,
            content_hash: dto.content_hash.clone(),
        })
        .await?;
    // The track list's lyrics indicator and the lyrics menu read this; the lyrics themselves are fetched from the server.
    ctx.store.put_lyrics_assignment(file_id, dto.has_lyrics.then(|| SERVER_LYRICS_ASSIGNMENT.to_owned())).await?;

    if let Some(gain) = dto.normalization_gain {
        ctx.store
            .put_analysis(AnalysisResult {
                file_id: file_id.to_owned(),
                normalization_gain: gain,
                analyzed_at_ms: (ctx.now)(),
                size_bytes: dto.size_bytes,
                last_modified_ms: dto.last_modified_ms,
                // The server runs this same analysis code, so what it computed is current by definition; with this written,
                // playing the track never re-analyses it on the device.
                algorithm_version: ANALYSIS_ALGORITHM_VERSION,
            })
            .await?;
    }
    Ok(())
}

/// Applies one entry of the sync feed. Returns whether it changed something the library screens show: `false` for an
/// entity the app doesn't use (or only keeps for the "continue" list).
pub async fn apply_change<S: LibraryStore>(ctx: &ApplyContext<'_, S>, change: &SyncChangeDto) -> Result<bool, StoreError> {
    match change {
        SyncChangeDto::Track { id, track } => {
            let Some(file_id) = file_id_of(id) else { return Ok(true) };
            match track {
                Some(track) => apply_track(ctx, &file_id, track).await?,
                None => ctx.store.delete_track(&file_id).await?,
            }
            Ok(true)
        }
        SyncChangeDto::Playlist { id, playlist } => {
            let Some(playlist_id) = file_id_of(id) else { return Ok(true) };
            match playlist {
                None => ctx.store.delete_playlist(&playlist_id).await?,
                Some(p) => ctx.store.upsert_playlist(PlaylistRecord { id: playlist_id.clone(), root_id: p.root_id.clone(), file_id: playlist_id, name: p.name.clone(), track_file_ids: file_ids_of(&p.track_ids) }).await?,
            }
            Ok(true)
        }
        SyncChangeDto::UserPlaylist { id, user_playlist } => {
            let local_id = format!("{USER_PLAYLIST_ID_PREFIX}{id}");
            let Some(playlist) = user_playlist else {
                ctx.store.delete_playlist(&local_id).await?;
                return Ok(true);
            };
            // No library root to show it under yet; the next sync (after roots arrive) picks it up.
            let Some(root_id) = (ctx.default_root_id)() else { return Ok(true) };
            ctx.store.upsert_playlist(PlaylistRecord { id: local_id.clone(), root_id, file_id: local_id, name: playlist.name.clone(), track_file_ids: file_ids_of(&playlist.track_ids) }).await?;
            Ok(true)
        }
        SyncChangeDto::Setting { setting, .. } => {
            // Only the account-wide preferences; see SYNCED_SETTING_KEYS for why anything else is ignored.
            let Some(setting) = setting else { return Ok(false) };
            if !SYNCED_SETTING_KEYS.contains(&setting.key.as_str()) {
                return Ok(false);
            }
            ctx.store.put_setting(&setting.key, &setting.value).await?;
            Ok(true)
        }
        SyncChangeDto::PlaybackState { id, playback_state } => {
            store_device_playback(ctx.store, ctx.device_id, id, playback_state.as_ref()).await?;
            Ok(false) // kept for the "continue" list only; nothing in the library changed
        }
    }
}

#[cfg(test)]
mod tests {
    use beatseam_api::sync::{PlaybackStateDto, UserPlaylistDto};
    use beatseam_api::{PlaylistDto, encode_track_key};
    use beatseam_core::http::block_on;
    use beatseam_core::library_store::MemoryLibraryStore;
    use serde_json::{Map, json};

    use super::*;
    use beatseam_api::sync::UserSettingDto;

    fn ctx<'a>(store: &'a MemoryLibraryStore, root: &'a dyn Fn() -> Option<String>, now: &'a dyn Fn() -> f64) -> ApplyContext<'a, MemoryLibraryStore> {
        ApplyContext { store, default_root_id: root, now, device_id: "me" }
    }

    fn track_dto(path: &str) -> TrackDto {
        TrackDto {
            id: encode_track_key(path),
            root_id: "root".into(),
            relative_path: path.into(),
            size_bytes: 100,
            last_modified_ms: 5.0,
            missing: false,
            title: Some("Song".into()),
            artists: vec!["A".into()],
            album: Some("Alb".into()),
            duration_seconds: Some(200.0),
            content_hash: Some("h1".into()),
            normalization_gain: Some(0.8),
            has_cover: false,
            has_lyrics: true,
        }
    }

    fn run(store: &MemoryLibraryStore, change: SyncChangeDto) -> bool {
        block_on(apply_change(&ctx(store, &|| Some("root".into()), &|| 42.0), &change)).unwrap()
    }

    #[test]
    fn a_track_arrives_as_a_scanned_one_with_metadata_lyrics_flag_and_analysis() {
        let store = MemoryLibraryStore::new();
        let dto = track_dto("Dir/a.mp3");
        assert!(run(&store, SyncChangeDto::Track { id: dto.id.clone(), track: Some(dto.clone()) }));
        let track = &store.snapshot_tracks()[0];
        assert_eq!((track.file_id.as_str(), track.root_id.as_str(), track.relative_path.as_str(), track.size_bytes, track.missing), ("Dir/a.mp3", "root", "Dir/a.mp3", 100, false));
        let metadata = block_on(store.get_metadata("Dir/a.mp3")).unwrap().unwrap();
        assert_eq!((metadata.title.as_deref(), metadata.parser_version, metadata.content_hash.as_deref()), (Some("Song"), METADATA_PARSER_VERSION, Some("h1")));
        assert_eq!(block_on(store.get_lyrics_assignment("Dir/a.mp3")).unwrap().as_deref(), Some(SERVER_LYRICS_ASSIGNMENT));
        let analysis = block_on(store.get_analysis("Dir/a.mp3")).unwrap().unwrap();
        assert_eq!((analysis.normalization_gain, analysis.analyzed_at_ms, analysis.algorithm_version), (0.8, 42.0, ANALYSIS_ALGORITHM_VERSION));
    }

    #[test]
    fn a_missing_placeholder_writes_only_the_track_and_no_gain_means_no_analysis() {
        let store = MemoryLibraryStore::new();
        let mut gone = track_dto("gone.mp3");
        gone.missing = true;
        run(&store, SyncChangeDto::Track { id: gone.id.clone(), track: Some(gone) });
        assert!(store.snapshot_tracks()[0].missing);
        assert!(block_on(store.get_metadata("gone.mp3")).unwrap().is_none());

        let mut ungained = track_dto("b.mp3");
        ungained.normalization_gain = None;
        ungained.has_lyrics = false;
        run(&store, SyncChangeDto::Track { id: ungained.id.clone(), track: Some(ungained) });
        assert!(block_on(store.get_analysis("b.mp3")).unwrap().is_none());
        assert_eq!(block_on(store.get_lyrics_assignment("b.mp3")).unwrap(), None);
    }

    #[test]
    fn a_changed_content_hash_drops_the_cached_cover_art() {
        let store = MemoryLibraryStore::new();
        let dto = track_dto("a.mp3");
        run(&store, SyncChangeDto::Track { id: dto.id.clone(), track: Some(dto.clone()) });
        block_on(store.put_cover_art("a.mp3", Some(beatseam_core::metadata::CoverArtBytes { mime_type: "image/png".into(), data: vec![1, 2, 3] }))).unwrap();
        run(&store, SyncChangeDto::Track { id: dto.id.clone(), track: Some(dto.clone()) });
        assert!(block_on(store.get_cover_art("a.mp3")).unwrap().is_some(), "same hash keeps it");
        let mut changed = dto.clone();
        changed.content_hash = Some("h2".into());
        run(&store, SyncChangeDto::Track { id: dto.id.clone(), track: Some(changed) });
        assert!(block_on(store.get_cover_art("a.mp3")).unwrap().is_none());
    }

    #[test]
    fn a_removed_track_is_deleted_and_an_undecodable_key_is_skipped() {
        let store = MemoryLibraryStore::new();
        let dto = track_dto("a.mp3");
        run(&store, SyncChangeDto::Track { id: dto.id.clone(), track: Some(dto.clone()) });
        run(&store, SyncChangeDto::Track { id: dto.id.clone(), track: None });
        assert!(store.snapshot_tracks().is_empty());
        assert!(run(&store, SyncChangeDto::Track { id: "!!not base64!!".into(), track: Some(dto) }), "skipped, not failed");
        assert!(store.snapshot_tracks().is_empty());
    }

    #[test]
    fn playlists_decode_their_track_keys_and_drop_undecodable_ones() {
        let store = MemoryLibraryStore::new();
        let id = encode_track_key("Mix.m3u8");
        let playlist = PlaylistDto { id: id.clone(), root_id: "root".into(), name: "Mix".into(), track_ids: vec![encode_track_key("a.mp3"), "%%".into(), encode_track_key("b.mp3")] };
        run(&store, SyncChangeDto::Playlist { id: id.clone(), playlist: Some(playlist) });
        let stored = &store.snapshot_playlists()[0];
        assert_eq!((stored.id.as_str(), stored.file_id.as_str(), stored.name.as_str(), stored.track_file_ids.clone()), ("Mix.m3u8", "Mix.m3u8", "Mix", vec!["a.mp3".to_string(), "b.mp3".to_string()]));
        run(&store, SyncChangeDto::Playlist { id, playlist: None });
        assert!(store.snapshot_playlists().is_empty());
    }

    #[test]
    fn user_playlists_are_filed_under_the_default_root_with_a_prefixed_id() {
        let store = MemoryLibraryStore::new();
        let dto = UserPlaylistDto { id: "abc".into(), name: "Mine".into(), track_ids: vec![encode_track_key("a.mp3")], updated_at_ms: 1.0 };
        run(&store, SyncChangeDto::UserPlaylist { id: "abc".into(), user_playlist: Some(dto.clone()) });
        let stored = &store.snapshot_playlists()[0];
        assert_eq!((stored.id.as_str(), stored.root_id.as_str(), stored.track_file_ids.clone()), ("user:abc", "root", vec!["a.mp3".to_string()]));
        run(&store, SyncChangeDto::UserPlaylist { id: "abc".into(), user_playlist: None });
        assert!(store.snapshot_playlists().is_empty());

        // No root yet: nothing written, and it still counts as handled.
        let none = |c: &MemoryLibraryStore| block_on(apply_change(&ctx(c, &|| None, &|| 0.0), &SyncChangeDto::UserPlaylist { id: "abc".into(), user_playlist: Some(dto.clone()) })).unwrap();
        assert!(none(&store));
        assert!(store.snapshot_playlists().is_empty());
    }

    #[test]
    fn only_whitelisted_settings_are_accepted_from_a_server() {
        let store = MemoryLibraryStore::new();
        let setting = |key: &str| SyncChangeDto::Setting { id: key.into(), setting: Some(UserSettingDto { key: key.into(), value: "dark".into(), updated_at_ms: 1.0 }) };
        assert!(run(&store, setting("settings.themeMode")));
        assert_eq!(block_on(store.get_setting("settings.themeMode")).unwrap().as_deref(), Some("dark"));
        assert!(!run(&store, setting("settings.lastFmSessionKey")));
        assert_eq!(block_on(store.get_setting("settings.lastFmSessionKey")).unwrap(), None);
        assert!(!run(&store, SyncChangeDto::Setting { id: "settings.themeMode".into(), setting: None }));
    }

    #[test]
    fn another_devices_playback_state_is_kept_for_continue_but_changes_nothing_in_the_library() {
        let store = MemoryLibraryStore::new();
        let mut state = Map::new();
        state.insert("playlistId".into(), json!("p"));
        state.insert("currentTrackFileId".into(), json!("t"));
        let dto = PlaybackStateDto { device_id: "other".into(), state, updated_at_ms: 1.0 };
        assert!(!run(&store, SyncChangeDto::PlaybackState { id: "other".into(), playback_state: Some(dto.clone()) }));
        assert!(block_on(store.get_setting(crate::device_playback::DEVICE_PLAYBACK_KEY)).unwrap().unwrap().contains("other"));
        assert!(!run(&store, SyncChangeDto::PlaybackState { id: "me".into(), playback_state: Some(dto) }));
        assert!(!block_on(store.get_setting(crate::device_playback::DEVICE_PLAYBACK_KEY)).unwrap().unwrap().contains(r#""me""#));
    }
}
