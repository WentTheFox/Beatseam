use std::fmt;

use serde_json::{Map, Value};

use super::md5::md5;
use crate::http::{HttpRequest, HttpTransport, Method, TransportError, encode_uri_component, form_urlencode};

const API_ROOT: &str = "https://ws.audioscrobbler.com/2.0/";

/// Beatseam's own registered Last.fm API application (last.fm/api/account/create) - shared across every install
/// rather than making each person register their own app, the same way GNOME's own Last.fm integration ships one
/// baked-in credential for every user rather than a per-user app registration. The app-level key/secret pair only
/// identifies the app for request signing - it never grants access to any specific person's account without that
/// person separately logging into their own Last.fm session and approving Beatseam there, so sharing one across
/// installs doesn't expose any individual user's data. A self-hoster/fork who'd rather use their own app identity can
/// override both in settings. (These are the same public values the TypeScript app ships.)
pub const DEFAULT_LASTFM_API_KEY: &str = "f56d82f8fe857197c4c2d1bd4d3fba49";
pub const DEFAULT_LASTFM_API_SECRET: &str = "cc0e42388b8a8232cab10e13e828c982";

/// Last.fm scrobbling rule: a track under this length is never eligible, regardless of how much of it played.
pub const MIN_SCROBBLE_TRACK_SECONDS: f64 = 30.0;
/// Last.fm scrobbling rule: scrobble once playback reaches this fraction of the track's duration...
pub const SCROBBLE_THRESHOLD_FRACTION: f64 = 0.5;
/// ...or this many seconds in, whichever comes first.
pub const SCROBBLE_THRESHOLD_MAX_SECONDS: f64 = 4.0 * 60.0;

/// An error Last.fm (or this module) reports: the service refused the call (`code` is Last.fm's error number, so
/// retrying the same call would fail the same way) or the response was unusable (`code` is `None`).
#[derive(Clone, Debug, PartialEq)]
pub struct LastFmError {
    pub message: String,
    pub code: Option<i64>,
}

impl fmt::Display for LastFmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LastFmError {}

impl LastFmError {
    fn new(message: impl Into<String>, code: Option<i64>) -> Self {
        Self { message: message.into(), code }
    }
}

/// Why a Last.fm call did not succeed: the service answered with an error, or the request never got an answer (retry
/// later could work).
#[derive(Clone, Debug, PartialEq)]
pub enum LastFmCallError {
    LastFm(LastFmError),
    Transport(TransportError),
}

