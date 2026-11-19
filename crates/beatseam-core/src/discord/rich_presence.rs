//! Discord Rich Presence - the same SET_ACTIVITY JSON-frame protocol Discord has used for its desktop named-pipe IPC for
//! years, also reachable on Android via a bound-service Binder connection to the real Discord app - this frame builder
//! is deliberately transport-agnostic so every platform (a named pipe on Windows/Linux/macOS, the Binder service on
//! Android) reuses it unchanged.
//!
//! No client secret or per-user auth involved - unlike Last.fm's api_key, a Discord application id is not confidential
//! (it's the same id already public in that app's invite links/OAuth redirects), so Beatseam bakes in its own by default
//! the same way it does for Last.fm.

/// Beatseam's own registered Discord application - see the module docs for why sharing one publicly-known id across
/// installs is fine.
pub const DEFAULT_DISCORD_APPLICATION_ID: &str = "1551930093159587941";

/// The platform's Discord connection. Every method resolves to a bool rather than failing: a missing Discord app is an
/// expected, common case, not an error worth surfacing to the user.
pub trait DiscordPresenceBridge {
    /// Connects to Discord's RPC service and performs the handshake. `false` if Discord isn't installed/reachable or
    /// refuses the handshake.
    fn connect(&self, application_id: &str) -> impl Future<Output = bool>;
    /// Sends a raw JSON RPC frame (see [`build_set_activity_frame`]). `false` if there's no active connection.
    fn send_frame(&self, frame: &str) -> impl Future<Output = bool>;
    fn disconnect(&self) -> impl Future<Output = ()>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiscordNowPlayingInfo {
    pub title: String,
    pub artist: String,
    /// When this play instance started, so Discord can show a live-counting elapsed time rather than a static string.
    pub started_at_unix_ms: i64,
    /// Only usable if it's a real http(s) URL Discord's own servers can fetch - [`build_set_activity_frame`] ignores
    /// anything else, because local cover art is always a `data:`/`content://`/`blob:` URI, and it was confirmed live
    /// that a `data:` URI is silently rejected by Discord (renders as a blank circle, not the image, not even the
    /// "unknown asset key" placeholder). Real per-track art here needs a publicly-reachable self-hosted Beatseam
    /// server URL, which isn't wired up yet - always `None` until that exists.
    pub album_art_url: Option<String>,
}

/// Makes the unique nonce every frame carries: `beatseam-<unix ms>-<counter>`.
#[derive(Debug, Default)]
pub struct NonceGenerator {
    counter: u64,
}

impl NonceGenerator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn next(&mut self, now_unix_ms: i64) -> String {
        self.counter += 1;
        format!("beatseam-{now_unix_ms}-{}", self.counter)
    }
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).expect("a string always serializes")
}

