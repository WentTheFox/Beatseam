//! Port of `store/settings`: the app's preferences, how they are stored, and how a patch is turned into writes.

use std::collections::HashMap;

use beatseam_core::library_store::{LibraryStore, StoreError};
use beatseam_core::scrobble::{DEFAULT_LASTFM_API_KEY, DEFAULT_LASTFM_API_SECRET};

/// The single source for the app's accent color - the "glaucous" preset's hex and the default selection of the accent color
/// setting.
pub const DEFAULT_ACCENT_COLOR: &str = "#6181b8";
/// "Glaucous" - the actual name of this pale blue-grey hue, and the app's signature color since before this setting existed.
pub const DEFAULT_ACCENT_COLOR_ID: &str = "glaucous";

pub const DEFAULT_VOLUME_NORMALIZATION_ENABLED: bool = true;
pub const DEFAULT_CROSSFADE_SECONDS: f64 = 8.0;
pub const DEFAULT_LYRICS_ENABLED: bool = true;
pub const DEFAULT_SHOW_VOLUME_BUTTON_ON_NOW_PLAYING: bool = true;
pub const DEFAULT_DISCORD_RICH_PRESENCE_ENABLED: bool = true;
pub const MIN_CROSSFADE_SECONDS: f64 = 1.0;
pub const MAX_CROSSFADE_SECONDS: f64 = 20.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeMode {
    #[default]
    Light,
    Flux,
    Dark,
    Amoled,
}

impl ThemeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Flux => "flux",
            Self::Dark => "dark",
            Self::Amoled => "amoled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "light" => Some(Self::Light),
            "flux" => Some(Self::Flux),
            "dark" => Some(Self::Dark),
            "amoled" => Some(Self::Amoled),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppSettings {
    pub theme_mode: ThemeMode,
    /// An accent option id (see [`ACCENT_COLOR_OPTIONS`]) - NOT a hex value. Storing the id rather than the hex it currently
    /// resolves to means a future retune of the palette's saturation/lightness formula reflows every existing install's choice
    /// along with it, instead of leaving already-saved settings pointing at a shade the picker itself no longer offers.
    pub accent_color: String,
    pub volume_normalization_enabled: bool,
    pub crossfade_seconds: f64,
    /// Hides the Now Playing screen's lyrics panel entirely (and skips fetching lyrics for the current track) to free up
    /// vertical space for people who don't use lyrics.
    pub lyrics_enabled: bool,
    /// Hides the volume button on the Now Playing screen - the volume slider in Settings is always shown regardless of this, so
    /// volume stays reachable even with it off.
    pub show_volume_button_on_now_playing: bool,
    /// A Last.fm API app's key/secret - bring-your-own, not a key baked into the app itself. Only these two are directly
    /// user-edited; the session key and username are written by the connection flow, never typed in directly.
    pub last_fm_api_key: String,
    pub last_fm_api_secret: String,
    /// `None` until the connection flow completes. Clearing this (disconnect) is what actually turns scrobbling off - the api
    /// key/secret alone don't scrobble anything without a session.
    pub last_fm_session_key: Option<String>,
    /// Display-only - the Last.fm username the session key authenticates as, so Settings can show "Connected as X" without an
    /// extra API round trip.
    pub last_fm_username: Option<String>,
    /// Show the current track as a Discord Rich Presence status. No credential to enter (a Discord application id isn't
    /// confidential), just this on/off toggle. Only takes effect where a presence bridge actually exists.
    pub discord_rich_presence_enabled: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            // The real first-run default is the system color scheme, which only the UI knows - hydration supplies it.
            theme_mode: ThemeMode::Light,
            accent_color: DEFAULT_ACCENT_COLOR_ID.into(),
            volume_normalization_enabled: DEFAULT_VOLUME_NORMALIZATION_ENABLED,
            crossfade_seconds: DEFAULT_CROSSFADE_SECONDS,
            lyrics_enabled: DEFAULT_LYRICS_ENABLED,
            show_volume_button_on_now_playing: DEFAULT_SHOW_VOLUME_BUTTON_ON_NOW_PLAYING,
            last_fm_api_key: DEFAULT_LASTFM_API_KEY.into(),
            last_fm_api_secret: DEFAULT_LASTFM_API_SECRET.into(),
            last_fm_session_key: None,
            last_fm_username: None,
            discord_rich_presence_enabled: DEFAULT_DISCORD_RICH_PRESENCE_ENABLED,
        }
    }
}

