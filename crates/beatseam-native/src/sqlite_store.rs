//! The library store on SQLite (desktop and Android).

use std::path::Path;
use std::sync::Mutex;

use beatseam_core::library_store::{AnalysisResult, LibraryStore, LoopMode, LyricsScope, OpenedPlaylist, PlaybackState, PlaylistRecord, StoreError, TrackRecord};
use beatseam_core::metadata::{CoverArtBytes, TrackMetadata, encode_base64};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

/// The schema version this build writes (`PRAGMA user_version`). A database written by a newer build is refused rather than
/// silently misread.
const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE tracks (
    file_id TEXT PRIMARY KEY, root_id TEXT NOT NULL, relative_path TEXT NOT NULL,
    size_bytes INTEGER NOT NULL, last_modified_ms REAL NOT NULL, missing INTEGER NOT NULL
);
CREATE INDEX tracks_by_root ON tracks (root_id);
CREATE TABLE playlists (
    id TEXT PRIMARY KEY, root_id TEXT NOT NULL, file_id TEXT NOT NULL, name TEXT NOT NULL, track_file_ids TEXT NOT NULL
);
CREATE INDEX playlists_by_root ON playlists (root_id);
CREATE TABLE analysis (
    file_id TEXT PRIMARY KEY, normalization_gain REAL NOT NULL, analyzed_at_ms REAL NOT NULL,
    size_bytes INTEGER NOT NULL, last_modified_ms REAL NOT NULL, algorithm_version INTEGER NOT NULL
);
CREATE TABLE metadata (
    file_id TEXT PRIMARY KEY, title TEXT, artists TEXT NOT NULL, album TEXT, duration_seconds REAL,
    size_bytes INTEGER NOT NULL, last_modified_ms REAL NOT NULL, parser_version INTEGER NOT NULL, content_hash TEXT
);
CREATE TABLE cover_art (file_id TEXT PRIMARY KEY, mime_type TEXT NOT NULL, data BLOB NOT NULL);
CREATE TABLE playback_state (id INTEGER PRIMARY KEY CHECK (id = 1), json TEXT NOT NULL);
CREATE TABLE lyrics_scopes (root_id TEXT NOT NULL, relative_path TEXT NOT NULL, PRIMARY KEY (root_id, relative_path));
CREATE TABLE lyrics_assignments (file_id TEXT PRIMARY KEY, lrc_file_id TEXT NOT NULL);
CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

fn err(e: impl std::fmt::Display) -> StoreError {
    StoreError(e.to_string())
}

/// A [`LibraryStore`] in a SQLite file (or in memory, for tests). Rows come back in the order they were first written - an upsert
/// keeps its row's place - which is what the screens rely on for a stable library order.
///
/// Calls are synchronous SQLite work behind a mutex; they are quick (indexed, small rows), and a host that must never block its UI
/// thread runs the whole store on a worker.
pub struct SqliteLibraryStore {
    connection: Mutex<Connection>,
}

impl SqliteLibraryStore {
    /// Opens (creating if needed) the database at `path`.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Self::from_connection(Connection::open(path).map_err(err)?)
    }

    /// A throwaway in-memory database.
    pub fn in_memory() -> Result<Self, StoreError> {
        Self::from_connection(Connection::open_in_memory().map_err(err)?)
    }

    fn from_connection(connection: Connection) -> Result<Self, StoreError> {
        // WAL keeps readers from blocking the writer and survives a crash mid-write; ignored (stays "memory") for an in-memory database.
        let _ = connection.pragma_update(None, "journal_mode", "WAL");
        connection.pragma_update(None, "foreign_keys", "ON").map_err(err)?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0)).map_err(err)?;
        match version {
            0 => {
                connection.execute_batch(SCHEMA).map_err(err)?;
                connection.pragma_update(None, "user_version", SCHEMA_VERSION).map_err(err)?;
            }
            SCHEMA_VERSION => {}
            newer => return Err(StoreError(format!("This library database was written by a newer version of Beatseam (schema {newer}); this version understands schema {SCHEMA_VERSION}."))),
        }
        Ok(Self { connection: Mutex::new(connection) })
    }

    fn with<R>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<R>) -> Result<R, StoreError> {
        let connection = self.connection.lock().map_err(|_| err("the library database lock was poisoned by a panic"))?;
        f(&connection).map_err(err)
    }
}