/// Builds a SET_ACTIVITY frame - `info: None` clears the activity (shows nothing in the user's Discord status) rather
/// than leaving a stale "still playing" status up after playback stops. `nonce` must be unique per frame (see
/// [`NonceGenerator`]).
///
/// `type: 2` ("Listening to Beatseam, details/state below") - RPC clients are restricted to Playing(0)/Listening(2)/
/// Watching(3)/Competing(5) per Discord's own RPC docs, and Listening is the only one of those that actually fits a
/// music player.
///
/// The field order is the TypeScript's `JSON.stringify` order, so frames are byte-identical.
pub fn build_set_activity_frame(info: Option<&DiscordNowPlayingInfo>, nonce: &str) -> String {
    // See `album_art_url`'s doc - only a real http(s) URL is usable; anything else (data:/content:///blob:, which is
    // what local cover art always is today) is treated the same as having no art at all, since Discord can't fetch it
    // and silently shows a blank circle rather than falling back on its own. The check is the TypeScript's
    // `startsWith('http')`, deliberately not stricter.
    let activity = match info {
        None => "null".to_string(),
        Some(info) => {
            let assets = match info.album_art_url.as_deref().filter(|u| u.starts_with("http")) {
                // No `assets` at all when there's no real art, rather than referencing a Rich Presence Assets key -
                // Discord's own asset-key resolution can 404 on a freshly-uploaded, correctly-keyed asset, whereas
                // omitting `assets` entirely reliably falls back to the application's own registered icon.
                None => String::new(),
                Some(url) => format!(r#","assets":{{"large_image":{},"large_text":{}}}"#, json_string(url), json_string(&info.title)),
            };
            format!(
                // `name` is required: without it Discord shows the generic "Listening to Game" instead of
                // "Listening to Beatseam".
                r#"{{"type":2,"name":"Beatseam","details":{},"state":{},"timestamps":{{"start":{}}}{assets}}}"#,
                json_string(&info.title),
                json_string(&info.artist),
                info.started_at_unix_ms,
            )
        }
    };
    format!(r#"{{"cmd":"SET_ACTIVITY","nonce":{},"args":{{"pid":0,"activity":{activity}}}}}"#, json_string(nonce))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn info(art: Option<&str>) -> DiscordNowPlayingInfo {
        DiscordNowPlayingInfo { title: "Track Title".into(), artist: "Some Artist".into(), started_at_unix_ms: 1_700_000_000_000, album_art_url: art.map(Into::into) }
    }

    fn frame(info: Option<&DiscordNowPlayingInfo>) -> Value {
        serde_json::from_str(&build_set_activity_frame(info, "n")).unwrap()
    }

    #[test]
    fn omits_assets_entirely_when_there_is_no_album_art() {
        let f = frame(Some(&info(None)));
        assert_eq!(f["cmd"], "SET_ACTIVITY");
        assert_eq!(f["args"]["activity"], json!({"type": 2, "name": "Beatseam", "details": "Track Title", "state": "Some Artist", "timestamps": {"start": 1_700_000_000_000i64}}));
        assert!(f["args"]["activity"].get("assets").is_none());
    }

    #[test]
    fn includes_album_art_as_large_image_when_a_real_http_url_is_given() {
        let f = frame(Some(&info(Some("https://beatseam.went.tf/art/abc.jpg"))));
        assert_eq!(f["args"]["activity"]["assets"], json!({"large_image": "https://beatseam.went.tf/art/abc.jpg", "large_text": "Track Title"}));
    }

    #[test]
    fn omits_assets_for_a_data_uri_which_discord_silently_rejects() {
        assert!(frame(Some(&info(Some("data:image/jpeg;base64,abc"))))["args"]["activity"].get("assets").is_none());
        assert!(frame(Some(&info(Some("content://x/y"))))["args"]["activity"].get("assets").is_none());
    }

    #[test]
    fn clears_the_activity_when_given_none() {
        let f = frame(None);
        assert_eq!(f["cmd"], "SET_ACTIVITY");
        assert!(f["args"]["activity"].is_null());
        assert_eq!(f["args"]["pid"], 0);
    }

    #[test]
    fn gives_each_frame_a_unique_nonce() {
        let mut nonces = NonceGenerator::new();
        let (a, b) = (nonces.next(1_700_000_000_123), nonces.next(1_700_000_000_123));
        assert_eq!((a.as_str(), b.as_str()), ("beatseam-1700000000123-1", "beatseam-1700000000123-2"));
        assert_ne!(frame_nonce(&a), frame_nonce(&b));
    }

    fn frame_nonce(nonce: &str) -> String {
        serde_json::from_str::<Value>(&build_set_activity_frame(None, nonce)).unwrap()["nonce"].as_str().unwrap().to_owned()
    }

    #[test]
    fn strings_are_escaped_like_json_stringify() {
        let tricky = DiscordNowPlayingInfo { title: "Qu\"ote \\ \n é😀".into(), artist: "\u{1}".into(), ..info(None) };
        let f = frame(Some(&tricky));
        assert_eq!(f["args"]["activity"]["details"], "Qu\"ote \\ \n é😀");
        assert_eq!(f["args"]["activity"]["state"], "\u{1}");
    }
}