/// Preset swatches for the accent color picker - a full color picker is more control than a single-hue accent tint needs.
/// Spread evenly around the hue wheel at 15 degree steps (a consistent 45%/56% saturation/lightness) so the row reads as a full
/// rainbow rather than a handful of arbitrarily chosen colors. The default (every install's accent before this setting
/// existed) is kept as an exact value in its place on the wheel (~218 degrees) rather than nudged onto the nearest 15 degree
/// step, replacing the generated swatch that would otherwise have landed one step away from it - two near-identical blues one
/// step apart would be a pointless choice to offer.
pub const ACCENT_COLOR_OPTIONS: [(&str, &str); 24] = [
    ("red", "#c15c5c"),
    ("vermilion", "#c1765c"),
    ("orange", "#c18f5c"),
    ("amber", "#c1a85c"),
    ("yellow", "#c1c15c"),
    ("lime", "#a8c15c"),
    ("chartreuse", "#8fc15c"),
    ("springGreen", "#76c15c"),
    ("green", "#5cc15c"),
    ("emerald", "#5cc176"),
    ("teal", "#5cc18f"),
    ("turquoise", "#5cc1a8"),
    ("cyan", "#5cc1c1"),
    ("sky", "#5ca8c1"),
    ("azure", "#5c8fc1"),
    (DEFAULT_ACCENT_COLOR_ID, DEFAULT_ACCENT_COLOR),
    ("indigo", "#5c5cc1"),
    ("violet", "#765cc1"),
    ("purple", "#8f5cc1"),
    ("orchid", "#a85cc1"),
    ("magenta", "#c15cc1"),
    ("fuchsia", "#c15ca8"),
    ("rose", "#c15c8f"),
    ("crimson", "#c15c76"),
];

/// Resolves an accent id to the hex value it currently means - falls back to the default for an id that's missing,
/// unrecognized, or (pre-migration) still a raw hex string, so a stale/corrupt value never breaks rendering.
pub fn get_accent_color_hex(id: &str) -> &'static str {
    ACCENT_COLOR_OPTIONS.iter().find(|(i, _)| *i == id).map_or(DEFAULT_ACCENT_COLOR, |(_, hex)| hex)
}

/// Accepts a known accent id as-is. A value that isn't one - either unrecognized, or (pre-migration) a raw hex string from
/// before the accent became an id - maps to the closest known id by hex match, or the default if even that fails, so an old
/// install's saved color choice still resolves to a real swatch instead of silently falling back to the default on every
/// launch.
pub fn normalize_accent_color_id(value: &str) -> String {
    if ACCENT_COLOR_OPTIONS.iter().any(|(id, _)| *id == value) {
        return value.to_owned();
    }
    ACCENT_COLOR_OPTIONS.iter().find(|(_, hex)| hex.eq_ignore_ascii_case(value)).map_or(DEFAULT_ACCENT_COLOR_ID, |(id, _)| id).to_owned()
}

pub const THEME_MODE_SETTING_KEY: &str = "settings.themeMode";
pub const ACCENT_COLOR_SETTING_KEY: &str = "settings.accentColor";
pub const VOLUME_NORMALIZATION_SETTING_KEY: &str = "settings.volumeNormalizationEnabled";
pub const CROSSFADE_SECONDS_SETTING_KEY: &str = "settings.crossfadeSeconds";
pub const LYRICS_ENABLED_SETTING_KEY: &str = "settings.lyricsEnabled";
pub const SHOW_VOLUME_BUTTON_ON_NOW_PLAYING_SETTING_KEY: &str = "settings.showVolumeButtonOnNowPlaying";
pub const LASTFM_API_KEY_SETTING_KEY: &str = "settings.lastFmApiKey";
pub const LASTFM_API_SECRET_SETTING_KEY: &str = "settings.lastFmApiSecret";
pub const LASTFM_SESSION_KEY_SETTING_KEY: &str = "settings.lastFmSessionKey";
pub const LASTFM_USERNAME_SETTING_KEY: &str = "settings.lastFmUsername";
pub const DISCORD_RICH_PRESENCE_ENABLED_SETTING_KEY: &str = "settings.discordRichPresenceEnabled";