impl fmt::Display for LastFmCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LastFm(e) => e.fmt(f),
            Self::Transport(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for LastFmCallError {}

impl From<LastFmError> for LastFmCallError {
    fn from(e: LastFmError) -> Self {
        Self::LastFm(e)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastFmCredentials {
    pub api_key: String,
    pub api_secret: String,
}

impl Default for LastFmCredentials {
    fn default() -> Self {
        Self { api_key: DEFAULT_LASTFM_API_KEY.into(), api_secret: DEFAULT_LASTFM_API_SECRET.into() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastFmSession {
    pub session_key: String,
    pub username: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScrobbleEntry {
    pub artist: String,
    pub track: String,
    /// Unix seconds the track STARTED playing - not "now" - per Last.fm's `track.scrobble` timestamp semantics.
    pub timestamp: i64,
    pub album: Option<String>,
    pub duration_seconds: Option<f64>,
}

/// Params sorted and concatenated per Last.fm's signing scheme (`format` and `callback` excluded, everything else
/// included) with the shared secret appended, then MD5'd - see https://www.last.fm/api/authspec#8.
fn build_api_sig(params: &[(String, String)], api_secret: &str) -> String {
    let mut included: Vec<&(String, String)> = params.iter().filter(|(k, _)| k != "format" && k != "callback").collect();
    included.sort_by(|a, b| a.0.cmp(&b.0));
    let concatenated: String = included.iter().map(|(k, v)| format!("{k}{v}")).collect();
    md5(&(concatenated + api_secret))
}

/// `Math.round(x)` as the integer Last.fm's `duration` parameter is written as.
fn js_round(x: f64) -> i64 {
    crate::js_compat::js_round(x) as i64
}

/// A Last.fm client over whatever [`HttpTransport`] the caller supplies.
pub struct LastFmClient<T: HttpTransport> {
    pub credentials: LastFmCredentials,
    pub transport: T,
}

impl<T: HttpTransport> LastFmClient<T> {
    pub fn new(credentials: LastFmCredentials, transport: T) -> Self {
        Self { credentials, transport }
    }

    async fn call_api(&self, method: Method, params: Vec<(String, String)>, sign: bool) -> Result<Map<String, Value>, LastFmCallError> {
        let mut full = params;
        full.push(("api_key".into(), self.credentials.api_key.clone()));
        if sign {
            let sig = build_api_sig(&full, &self.credentials.api_secret);
            full.push(("api_sig".into(), sig));
        }
        full.push(("format".into(), "json".into()));

        let body = form_urlencode(&full);
        let request = match method {
            Method::Get => HttpRequest::new(method, format!("{API_ROOT}?{body}")),
            _ => HttpRequest {
                method,
                url: API_ROOT.into(),
                headers: vec![("Content-Type".into(), "application/x-www-form-urlencoded".into())],
                body: Some(body),
                timeout_ms: None,
            },
        };
        let response = self.transport.send(request).await.map_err(LastFmCallError::Transport)?;

        // `response.json().catch(() => ({}))`: anything that isn't a JSON object reads as an empty one.
        let json = match serde_json::from_slice::<Value>(&response.body) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        };
        if let Some(code) = json.get("error").and_then(Value::as_f64) {
            let message = json.get("message").and_then(Value::as_str).unwrap_or("Last.fm API error");
            return Err(LastFmError::new(message, Some(code as i64)).into());
        }
        if !response.ok() {
            return Err(LastFmError::new(format!("Last.fm API request failed (HTTP {})", response.status), None).into());
        }
        Ok(json)
    }

    /// Step 1 of the desktop auth flow: an unauthenticated token to send the user to authorize in a browser.
    pub async fn get_auth_token(&self) -> Result<String, LastFmCallError> {
        let json = self.call_api(Method::Get, vec![("method".into(), "auth.getToken".into())], true).await?;
        match json.get("token").and_then(Value::as_str) {
            Some(token) if !token.is_empty() => Ok(token.to_owned()),
            _ => Err(LastFmError::new("Last.fm did not return an auth token", None).into()),
        }
    }

    /// Step 2: exchange an approved token for a permanent session key. Fails with code 14 ("Unauthorized Token") if
    /// the user hasn't actually approved it in the browser yet - callers should surface that as "not yet - try again
    /// after approving" rather than a hard failure.
    pub async fn get_session(&self, token: &str) -> Result<LastFmSession, LastFmCallError> {
        let json = self
            .call_api(Method::Get, vec![("method".into(), "auth.getSession".into()), ("token".into(), token.into())], true)
            .await?;
        let session = json.get("session").and_then(Value::as_object);
        let field = |name: &str| session.and_then(|s| s.get(name)).and_then(Value::as_str).filter(|v| !v.is_empty());
        match (field("key"), field("name")) {
            (Some(key), Some(name)) => Ok(LastFmSession { session_key: key.to_owned(), username: name.to_owned() }),
            _ => Err(LastFmError::new("Last.fm did not return a session", None).into()),
        }
    }

    /// Fire on track start - Last.fm surfaces this as the user's real-time "now playing" status, separate from the
    /// eventual scrobble.
    pub async fn update_now_playing(&self, session_key: &str, artist: &str, track: &str, album: Option<&str>, duration_seconds: Option<f64>) -> Result<(), LastFmCallError> {
        let mut params: Vec<(String, String)> = vec![
            ("method".into(), "track.updateNowPlaying".into()),
            ("sk".into(), session_key.into()),
            ("artist".into(), artist.into()),
            ("track".into(), track.into()),
        ];
        if let Some(album) = album.filter(|a| !a.is_empty()) {
            params.push(("album".into(), album.into()));
        }
        if let Some(d) = duration_seconds.filter(|d| *d != 0.0 && !d.is_nan()) {
            params.push(("duration".into(), js_round(d).to_string()));
        }
        self.call_api(Method::Post, params, true).await.map(drop)
    }

    /// Submits up to 50 scrobbles in one call (Last.fm's batch form: `artist[0]`, `track[0]`, `timestamp[0]`... rather
    /// than repeated single calls) - used both for a normal single-track scrobble and for flushing the offline queue.
    pub async fn scrobble_batch(&self, session_key: &str, entries: &[ScrobbleEntry]) -> Result<(), LastFmCallError> {
        if entries.is_empty() {
            return Ok(());
        }
        if entries.len() > 50 {
            return Err(LastFmError::new("scrobbleBatch supports at most 50 entries per call", None).into());
        }
        let mut params: Vec<(String, String)> = vec![("method".into(), "track.scrobble".into()), ("sk".into(), session_key.into())];
        for (i, entry) in entries.iter().enumerate() {
            params.push((format!("artist[{i}]"), entry.artist.clone()));
            params.push((format!("track[{i}]"), entry.track.clone()));
            params.push((format!("timestamp[{i}]"), entry.timestamp.to_string()));
            if let Some(album) = entry.album.as_ref().filter(|a| !a.is_empty()) {
                params.push((format!("album[{i}]"), album.clone()));
            }
            if let Some(d) = entry.duration_seconds.filter(|d| *d != 0.0 && !d.is_nan()) {
                params.push((format!("duration[{i}]"), js_round(d).to_string()));
            }
        }
        self.call_api(Method::Post, params, true).await.map(drop)
    }
}

/// The page to open so the user can approve `token` - no callback URL needed for this desktop-style flow; the caller
/// re-polls `get_session` once the user says they've approved.
pub fn build_auth_url(credentials: &LastFmCredentials, token: &str) -> String {
    format!("https://www.last.fm/api/auth/?api_key={}&token={}", encode_uri_component(&credentials.api_key), encode_uri_component(token))
}

/// Last.fm's "web application" auth variant: no `auth.getToken` call up front - Last.fm mints the token itself and
/// redirects the browser to `callback_url?token=...` once the user approves, which the caller reads back automatically
/// (a same-origin popup message on web, a `beatseam://` deep link on Android) instead of the desktop flow's manual
/// "I've approved it" confirmation. Unlike most OAuth providers, Last.fm doesn't require `cb` to be pre-registered -
/// any URL works at request time, which is what makes this usable from an arbitrary self-hosted Beatseam origin.
pub fn build_web_auth_url(credentials: &LastFmCredentials, callback_url: &str) -> String {
    format!("https://www.last.fm/api/auth/?api_key={}&cb={}", encode_uri_component(&credentials.api_key), encode_uri_component(callback_url))
}

/// Whether a played track/position qualifies for scrobbling per Last.fm's own rule (see the threshold constants).
pub fn meets_scrobble_threshold(duration_seconds: f64, elapsed_seconds: f64) -> bool {
    if duration_seconds < MIN_SCROBBLE_TRACK_SECONDS {
        return false;
    }
    elapsed_seconds >= (duration_seconds * SCROBBLE_THRESHOLD_FRACTION).min(SCROBBLE_THRESHOLD_MAX_SECONDS)
}

#[cfg(test)]
mod tests {
    use crate::http::test_support::FakeTransport;
    use super::*;
    use crate::http::block_on;

    fn creds() -> LastFmCredentials {
        LastFmCredentials { api_key: "key".into(), api_secret: "secret".into() }
    }

    fn entry(n: i64) -> ScrobbleEntry {
        ScrobbleEntry { artist: format!("Artist {n}"), track: format!("Track {n}"), timestamp: 1_700_000_000 + n, album: None, duration_seconds: Some(200.4) }
    }

    #[test]
    fn the_signature_sorts_params_skips_format_and_callback_and_appends_the_secret() {
        let params = vec![("method".to_string(), "auth.getToken".to_string()), ("api_key".to_string(), "key".to_string()), ("format".to_string(), "json".to_string()), ("callback".to_string(), "x".to_string())];
        assert_eq!(build_api_sig(&params, "secret"), md5("api_keykeymethodauth.getTokensecret"));
    }

    #[test]
    fn get_auth_token_sends_a_signed_get_and_returns_the_token() {
        let transport = FakeTransport::json(200, r#"{"token":"abc"}"#);
        let client = LastFmClient::new(creds(), &transport);
        assert_eq!(block_on(client.get_auth_token()), Ok("abc".to_string()));
        let req = &transport.requests.borrow()[0];
        let sig = md5("api_keykeymethodauth.getTokensecret");
        assert_eq!(req.url, format!("https://ws.audioscrobbler.com/2.0/?method=auth.getToken&api_key=key&api_sig={sig}&format=json"));
        assert_eq!((req.method, req.body.as_ref(), req.headers.len()), (Method::Get, None, 0));
    }

    #[test]
    fn posts_are_form_encoded_with_a_content_type_header() {
        let transport = FakeTransport::json(200, "{}");
        let client = LastFmClient::new(creds(), &transport);
        block_on(client.update_now_playing("sk", "A & B", "T", Some("Alb"), Some(199.5))).unwrap();
        let req = &transport.requests.borrow()[0];
        assert_eq!((req.method, req.url.as_str()), (Method::Post, "https://ws.audioscrobbler.com/2.0/"));
        assert_eq!(req.headers, vec![("Content-Type".to_string(), "application/x-www-form-urlencoded".to_string())]);
        let body = req.body.as_ref().unwrap();
        assert!(body.starts_with("method=track.updateNowPlaying&sk=sk&artist=A+%26+B&track=T&album=Alb&duration=200&api_key=key&api_sig="), "{body}");
        assert!(body.ends_with("&format=json"));
    }

    #[test]
    fn empty_album_and_zero_duration_are_left_out() {
        let transport = FakeTransport::json(200, "{}");
        let client = LastFmClient::new(creds(), &transport);
        block_on(client.update_now_playing("sk", "A", "T", Some(""), Some(0.0))).unwrap();
        let body = transport.requests.borrow()[0].body.clone().unwrap();
        assert!(!body.contains("album=") && !body.contains("duration="), "{body}");
    }

    #[test]
    fn scrobble_batch_indexes_every_field_and_enforces_the_limits() {
        let transport = FakeTransport::json(200, "{}");
        let client = LastFmClient::new(creds(), &transport);
        block_on(client.scrobble_batch("sk", &[entry(0), ScrobbleEntry { album: Some("Alb".into()), ..entry(1) }])).unwrap();
        let body = transport.requests.borrow()[0].body.clone().unwrap();
        assert!(body.contains("artist%5B0%5D=Artist+0") && body.contains("timestamp%5B1%5D=1700000001") && body.contains("album%5B1%5D=Alb") && body.contains("duration%5B0%5D=200"), "{body}");
        assert!(!body.contains("album%5B0%5D"));

        assert_eq!(block_on(client.scrobble_batch("sk", &[])), Ok(()));
        assert_eq!(transport.requests.borrow().len(), 1, "an empty batch sends nothing");
        let too_many: Vec<_> = (0..51).map(entry).collect();
        let err = block_on(client.scrobble_batch("sk", &too_many)).unwrap_err();
        assert_eq!(err, LastFmCallError::LastFm(LastFmError { message: "scrobbleBatch supports at most 50 entries per call".into(), code: None }));
        assert_eq!(transport.requests.borrow().len(), 1);
    }

    #[test]
    fn api_errors_carry_the_last_fm_code_and_message() {
        let transport = FakeTransport::json(403, r#"{"error":14,"message":"Unauthorized Token"}"#);
        let err = block_on(LastFmClient::new(creds(), &transport).get_session("tok")).unwrap_err();
        assert_eq!(err, LastFmCallError::LastFm(LastFmError { message: "Unauthorized Token".into(), code: Some(14) }));

        let no_message = FakeTransport::json(200, r#"{"error":9}"#);
        let err = block_on(LastFmClient::new(creds(), &no_message).get_auth_token()).unwrap_err();
        assert_eq!(err, LastFmCallError::LastFm(LastFmError { message: "Last.fm API error".into(), code: Some(9) }));
    }

    #[test]
    fn an_http_failure_without_a_json_error_is_reported_with_its_status() {
        for body in ["<html>bad gateway</html>", "[]", "", "null"] {
            let transport = FakeTransport::json(502, body);
            let err = block_on(LastFmClient::new(creds(), &transport).get_auth_token()).unwrap_err();
            assert_eq!(err, LastFmCallError::LastFm(LastFmError { message: "Last.fm API request failed (HTTP 502)".into(), code: None }), "{body:?}");
        }
    }

    #[test]
    fn a_successful_response_missing_what_was_asked_for_is_an_error() {
        let err = block_on(LastFmClient::new(creds(), &FakeTransport::json(200, "{}")).get_auth_token()).unwrap_err();
        assert!(matches!(err, LastFmCallError::LastFm(LastFmError { code: None, .. })));
        for body in ["{}", r#"{"session":{"key":"k"}}"#, r#"{"session":{"key":"","name":"n"}}"#, r#"{"session":"x"}"#] {
            let err = block_on(LastFmClient::new(creds(), &FakeTransport::json(200, body)).get_session("t")).unwrap_err();
            assert_eq!(err, LastFmCallError::LastFm(LastFmError { message: "Last.fm did not return a session".into(), code: None }), "{body}");
        }
        let ok = block_on(LastFmClient::new(creds(), &FakeTransport::json(200, r#"{"session":{"key":"k","name":"n","subscriber":0}}"#)).get_session("t"));
        assert_eq!(ok, Ok(LastFmSession { session_key: "k".into(), username: "n".into() }));
    }

    #[test]
    fn a_dead_network_is_a_transport_error_not_a_last_fm_one() {
        let transport = FakeTransport::offline();
        let err = block_on(LastFmClient::new(creds(), &transport).get_auth_token()).unwrap_err();
        assert_eq!(err, LastFmCallError::Transport(TransportError("offline".into())));
    }

    #[test]
    fn auth_urls_encode_like_encode_uri_component() {
        assert_eq!(build_auth_url(&creds(), "t k/n"), "https://www.last.fm/api/auth/?api_key=key&token=t%20k%2Fn");
        assert_eq!(build_web_auth_url(&creds(), "beatseam://cb?x=1&y=2"), "https://www.last.fm/api/auth/?api_key=key&cb=beatseam%3A%2F%2Fcb%3Fx%3D1%26y%3D2");
    }

    #[test]
    fn the_scrobble_threshold_is_half_the_track_or_four_minutes_with_a_30_second_floor() {
        assert!(!meets_scrobble_threshold(29.0, 29.0));
        assert!(!meets_scrobble_threshold(200.0, 99.9));
        assert!(meets_scrobble_threshold(200.0, 100.0));
        assert!(!meets_scrobble_threshold(1000.0, 239.9));
        assert!(meets_scrobble_threshold(1000.0, 240.0));
        assert!(meets_scrobble_threshold(30.0, 15.0));
    }
}