fn track_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<TrackRecord> {
    Ok(TrackRecord { file_id: row.get(0)?, root_id: row.get(1)?, relative_path: row.get(2)?, size_bytes: row.get::<_, i64>(3)? as u64, last_modified_ms: row.get(4)?, missing: row.get::<_, i64>(5)? != 0 })
}

fn playlist_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<PlaylistRecord> {
    let ids: String = row.get(4)?;
    let track_file_ids = match serde_json::from_str::<Value>(&ids) {
        Ok(Value::Array(items)) => items.into_iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
        _ => Vec::new(),
    };
    Ok(PlaylistRecord { id: row.get(0)?, root_id: row.get(1)?, file_id: row.get(2)?, name: row.get(3)?, track_file_ids })
}

fn encode_playback_state(state: &PlaybackState) -> String {
    let opened = match &state.opened_playlist_id {
        OpenedPlaylist::Unset => json!({"kind": "unset"}),
        OpenedPlaylist::Library => json!({"kind": "library"}),
        OpenedPlaylist::Playlist(id) => json!({"kind": "playlist", "id": id}),
    };
    json!({
        "playlistId": state.playlist_id,
        "currentTrackFileId": state.current_track_file_id,
        "rootId": state.root_id,
        "openedPlaylistId": opened,
        "openedRootId": state.opened_root_id,
        "positionSeconds": state.position_seconds,
        "loopMode": match state.loop_mode { LoopMode::Off => "off", LoopMode::All => "all", LoopMode::One => "one" },
        "shuffleEnabled": state.shuffle_enabled,
        "shuffleOrder": state.shuffle_order,
        "volume": state.volume,
        "nowPlayingOpen": state.now_playing_open,
    })
    .to_string()
}

/// Reads a saved state, tolerating missing fields (an older build's) and unknown ones (a newer build's).
fn decode_playback_state(text: &str) -> Option<PlaybackState> {
    let v: Value = serde_json::from_str(text).ok()?;
    let string = |key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
    Some(PlaybackState {
        playlist_id: string("playlistId"),
        current_track_file_id: string("currentTrackFileId"),
        root_id: string("rootId"),
        opened_playlist_id: match v.get("openedPlaylistId") {
            Some(Value::Object(o)) => match o.get("kind").and_then(Value::as_str) {
                Some("library") => OpenedPlaylist::Library,
                Some("playlist") => o.get("id").and_then(Value::as_str).map_or(OpenedPlaylist::Unset, |id| OpenedPlaylist::Playlist(id.to_owned())),
                _ => OpenedPlaylist::Unset,
            },
            _ => OpenedPlaylist::Unset,
        },
        opened_root_id: string("openedRootId"),
        position_seconds: v.get("positionSeconds").and_then(Value::as_f64).unwrap_or(0.0),
        loop_mode: match v.get("loopMode").and_then(Value::as_str) {
            Some("all") => LoopMode::All,
            Some("one") => LoopMode::One,
            _ => LoopMode::Off,
        },
        shuffle_enabled: v.get("shuffleEnabled").and_then(Value::as_bool).unwrap_or(false),
        shuffle_order: v.get("shuffleOrder").and_then(Value::as_array).map(|a| a.iter().filter_map(|i| i.as_str().map(str::to_owned)).collect()),
        volume: v.get("volume").and_then(Value::as_f64).unwrap_or(0.25),
        now_playing_open: v.get("nowPlayingOpen").and_then(Value::as_bool).unwrap_or(false),
    })
}