pub const ALL_SETTING_KEYS: [&str; 11] = [
    THEME_MODE_SETTING_KEY,
    ACCENT_COLOR_SETTING_KEY,
    VOLUME_NORMALIZATION_SETTING_KEY,
    CROSSFADE_SECONDS_SETTING_KEY,
    LYRICS_ENABLED_SETTING_KEY,
    SHOW_VOLUME_BUTTON_ON_NOW_PLAYING_SETTING_KEY,
    LASTFM_API_KEY_SETTING_KEY,
    LASTFM_API_SECRET_SETTING_KEY,
    LASTFM_SESSION_KEY_SETTING_KEY,
    LASTFM_USERNAME_SETTING_KEY,
    DISCORD_RICH_PRESENCE_ENABLED_SETTING_KEY,
];

/// The preferences that follow the account to every device. Anything else stays on the device: Last.fm and Discord are
/// per-device integrations (their credentials must never travel), and this list is also what the app is willing to accept
/// from a server, so a server can't write arbitrary local settings.
pub const SYNCED_SETTING_KEYS: [&str; 6] = [
    THEME_MODE_SETTING_KEY,
    ACCENT_COLOR_SETTING_KEY,
    VOLUME_NORMALIZATION_SETTING_KEY,
    CROSSFADE_SECONDS_SETTING_KEY,
    LYRICS_ENABLED_SETTING_KEY,
    SHOW_VOLUME_BUTTON_ON_NOW_PLAYING_SETTING_KEY,
];

/// Raw string values (or `None` if never stored) read back from the library store, keyed by storage key.
pub type StoredSettings = HashMap<String, Option<String>>;

/// Overlays whatever was stored on `base`; a key never stored keeps `base`'s value. (An empty stored string is "cleared" for the
/// two nullable Last.fm fields, which were written as `''` because the store has no delete.)
pub fn parse_stored_settings(stored: &StoredSettings, base: &AppSettings) -> AppSettings {
    let get = |key: &str| stored.get(key).and_then(|v| v.as_deref());
    // JavaScript's `value ?? base`: a stored empty string is still a value.
    let flag = |key: &str, base: bool| get(key).map_or(base, |v| v == "1");
    let nullable = |key: &str, base: &Option<String>| match get(key) {
        Some(v) if !v.is_empty() => Some(v.to_owned()),
        Some(_) => None,
        None => base.clone(),
    };
    AppSettings {
        theme_mode: get(THEME_MODE_SETTING_KEY).and_then(ThemeMode::parse).unwrap_or(base.theme_mode),
        accent_color: get(ACCENT_COLOR_SETTING_KEY).filter(|v| !v.is_empty()).map_or_else(|| base.accent_color.clone(), normalize_accent_color_id),
        volume_normalization_enabled: flag(VOLUME_NORMALIZATION_SETTING_KEY, base.volume_normalization_enabled),
        // `Number(s)` of a non-numeric string is NaN, which is falsy-checked nowhere in the TypeScript: it would have stored NaN.
        // Falling back to the base value instead can only be an improvement.
        crossfade_seconds: get(CROSSFADE_SECONDS_SETTING_KEY).filter(|v| !v.is_empty()).and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite()).unwrap_or(base.crossfade_seconds),
        lyrics_enabled: flag(LYRICS_ENABLED_SETTING_KEY, base.lyrics_enabled),
        show_volume_button_on_now_playing: flag(SHOW_VOLUME_BUTTON_ON_NOW_PLAYING_SETTING_KEY, base.show_volume_button_on_now_playing),
        last_fm_api_key: get(LASTFM_API_KEY_SETTING_KEY).map_or_else(|| base.last_fm_api_key.clone(), str::to_owned),
        last_fm_api_secret: get(LASTFM_API_SECRET_SETTING_KEY).map_or_else(|| base.last_fm_api_secret.clone(), str::to_owned),
        last_fm_session_key: nullable(LASTFM_SESSION_KEY_SETTING_KEY, &base.last_fm_session_key),
        last_fm_username: nullable(LASTFM_USERNAME_SETTING_KEY, &base.last_fm_username),
        discord_rich_presence_enabled: flag(DISCORD_RICH_PRESENCE_ENABLED_SETTING_KEY, base.discord_rich_presence_enabled),
    }
}

/// A change to some of the settings: only the `Some` fields are set. The two nullable Last.fm fields are `Option<Option<..>>` so a
/// patch can clear them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SettingsPatch {
    pub theme_mode: Option<ThemeMode>,
    pub accent_color: Option<String>,
    pub volume_normalization_enabled: Option<bool>,
    pub crossfade_seconds: Option<f64>,
    pub lyrics_enabled: Option<bool>,
    pub show_volume_button_on_now_playing: Option<bool>,
    pub last_fm_api_key: Option<String>,
    pub last_fm_api_secret: Option<String>,
    pub last_fm_session_key: Option<Option<String>>,
    pub last_fm_username: Option<Option<String>>,
    pub discord_rich_presence_enabled: Option<bool>,
}

impl SettingsPatch {
    pub fn apply_to(&self, settings: &mut AppSettings) {
        if let Some(v) = self.theme_mode {
            settings.theme_mode = v;
        }
        if let Some(v) = &self.accent_color {
            settings.accent_color = v.clone();
        }
        if let Some(v) = self.volume_normalization_enabled {
            settings.volume_normalization_enabled = v;
        }
        if let Some(v) = self.crossfade_seconds {
            settings.crossfade_seconds = v;
        }
        if let Some(v) = self.lyrics_enabled {
            settings.lyrics_enabled = v;
        }
        if let Some(v) = self.show_volume_button_on_now_playing {
            settings.show_volume_button_on_now_playing = v;
        }
        if let Some(v) = &self.last_fm_api_key {
            settings.last_fm_api_key = v.clone();
        }
        if let Some(v) = &self.last_fm_api_secret {
            settings.last_fm_api_secret = v.clone();
        }
        if let Some(v) = &self.last_fm_session_key {
            settings.last_fm_session_key = v.clone();
        }
        if let Some(v) = &self.last_fm_username {
            settings.last_fm_username = v.clone();
        }
        if let Some(v) = self.discord_rich_presence_enabled {
            settings.discord_rich_presence_enabled = v;
        }
    }