impl LibraryStore for SqliteLibraryStore {
    async fn upsert_track(&self, track: TrackRecord) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "INSERT INTO tracks (file_id, root_id, relative_path, size_bytes, last_modified_ms, missing) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(file_id) DO UPDATE SET root_id = ?2, relative_path = ?3, size_bytes = ?4, last_modified_ms = ?5, missing = ?6",
                params![track.file_id, track.root_id, track.relative_path, track.size_bytes as i64, track.last_modified_ms, track.missing as i64],
            )
            .map(drop)
        })
    }

    async fn delete_track(&self, file_id: &str) -> Result<(), StoreError> {
        self.with(|c| c.execute("DELETE FROM tracks WHERE file_id = ?1", [file_id]).map(drop))
    }

    async fn upsert_playlist(&self, playlist: PlaylistRecord) -> Result<(), StoreError> {
        let ids = Value::Array(playlist.track_file_ids.iter().cloned().map(Value::String).collect()).to_string();
        self.with(|c| {
            c.execute(
                "INSERT INTO playlists (id, root_id, file_id, name, track_file_ids) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET root_id = ?2, file_id = ?3, name = ?4, track_file_ids = ?5",
                params![playlist.id, playlist.root_id, playlist.file_id, playlist.name, ids],
            )
            .map(drop)
        })
    }

    async fn delete_playlist(&self, id: &str) -> Result<(), StoreError> {
        self.with(|c| c.execute("DELETE FROM playlists WHERE id = ?1", [id]).map(drop))
    }

    async fn list_tracks(&self, root_id: &str) -> Result<Vec<TrackRecord>, StoreError> {
        self.with(|c| c.prepare_cached("SELECT file_id, root_id, relative_path, size_bytes, last_modified_ms, missing FROM tracks WHERE root_id = ?1 ORDER BY rowid")?.query_map([root_id], track_from)?.collect())
    }

    async fn list_playlists(&self, root_id: &str) -> Result<Vec<PlaylistRecord>, StoreError> {
        self.with(|c| c.prepare_cached("SELECT id, root_id, file_id, name, track_file_ids FROM playlists WHERE root_id = ?1 ORDER BY rowid")?.query_map([root_id], playlist_from)?.collect())
    }

    async fn list_all_tracks(&self) -> Result<Vec<TrackRecord>, StoreError> {
        self.with(|c| c.prepare_cached("SELECT file_id, root_id, relative_path, size_bytes, last_modified_ms, missing FROM tracks ORDER BY rowid")?.query_map([], track_from)?.collect())
    }

    async fn list_all_playlists(&self) -> Result<Vec<PlaylistRecord>, StoreError> {
        self.with(|c| c.prepare_cached("SELECT id, root_id, file_id, name, track_file_ids FROM playlists ORDER BY rowid")?.query_map([], playlist_from)?.collect())
    }

    async fn get_analysis(&self, file_id: &str) -> Result<Option<AnalysisResult>, StoreError> {
        self.with(|c| {
            c.query_row("SELECT file_id, normalization_gain, analyzed_at_ms, size_bytes, last_modified_ms, algorithm_version FROM analysis WHERE file_id = ?1", [file_id], |row| {
                Ok(AnalysisResult { file_id: row.get(0)?, normalization_gain: row.get(1)?, analyzed_at_ms: row.get(2)?, size_bytes: row.get::<_, i64>(3)? as u64, last_modified_ms: row.get(4)?, algorithm_version: row.get::<_, i64>(5)? as u32 })
            })
            .optional()
        })
    }

    async fn put_analysis(&self, r: AnalysisResult) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO analysis (file_id, normalization_gain, analyzed_at_ms, size_bytes, last_modified_ms, algorithm_version) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![r.file_id, r.normalization_gain, r.analyzed_at_ms, r.size_bytes as i64, r.last_modified_ms, r.algorithm_version as i64],
            )
            .map(drop)
        })
    }

    async fn get_metadata(&self, file_id: &str) -> Result<Option<TrackMetadata>, StoreError> {
        self.with(|c| {
            c.query_row("SELECT file_id, title, artists, album, duration_seconds, size_bytes, last_modified_ms, parser_version, content_hash FROM metadata WHERE file_id = ?1", [file_id], |row| {
                let artists: String = row.get(2)?;
                Ok(TrackMetadata {
                    file_id: row.get(0)?,
                    title: row.get(1)?,
                    artists: match serde_json::from_str::<Value>(&artists) {
                        Ok(Value::Array(items)) => items.into_iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
                        _ => Vec::new(),
                    },
                    album: row.get(3)?,
                    duration_seconds: row.get(4)?,
                    size_bytes: row.get::<_, i64>(5)? as u64,
                    last_modified_ms: row.get(6)?,
                    parser_version: row.get::<_, i64>(7)? as u32,
                    content_hash: row.get(8)?,
                })
            })
            .optional()
        })
    }

    async fn put_metadata(&self, m: TrackMetadata) -> Result<(), StoreError> {
        let artists = Value::Array(m.artists.iter().cloned().map(Value::String).collect()).to_string();
        self.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO metadata (file_id, title, artists, album, duration_seconds, size_bytes, last_modified_ms, parser_version, content_hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![m.file_id, m.title, artists, m.album, m.duration_seconds, m.size_bytes as i64, m.last_modified_ms, m.parser_version as i64, m.content_hash],
            )
            .map(drop)
        })
    }

    async fn get_cover_art(&self, file_id: &str) -> Result<Option<String>, StoreError> {
        self.with(|c| {
            c.query_row("SELECT mime_type, data FROM cover_art WHERE file_id = ?1", [file_id], |row| {
                let (mime, data): (String, Vec<u8>) = (row.get(0)?, row.get(1)?);
                Ok(format!("data:{mime};base64,{}", encode_base64(&data)))
            })
            .optional()
        })
    }

    async fn put_cover_art(&self, file_id: &str, art: Option<CoverArtBytes>) -> Result<(), StoreError> {
        self.with(|c| match art {
            Some(art) => c.execute("INSERT OR REPLACE INTO cover_art (file_id, mime_type, data) VALUES (?1, ?2, ?3)", params![file_id, art.mime_type, art.data]).map(drop),
            None => c.execute("DELETE FROM cover_art WHERE file_id = ?1", [file_id]).map(drop),
        })
    }

    async fn get_playback_state(&self) -> Result<Option<PlaybackState>, StoreError> {
        let text: Option<String> = self.with(|c| c.query_row("SELECT json FROM playback_state WHERE id = 1", [], |row| row.get(0)).optional())?;
        Ok(text.as_deref().and_then(decode_playback_state))
    }

    async fn put_playback_state(&self, state: PlaybackState) -> Result<(), StoreError> {
        let json = encode_playback_state(&state);
        self.with(|c| c.execute("INSERT OR REPLACE INTO playback_state (id, json) VALUES (1, ?1)", [json]).map(drop))
    }

    async fn get_lyrics_scopes(&self) -> Result<Vec<LyricsScope>, StoreError> {
        self.with(|c| c.prepare_cached("SELECT root_id, relative_path FROM lyrics_scopes ORDER BY rowid")?.query_map([], |row| Ok(LyricsScope { root_id: row.get(0)?, relative_path: row.get(1)? }))?.collect())
    }

    async fn add_lyrics_scope(&self, scope: LyricsScope) -> Result<(), StoreError> {
        self.with(|c| c.execute("INSERT OR IGNORE INTO lyrics_scopes (root_id, relative_path) VALUES (?1, ?2)", params![scope.root_id, scope.relative_path]).map(drop))
    }

    async fn remove_lyrics_scope(&self, root_id: &str, relative_path: &str) -> Result<(), StoreError> {
        self.with(|c| c.execute("DELETE FROM lyrics_scopes WHERE root_id = ?1 AND relative_path = ?2", params![root_id, relative_path]).map(drop))
    }

    async fn get_lyrics_assignment(&self, file_id: &str) -> Result<Option<String>, StoreError> {
        self.with(|c| c.query_row("SELECT lrc_file_id FROM lyrics_assignments WHERE file_id = ?1", [file_id], |row| row.get(0)).optional())
    }

    async fn put_lyrics_assignment(&self, file_id: &str, lrc_file_id: Option<String>) -> Result<(), StoreError> {
        self.with(|c| match lrc_file_id {
            Some(lrc) => c.execute("INSERT OR REPLACE INTO lyrics_assignments (file_id, lrc_file_id) VALUES (?1, ?2)", params![file_id, lrc]).map(drop),
            None => c.execute("DELETE FROM lyrics_assignments WHERE file_id = ?1", [file_id]).map(drop),
        })
    }

    async fn get_setting(&self, key: &str) -> Result<Option<String>, StoreError> {
        self.with(|c| c.query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| row.get(0)).optional())
    }

    async fn put_setting(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.with(|c| c.execute("INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)", params![key, value]).map(drop))
    }
}