    /// The [storage key, serialized value] writes this patch implies - only the fields it actually sets.
    pub fn serialize(&self) -> Vec<(&'static str, String)> {
        let on_off = |b: bool| if b { "1" } else { "0" }.to_owned();
        let mut writes = Vec::new();
        if let Some(v) = self.theme_mode {
            writes.push((THEME_MODE_SETTING_KEY, v.as_str().to_owned()));
        }
        if let Some(v) = &self.accent_color {
            writes.push((ACCENT_COLOR_SETTING_KEY, v.clone()));
        }
        if let Some(v) = self.volume_normalization_enabled {
            writes.push((VOLUME_NORMALIZATION_SETTING_KEY, on_off(v)));
        }
        if let Some(v) = self.crossfade_seconds {
            writes.push((CROSSFADE_SECONDS_SETTING_KEY, beatseam_core::js_compat::js_number(v)));
        }
        if let Some(v) = self.lyrics_enabled {
            writes.push((LYRICS_ENABLED_SETTING_KEY, on_off(v)));
        }
        if let Some(v) = self.show_volume_button_on_now_playing {
            writes.push((SHOW_VOLUME_BUTTON_ON_NOW_PLAYING_SETTING_KEY, on_off(v)));
        }
        if let Some(v) = &self.last_fm_api_key {
            writes.push((LASTFM_API_KEY_SETTING_KEY, v.clone()));
        }
        if let Some(v) = &self.last_fm_api_secret {
            writes.push((LASTFM_API_SECRET_SETTING_KEY, v.clone()));
        }
        if let Some(v) = &self.last_fm_session_key {
            writes.push((LASTFM_SESSION_KEY_SETTING_KEY, v.clone().unwrap_or_default()));
        }
        if let Some(v) = &self.last_fm_username {
            writes.push((LASTFM_USERNAME_SETTING_KEY, v.clone().unwrap_or_default()));
        }
        if let Some(v) = self.discord_rich_presence_enabled {
            writes.push((DISCORD_RICH_PRESENCE_ENABLED_SETTING_KEY, on_off(v)));
        }
        writes
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SettingsState {
    pub values: AppSettings,
    /// False until the initial read from the library store resolves - `values` holds defaults until then.
    pub loaded: bool,
}

impl Default for SettingsState {
    fn default() -> Self {
        Self { values: AppSettings::default(), loaded: false }
    }
}

impl SettingsState {
    /// Initial read from storage finished. Not persisted back (it came from storage). `system_theme_mode` is the first-run theme
    /// default (nothing stored yet).
    pub fn loaded(&mut self, stored: &StoredSettings, system_theme_mode: ThemeMode) {
        self.values = parse_stored_settings(stored, &AppSettings { theme_mode: system_theme_mode, ..self.values.clone() });
        self.loaded = true;
    }

    /// A user/auth-flow change. The caller writes the patched keys through the library store (see [`persist_patch`]).
    pub fn updated(&mut self, patch: &SettingsPatch) {
        patch.apply_to(&mut self.values);
    }
}

/// Reads every persisted setting.
pub async fn read_stored_settings<S: LibraryStore>(store: &S) -> Result<StoredSettings, StoreError> {
    let mut stored = StoredSettings::new();
    for key in ALL_SETTING_KEYS {
        stored.insert(key.to_owned(), store.get_setting(key).await?);
    }
    Ok(stored)
}

/// Writes a patch's keys (the settings persistence effect). Returns the keys that follow the account to other devices, with
/// their new values, for the caller to queue as `setting.set` mutations when signed in to a server.
pub async fn persist_patch<S: LibraryStore>(store: &S, patch: &SettingsPatch) -> Result<Vec<(&'static str, String)>, StoreError> {
    let writes = patch.serialize();
    for (key, value) in &writes {
        store.put_setting(key, value).await?;
    }
    Ok(writes.into_iter().filter(|(key, _)| SYNCED_SETTING_KEYS.contains(key)).collect())
}

/// What "Reset settings" changes. Deliberately leaves the Last.fm key/secret/session/username untouched - Last.fm's connection is a
/// separate integration credential, not an appearance/playback preference, and the reset-confirm dialog only promises to reset
/// the fields listed here. Disconnecting Last.fm has its own explicit action instead.
pub fn reset_patch(system_theme_mode: ThemeMode) -> SettingsPatch {
    SettingsPatch {
        theme_mode: Some(system_theme_mode),
        accent_color: Some(DEFAULT_ACCENT_COLOR_ID.into()),
        volume_normalization_enabled: Some(DEFAULT_VOLUME_NORMALIZATION_ENABLED),
        crossfade_seconds: Some(DEFAULT_CROSSFADE_SECONDS),
        lyrics_enabled: Some(DEFAULT_LYRICS_ENABLED),
        show_volume_button_on_now_playing: Some(DEFAULT_SHOW_VOLUME_BUTTON_ON_NOW_PLAYING),
        discord_rich_presence_enabled: Some(DEFAULT_DISCORD_RICH_PRESENCE_ENABLED),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use beatseam_core::http::block_on;
    use beatseam_core::library_store::MemoryLibraryStore;

    use super::*;

    fn stored(pairs: &[(&str, &str)]) -> StoredSettings {
        pairs.iter().map(|(k, v)| (k.to_string(), Some(v.to_string()))).collect()
    }

    #[test]
    fn the_synced_list_is_exactly_what_the_server_crate_accepts() {
        assert_eq!(SYNCED_SETTING_KEYS, beatseam_server_mode::SYNCED_SETTING_KEYS);
    }

    #[test]
    fn defaults_match_the_typescript() {
        let d = AppSettings::default();
        assert_eq!((d.crossfade_seconds, d.accent_color.as_str(), d.lyrics_enabled, d.discord_rich_presence_enabled, d.last_fm_session_key), (8.0, "glaucous", true, true, None));
        assert_eq!(d.last_fm_api_key, DEFAULT_LASTFM_API_KEY);
    }

    #[test]
    fn nothing_stored_keeps_the_base_and_hydration_takes_the_system_theme() {
        let mut state = SettingsState::default();
        state.loaded(&StoredSettings::new(), ThemeMode::Dark);
        assert!(state.loaded);
        assert_eq!(state.values, AppSettings { theme_mode: ThemeMode::Dark, ..Default::default() });
    }

    #[test]
    fn stored_values_overlay_the_base_with_the_typescripts_parsing_rules() {
        let values = parse_stored_settings(
            &stored(&[
                ("settings.themeMode", "amoled"),
                ("settings.accentColor", "#C15C5C"), // a pre-migration raw hex maps to its id, case-insensitively
                ("settings.volumeNormalizationEnabled", "0"),
                ("settings.crossfadeSeconds", "12.5"),
                ("settings.lyricsEnabled", "0"),
                ("settings.showVolumeButtonOnNowPlaying", "1"),
                ("settings.lastFmApiKey", "K"),
                ("settings.lastFmSessionKey", "S"),
                ("settings.lastFmUsername", ""), // cleared
                ("settings.discordRichPresenceEnabled", "0"),
            ]),
            &AppSettings { last_fm_username: Some("old".into()), ..Default::default() },
        );
        assert_eq!((values.theme_mode, values.accent_color.as_str(), values.volume_normalization_enabled, values.crossfade_seconds, values.lyrics_enabled), (ThemeMode::Amoled, "red", false, 12.5, false));
        assert_eq!((values.last_fm_api_key.as_str(), values.last_fm_session_key.as_deref(), values.last_fm_username, values.discord_rich_presence_enabled), ("K", Some("S"), None, false));
    }

    #[test]
    fn unrecognized_values_fall_back_to_the_base() {
        let values = parse_stored_settings(&stored(&[("settings.themeMode", "neon"), ("settings.accentColor", "not-a-color"), ("settings.crossfadeSeconds", "abc")]), &AppSettings::default());
        assert_eq!((values.theme_mode, values.accent_color.as_str(), values.crossfade_seconds), (ThemeMode::Light, "glaucous", 8.0));
    }

    #[test]
    fn accent_ids_resolve_to_hex_with_a_default_fallback() {
        assert_eq!(get_accent_color_hex("rose"), "#c15c8f");
        assert_eq!(get_accent_color_hex("#nonsense"), DEFAULT_ACCENT_COLOR);
        assert_eq!(normalize_accent_color_id("indigo"), "indigo");
        assert_eq!(normalize_accent_color_id("#6181B8"), "glaucous");
    }

    #[test]
    fn a_patch_updates_only_what_it_sets_and_serializes_only_those_keys() {
        let mut state = SettingsState::default();
        let patch = SettingsPatch { lyrics_enabled: Some(false), crossfade_seconds: Some(6.5), last_fm_session_key: Some(None), ..Default::default() };
        state.updated(&patch);
        assert_eq!((state.values.lyrics_enabled, state.values.crossfade_seconds, state.values.theme_mode), (false, 6.5, ThemeMode::Light));
        assert_eq!(patch.serialize(), [("settings.crossfadeSeconds", "6.5".to_owned()), ("settings.lyricsEnabled", "0".to_owned()), ("settings.lastFmSessionKey", String::new())]);
        assert!(SettingsPatch::default().serialize().is_empty());
    }

    #[test]
    fn persisting_writes_every_key_and_reports_only_the_account_wide_ones_for_syncing() {
        let store = MemoryLibraryStore::new();
        let patch = SettingsPatch { theme_mode: Some(ThemeMode::Flux), last_fm_api_key: Some("k".into()), volume_normalization_enabled: Some(false), ..Default::default() };
        let synced = block_on(persist_patch(&store, &patch)).unwrap();
        assert_eq!(synced, [("settings.themeMode", "flux".to_owned()), ("settings.volumeNormalizationEnabled", "0".to_owned())]);
        assert_eq!(block_on(store.get_setting("settings.lastFmApiKey")).unwrap().as_deref(), Some("k"));
        let read = block_on(read_stored_settings(&store)).unwrap();
        assert_eq!(parse_stored_settings(&read, &AppSettings::default()).theme_mode, ThemeMode::Flux);
    }

    #[test]
    fn reset_leaves_the_last_fm_connection_alone() {
        let patch = reset_patch(ThemeMode::Dark);
        let mut settings = AppSettings { last_fm_session_key: Some("s".into()), last_fm_username: Some("u".into()), lyrics_enabled: false, theme_mode: ThemeMode::Amoled, ..Default::default() };
        patch.apply_to(&mut settings);
        assert_eq!((settings.theme_mode, settings.lyrics_enabled, settings.last_fm_session_key.as_deref(), settings.last_fm_username.as_deref()), (ThemeMode::Dark, true, Some("s"), Some("u")));
        assert!(patch.serialize().iter().all(|(k, _)| !k.contains("lastFm")));
    }
}