#[cfg(test)]
mod tests {
    use beatseam_core::http::block_on;
    use beatseam_core::library_store::MemoryLibraryStore;

    use super::*;

    fn track(id: &str, root: &str, size: u64) -> TrackRecord {
        TrackRecord { file_id: id.into(), root_id: root.into(), relative_path: format!("Music/{id}.mp3"), size_bytes: size, last_modified_ms: 1_700_000_000_123.5, missing: false }
    }

    fn playlist(id: &str, root: &str, ids: &[&str]) -> PlaylistRecord {
        PlaylistRecord { id: id.into(), root_id: root.into(), file_id: id.into(), name: format!("Mix {id}"), track_file_ids: ids.iter().map(|s| s.to_string()).collect() }
    }

    fn state() -> PlaybackState {
        PlaybackState {
            playlist_id: Some("p".into()),
            current_track_file_id: Some("t".into()),
            root_id: Some("r".into()),
            opened_playlist_id: OpenedPlaylist::Playlist("q".into()),
            opened_root_id: Some("r2".into()),
            position_seconds: 12.75,
            loop_mode: LoopMode::One,
            shuffle_enabled: true,
            shuffle_order: Some(vec!["t".into(), "u".into()]),
            volume: 0.4,
            now_playing_open: true,
        }
    }

    /// Everything a store must do, run against both implementations: SQLite has to behave exactly like the in-memory reference.
    async fn conformance<S: LibraryStore>(store: &S) {
        // Tracks: insertion order is kept through an upsert, lists are per root.
        for id in ["a", "b", "c"] {
            store.upsert_track(track(id, "r1", 1)).await.unwrap();
        }
        store.upsert_track(track("x", "r2", 1)).await.unwrap();
        store.upsert_track(TrackRecord { missing: true, ..track("a", "r1", 99) }).await.unwrap();
        let listed = store.list_tracks("r1").await.unwrap();
        assert_eq!(listed.iter().map(|t| (t.file_id.as_str(), t.size_bytes, t.missing)).collect::<Vec<_>>(), [("a", 99, true), ("b", 1, false), ("c", 1, false)]);
        assert_eq!(listed[1].last_modified_ms, 1_700_000_000_123.5);
        assert_eq!(store.list_all_tracks().await.unwrap().len(), 4);
        store.delete_track("b").await.unwrap();
        store.delete_track("never-existed").await.unwrap();
        assert_eq!(store.list_tracks("r1").await.unwrap().len(), 2);
        assert!(store.list_tracks("nope").await.unwrap().is_empty());

        // Playlists keep their track order, duplicates included.
        store.upsert_playlist(playlist("p1", "r1", &["a", "c", "a"])).await.unwrap();
        store.upsert_playlist(playlist("p2", "r2", &[])).await.unwrap();
        store.upsert_playlist(PlaylistRecord { name: "Renamed".into(), ..playlist("p1", "r1", &["c", "a"]) }).await.unwrap();
        let playlists = store.list_playlists("r1").await.unwrap();
        assert_eq!((playlists.len(), playlists[0].name.as_str(), playlists[0].track_file_ids.clone()), (1, "Renamed", vec!["c".to_string(), "a".to_string()]));
        assert_eq!(store.list_all_playlists().await.unwrap().len(), 2);
        store.delete_playlist("p2").await.unwrap();
        assert_eq!(store.list_all_playlists().await.unwrap().len(), 1);

        // Analysis and metadata round trip every field, including optional ones.
        assert_eq!(store.get_analysis("a").await.unwrap(), None);
        let analysis = AnalysisResult { file_id: "a".into(), normalization_gain: 0.8123, analyzed_at_ms: 5.5, size_bytes: 99, last_modified_ms: 1.5, algorithm_version: 2 };
        store.put_analysis(analysis.clone()).await.unwrap();
        store.put_analysis(AnalysisResult { normalization_gain: 0.9, ..analysis.clone() }).await.unwrap();
        assert_eq!(store.get_analysis("a").await.unwrap().map(|a| a.normalization_gain), Some(0.9));
        let full = TrackMetadata { file_id: "a".into(), title: Some("T \"quoted\" é".into()), artists: vec!["A".into(), "B".into()], album: Some("Alb".into()), duration_seconds: Some(200.25), size_bytes: 99, last_modified_ms: 1.5, parser_version: 6, content_hash: Some("abc".into()) };
        let bare = TrackMetadata { file_id: "b".into(), title: None, artists: vec![], album: None, duration_seconds: None, size_bytes: 1, last_modified_ms: 2.0, parser_version: 6, content_hash: None };
        store.put_metadata(full.clone()).await.unwrap();
        store.put_metadata(bare.clone()).await.unwrap();
        assert_eq!(store.get_metadata("a").await.unwrap(), Some(full));
        assert_eq!(store.get_metadata("b").await.unwrap(), Some(bare));
        assert_eq!(store.get_metadata("zzz").await.unwrap(), None);

        // Cover art comes back as a data URI, and None clears it.
        assert_eq!(store.get_cover_art("a").await.unwrap(), None);
        store.put_cover_art("a", Some(CoverArtBytes { mime_type: "image/png".into(), data: vec![1, 2, 3, 250] })).await.unwrap();
        assert_eq!(store.get_cover_art("a").await.unwrap().as_deref(), Some("data:image/png;base64,AQID+g=="));
        store.put_cover_art("a", None).await.unwrap();
        assert_eq!(store.get_cover_art("a").await.unwrap(), None);

        // Playback state, lyrics scopes and assignments, settings.
        assert_eq!(store.get_playback_state().await.unwrap(), None);
        store.put_playback_state(state()).await.unwrap();
        assert_eq!(store.get_playback_state().await.unwrap(), Some(state()));
        let library_screen = PlaybackState { opened_playlist_id: OpenedPlaylist::Library, shuffle_order: None, ..state() };
        store.put_playback_state(library_screen.clone()).await.unwrap();
        assert_eq!(store.get_playback_state().await.unwrap(), Some(library_screen));

        let scope = LyricsScope { root_id: "r".into(), relative_path: "Lyrics".into() };
        store.add_lyrics_scope(scope.clone()).await.unwrap();
        store.add_lyrics_scope(scope.clone()).await.unwrap();
        store.add_lyrics_scope(LyricsScope { root_id: "r".into(), relative_path: String::new() }).await.unwrap();
        assert_eq!(store.get_lyrics_scopes().await.unwrap().len(), 2);
        store.remove_lyrics_scope("r", "Lyrics").await.unwrap();
        assert_eq!(store.get_lyrics_scopes().await.unwrap(), [LyricsScope { root_id: "r".into(), relative_path: String::new() }]);

        assert_eq!(store.get_lyrics_assignment("a").await.unwrap(), None);
        store.put_lyrics_assignment("a", Some("a.lrc".into())).await.unwrap();
        store.put_lyrics_assignment("a", Some("other.lrc".into())).await.unwrap();
        assert_eq!(store.get_lyrics_assignment("a").await.unwrap().as_deref(), Some("other.lrc"));
        store.put_lyrics_assignment("a", None).await.unwrap();
        assert_eq!(store.get_lyrics_assignment("a").await.unwrap(), None);

        assert_eq!(store.get_setting("k").await.unwrap(), None);
        store.put_setting("k", "v1").await.unwrap();
        store.put_setting("k", "v2").await.unwrap();
        store.put_setting("empty", "").await.unwrap();
        assert_eq!((store.get_setting("k").await.unwrap().as_deref(), store.get_setting("empty").await.unwrap().as_deref()), (Some("v2"), Some("")));
    }

    #[test]
    fn the_in_memory_reference_passes_the_conformance_suite() {
        block_on(conformance(&MemoryLibraryStore::new()));
    }

    #[test]
    fn sqlite_behaves_exactly_like_the_reference() {
        block_on(conformance(&SqliteLibraryStore::in_memory().unwrap()));
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("beatseam-native-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("library.db")
    }

    #[test]
    fn data_survives_closing_and_reopening_the_database_file() {
        let path = temp_path("reopen");
        {
            let store = SqliteLibraryStore::open(&path).unwrap();
            block_on(async {
                store.upsert_track(track("a", "r", 5)).await.unwrap();
                store.put_playback_state(state()).await.unwrap();
                store.put_setting("k", "v").await.unwrap();
            });
        }
        let store = SqliteLibraryStore::open(&path).unwrap();
        block_on(async {
            assert_eq!(store.list_all_tracks().await.unwrap().len(), 1);
            assert_eq!(store.get_playback_state().await.unwrap(), Some(state()));
            assert_eq!(store.get_setting("k").await.unwrap().as_deref(), Some("v"));
        });
        drop(store); // Windows will not delete a database file that is still open
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_database_from_a_newer_build_is_refused_not_misread() {
        let path = temp_path("newer");
        Connection::open(&path).unwrap().pragma_update(None, "user_version", 99).unwrap();
        let error = SqliteLibraryStore::open(&path).err().expect("refused");
        assert!(error.0.contains("newer version"), "{error}");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_playback_state_written_by_an_older_or_newer_build_still_reads() {
        let store = SqliteLibraryStore::in_memory().unwrap();
        store.with(|c| c.execute("INSERT INTO playback_state (id, json) VALUES (1, ?1)", [r#"{"playlistId":"p","currentTrackFileId":"t","somethingNew":42}"#]).map(drop)).unwrap();
        let read = block_on(store.get_playback_state()).unwrap().unwrap();
        assert_eq!((read.playlist_id.as_deref(), read.volume, read.opened_playlist_id, read.loop_mode), (Some("p"), 0.25, OpenedPlaylist::Unset, LoopMode::Off));
        store.with(|c| c.execute("UPDATE playback_state SET json = 'not json'", []).map(drop)).unwrap();
        assert_eq!(block_on(store.get_playback_state()).unwrap(), None, "unreadable state means none, not a crash");
    }
}
