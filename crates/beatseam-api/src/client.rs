//! A small, typed client for the Beatseam server (`packages/api/src/client.ts`). Shared by every app so the wire format
//! lives in one place; responses are deserialized into the same DTOs the server is written against, so a version
//! mismatch fails loudly here instead of corrupting local data.
//!
//! The client owns no network code: it is generic over [`HttpTransport`], and cancellation is an
//! [`AbortSignal`](beatseam_core::tasks::AbortSignal) (dropping the returned future also cancels the request).

use std::fmt;
use std::sync::Arc;

use beatseam_core::http::{HttpRequest, HttpResponse, HttpTransport, Method, encode_uri_component, form_urlencode};
use beatseam_core::tasks::{AbortSignal, Aborted, abortable};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::auth::{AuthStatusDto, DeviceCodeRequestDto, DeviceCodeResultDto, DeviceWaitRequestDto, DeviceWaitResultDto, MeDto};
use crate::compatibility::{ApiCompatibility, CompatibilityProblem, check_response_compatibility};
use crate::encoding::utf8_decode;
use crate::lastfm::{LastFmConnectResultDto, LastFmNowPlayingRequestDto, LastFmScrobbleEntryDto, LastFmScrobblesRequestDto, LastFmStatusDto};
use crate::mutations::{MutationDto, MutationsRequestDto, MutationsResponseDto};
use crate::schemas::{
    API_VERSION, API_VERSION_HEADER, API_VERSION_UNSUPPORTED_CODE, ListTracksQuery, LyricsAssignRequestDto, LyricsDto, LyricsReportDto, LyricsReportRequestDto, LyricsSaveLineDto, LyricsSaveRequestDto,
    LyricsSourceDto, Page, TrackDto,
};
use crate::sync::{SyncChangesResponseDto, SyncSnapshotResponseDto};
use crate::track_key::encode_track_key;

/// Long enough for a slow link, short enough that a stalled connection doesn't hold up everything behind it.
const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;
/// The server holds a device-code wait open for its own ~25 s before answering "pending".
const DEVICE_WAIT_TIMEOUT_MS: u64 = 120_000;

/// The server answered with an error status. `code` is the machine-readable reason when it gave one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
    pub code: Option<String>,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiClientError {
    Api(ApiError),
    /// The sync position is too old or unknown (HTTP 410, code `resync_required`): take a snapshot instead of asking
    /// for changes.
    ResyncRequired(String),
    /// The server and this app don't speak the same API version. Never retried or worked around: the person has to
    /// update the app or the server (`problem` says which), and `message` already tells them so.
    Version { message: String, problem: CompatibilityProblem },
    /// The server can't be reached at all (offline, wrong address, down, no answer in time) - as opposed to answering
    /// with an error.
    Network(String),
    /// The answer wasn't the shape this client expects (the TypeScript's schema-parse failure).
    Decode(String),
    /// Cancelled through the caller's [`AbortSignal`].
    Aborted,
}

impl ApiClientError {
    /// The HTTP status behind this error, where there is one (an [`ApiError`], a resync request, a version mismatch).
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Api(e) => Some(e.status),
            Self::ResyncRequired(_) => Some(410),
            Self::Version { .. } => Some(426),
            _ => None,
        }
    }

    /// The server's machine-readable `code`, if it gave one.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Api(e) => e.code.as_deref(),
            Self::ResyncRequired(_) => Some("resync_required"),
            Self::Version { .. } => Some(API_VERSION_UNSUPPORTED_CODE),
            _ => None,
        }
    }
}

impl fmt::Display for ApiClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api(e) => e.fmt(f),
            Self::ResyncRequired(m) | Self::Network(m) | Self::Decode(m) => f.write_str(m),
            Self::Version { message, .. } => f.write_str(message),
            Self::Aborted => f.write_str("aborted"),
        }
    }
}

impl std::error::Error for ApiClientError {}

impl From<Aborted> for ApiClientError {
    fn from(_: Aborted) -> Self {
        Self::Aborted
    }
}

type TokenSource = Arc<dyn Fn() -> Option<String> + Send + Sync>;

#[derive(Clone)]
pub struct ApiClientOptions {
    /// e.g. `https://music.example.com`. Empty means same-origin (the web app served by the server itself).
    pub base_url: String,
    /// A native app's device token. The web app leaves this out and relies on its session cookie. Called for every
    /// request, so it can read whatever the app keeps the token in (cache it in memory; this is not async).
    pub token: Option<TokenSource>,
    /// How long a JSON request may take to get its answer before it is given up on. Default 30 s.
    pub request_timeout_ms: Option<u64>,
}

impl ApiClientOptions {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self { base_url: base_url.into(), token: None, request_timeout_ms: None }
    }

    pub fn with_token(mut self, token: impl Fn() -> Option<String> + Send + Sync + 'static) -> Self {
        self.token = Some(Arc::new(token));
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum DeviceSignInProgress {
    Code { user_code: String, verification_uri: String, expires_in_seconds: f64 },
    Waiting,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DeviceSignInResult {
    SignedIn { token: String, me: MeDto },
    Denied,
    Expired,
}

pub struct ApiClient<T: HttpTransport> {
    base_url: String,
    options: ApiClientOptions,
    transport: T,
}

fn json_body(value: &impl Serialize) -> String {
    serde_json::to_string(value).expect("a DTO always serializes")
}

fn query_string(pairs: &[(&str, String)]) -> String {
    if pairs.is_empty() { String::new() } else { format!("?{}", form_urlencode(pairs)) }
}

impl<T: HttpTransport> ApiClient<T> {
    pub fn new(options: ApiClientOptions, transport: T) -> Self {
        let base_url = options.base_url.trim_end_matches('/').to_owned();
        Self { base_url, options, transport }
    }

    // ---------------------------------------------------------------- plumbing

    /// The headers a request to the server needs (API version, and the bearer token for a native app) - for backends
    /// that perform the request themselves, e.g. a native file downloader.
    pub fn auth_headers(&self, extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut headers = vec![(API_VERSION_HEADER.to_owned(), API_VERSION.to_string())];
        if let Some(token) = self.options.token.as_ref().and_then(|t| t()).filter(|t| !t.is_empty()) {
            headers.push(("authorization".into(), format!("Bearer {token}")));
        }
        headers.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        headers
    }

    /// Every /api/v1 answer says which versions the server speaks, so a mismatch is caught on the first request of any
    /// kind.
    fn assert_compatible(response: &HttpResponse) -> Result<(), ApiClientError> {
        match check_response_compatibility(&response.headers) {
            ApiCompatibility::Compatible => Ok(()),
            ApiCompatibility::Incompatible { problem, message } => Err(ApiClientError::Version { message, problem }),
        }
    }

    async fn transport_send(&self, request: HttpRequest, signal: Option<&AbortSignal>) -> Result<HttpResponse, ApiClientError> {
        abortable(signal, self.transport.send(request)).await?.map_err(|e| ApiClientError::Network(e.0))
    }

    /// Performs a request, turning "couldn't connect" and "server said no" into distinct errors.
    async fn send(&self, method: Method, path: &str, body: Option<String>, signal: Option<&AbortSignal>, timeout_ms: Option<u64>) -> Result<HttpResponse, ApiClientError> {
        let extra: &[(&str, &str)] = if body.is_some() { &[("content-type", "application/json")] } else { &[] };
        let request = HttpRequest {
            method,
            url: format!("{}{path}", self.base_url),
            headers: self.auth_headers(extra),
            body,
            // A request that never answers (a connection the OS silently stalls, e.g. while the app is in the
            // background) must not block whatever is queued behind it.
            timeout_ms: Some(timeout_ms.or(self.options.request_timeout_ms).unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS)),
        };
        let response = self.transport_send(request, signal).await?;
        Self::assert_compatible(&response)?;
        if response.ok() {
            return Ok(response);
        }

        let parsed: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
        let message = match parsed.get("message") {
            Some(Value::Array(parts)) => parts.iter().map(|p| p.as_str().map_or_else(|| p.to_string(), str::to_owned)).collect::<Vec<_>>().join("; "),
            Some(Value::String(s)) => s.clone(),
            _ => status_message(&response),
        };
        let code = parsed.get("code").and_then(Value::as_str).map(str::to_owned);
        if response.status == 410 && code.as_deref() == Some("resync_required") {
            return Err(ApiClientError::ResyncRequired(message));
        }
        Err(ApiClientError::Api(ApiError { status: response.status, message, code }))
    }

    async fn request<R: DeserializeOwned>(&self, method: Method, path: &str, body: Option<String>, signal: Option<&AbortSignal>, timeout_ms: Option<u64>) -> Result<R, ApiClientError> {
        let response = self.send(method, path, body, signal, timeout_ms).await?;
        serde_json::from_slice(&response.body).map_err(|e| ApiClientError::Decode(format!("{path}: {e}")))
    }

    async fn get<R: DeserializeOwned>(&self, path: &str) -> Result<R, ApiClientError> {
        self.request(Method::Get, path, None, None, None).await
    }

    async fn post<R: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<R, ApiClientError> {
        self.request(Method::Post, path, Some(json_body(body)), None, None).await
    }

    // ---------------------------------------------------------------- auth

    pub async fn auth_status(&self) -> Result<AuthStatusDto, ApiClientError> {
        self.get("/api/v1/auth/status").await
    }

    pub async fn me(&self) -> Result<MeDto, ApiClientError> {
        self.get("/api/v1/auth/me").await
    }

    pub async fn device_code(&self, device_name: &str) -> Result<DeviceCodeResultDto, ApiClientError> {
        self.post("/api/v1/auth/device/code", &DeviceCodeRequestDto { device_name: device_name.to_owned() }).await
    }

    /// Holds open until the user decides (or the server's wait ends with `Pending` - ask again).
    pub async fn device_wait(&self, device_code: &str, signal: Option<&AbortSignal>) -> Result<DeviceWaitResultDto, ApiClientError> {
        let body = json_body(&DeviceWaitRequestDto { device_code: device_code.to_owned() });
        self.request(Method::Post, "/api/v1/auth/device/wait", Some(body), signal, Some(DEVICE_WAIT_TIMEOUT_MS)).await
    }

    /// The whole native sign-in: ask for a code, hand it to `on_progress` for display, then wait for the user to
    /// approve it in a browser. The waiting is a chain of held-open requests, not a timer.
    pub async fn sign_in_with_device_code(&self, device_name: &str, mut on_progress: impl FnMut(DeviceSignInProgress), signal: Option<&AbortSignal>) -> Result<DeviceSignInResult, ApiClientError> {
        let grant = self.device_code(device_name).await?;
        on_progress(DeviceSignInProgress::Code { user_code: grant.user_code, verification_uri: grant.verification_uri, expires_in_seconds: grant.expires_in_seconds });
        while !signal.is_some_and(AbortSignal::is_aborted) {
            let result = self.device_wait(&grant.device_code, signal).await?;
            on_progress(DeviceSignInProgress::Waiting);
            match result {
                DeviceWaitResultDto::Approved { token, me } => return Ok(DeviceSignInResult::SignedIn { token, me }),
                DeviceWaitResultDto::Denied => return Ok(DeviceSignInResult::Denied),
                DeviceWaitResultDto::Expired => return Ok(DeviceSignInResult::Expired),
                DeviceWaitResultDto::Pending => {}
            }
        }
        Err(ApiClientError::Aborted)
    }

    /// A browser signing out: ends its session and clears the cookie.
    pub async fn logout(&self) -> Result<(), ApiClientError> {
        self.send(Method::Post, "/api/v1/auth/logout", Some("{}".into()), None, None).await.map(drop)
    }

    /// A native app signing itself out: revokes the token it is calling with.
    pub async fn sign_out_this_device(&self) -> Result<(), ApiClientError> {
        self.send(Method::Delete, "/api/v1/auth/me/token", None, None, None).await.map(drop)
    }

    // ---------------------------------------------------------------- last.fm (the server holds the link)

    pub async fn last_fm_status(&self) -> Result<LastFmStatusDto, ApiClientError> {
        self.get("/api/v1/lastfm").await
    }

    /// Starts linking: returns the Last.fm page to approve Beatseam on. Then call [`last_fm_complete`](Self::last_fm_complete).
    pub async fn last_fm_connect(&self) -> Result<LastFmConnectResultDto, ApiClientError> {
        self.post("/api/v1/lastfm/connect", &serde_json::json!({})).await
    }

    /// Finishes linking after approval. Fails with an [`ApiError`] 409 (code `not_authorized`) while the approval
    /// hasn't happened yet.
    pub async fn last_fm_complete(&self) -> Result<LastFmStatusDto, ApiClientError> {
        self.post("/api/v1/lastfm/complete", &serde_json::json!({})).await
    }

    pub async fn last_fm_disconnect(&self) -> Result<LastFmStatusDto, ApiClientError> {
        self.request(Method::Delete, "/api/v1/lastfm", None, None, None).await
    }

    pub async fn last_fm_now_playing(&self, track: &LastFmNowPlayingRequestDto) -> Result<(), ApiClientError> {
        self.send(Method::Post, "/api/v1/lastfm/now-playing", Some(json_body(track)), None, None).await.map(drop)
    }

    /// Fails with an [`ApiError`] 422 (code `lastfm_rejected`) when Last.fm refuses them for good, 502
    /// (`lastfm_unreachable`) when it couldn't be reached - the latter is worth retrying later.
    pub async fn last_fm_scrobble(&self, entries: Vec<LastFmScrobbleEntryDto>) -> Result<(), ApiClientError> {
        self.send(Method::Post, "/api/v1/lastfm/scrobbles", Some(json_body(&LastFmScrobblesRequestDto { entries })), None, None).await.map(drop)
    }

    // ---------------------------------------------------------------- library, sync, mutations

    pub async fn list_tracks(&self, query: &ListTracksQuery) -> Result<Page<TrackDto>, ApiClientError> {
        self.get(&format!("/api/v1/tracks{}", query_string(&query.to_pairs()))).await
    }

    /// Fails with [`ApiClientError::ResyncRequired`] when `since` is too old: take a snapshot instead.
    pub async fn sync_changes(&self, since: &str, limit: Option<u32>) -> Result<SyncChangesResponseDto, ApiClientError> {
        let limit = limit.filter(|l| *l != 0).map(|l| format!("&limit={l}")).unwrap_or_default();
        self.get(&format!("/api/v1/sync/changes?since={}{limit}", encode_uri_component(since))).await
    }

    pub async fn sync_snapshot(&self, after: Option<&str>, limit: Option<u32>) -> Result<SyncSnapshotResponseDto, ApiClientError> {
        let mut pairs = Vec::new();
        if let Some(after) = after.filter(|a| !a.is_empty()) {
            pairs.push(("after", after.to_owned()));
        }
        if let Some(limit) = limit.filter(|l| *l != 0) {
            pairs.push(("limit", limit.to_string()));
        }
        self.get(&format!("/api/v1/sync/snapshot{}", query_string(&pairs))).await
    }

    pub async fn mutations(&self, mutations: Vec<MutationDto>) -> Result<MutationsResponseDto, ApiClientError> {
        self.post("/api/v1/mutations", &MutationsRequestDto { mutations }).await
    }

    // ---------------------------------------------------------------- files

    /// Address of a track's audio. Fetch it with [`fetch_bytes`](Self::fetch_bytes) so the auth header is sent.
    pub fn stream_path(&self, track_key: &str) -> String {
        format!("/api/v1/tracks/{track_key}/stream")
    }

    pub fn cover_path(&self, track_key: &str) -> String {
        format!("/api/v1/tracks/{track_key}/cover")
    }

    /// For a raw file id (`Root:path/to/file.mp3`) rather than a key.
    pub fn stream_path_for_file_id(&self, file_id: &str) -> String {
        self.stream_path(&encode_track_key(file_id))
    }

    /// Full URL, for the rare place that needs one (e.g. handing to a native player). Auth still has to be supplied
    /// separately ([`auth_headers`](Self::auth_headers)).
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// The track's lyrics, or `None` if it has none (a 404).
    pub async fn lyrics(&self, track_key: &str, signal: Option<&AbortSignal>) -> Result<Option<LyricsDto>, ApiClientError> {
        match self.request(Method::Get, &format!("/api/v1/tracks/{track_key}/lyrics"), None, signal, None).await {
            Err(ApiClientError::Api(e)) if e.status == 404 => Ok(None),
            other => other.map(Some),
        }
    }

    /// Flags the track's lyrics as wrong (or missing a line, in the wrong language, ...) for an admin to review.
    /// Reporting again replaces the earlier report.
    pub async fn report_lyrics(&self, track_key: &str, reason: Option<&str>) -> Result<(), ApiClientError> {
        let body = LyricsReportRequestDto { reason: reason.filter(|r| !r.is_empty()).map(str::to_owned) };
        self.send(Method::Post, &format!("/api/v1/tracks/{track_key}/lyrics/report"), Some(json_body(&body)), None, None).await.map(drop)
    }

    /// Admin only. Lyrics reports waiting for review, newest first.
    pub async fn lyrics_reports(&self) -> Result<Vec<LyricsReportDto>, ApiClientError> {
        self.get("/api/v1/lyrics/reports").await
    }

    /// Admin only. Marks a report as dealt with.
    pub async fn resolve_lyrics_report(&self, id: &str) -> Result<(), ApiClientError> {
        self.send(Method::Delete, &format!("/api/v1/lyrics/reports/{}", encode_uri_component(id)), None, None, None).await.map(drop)
    }

    /// Admin only. The file assigned to the track's lyrics and every file it could be assigned.
    pub async fn lyrics_source(&self, track_key: &str) -> Result<LyricsSourceDto, ApiClientError> {
        self.get(&format!("/api/v1/tracks/{track_key}/lyrics/source")).await
    }

    /// Admin only. Links the track to a lyrics file (or, with `None`, to none).
    pub async fn assign_lyrics(&self, track_key: &str, lrc_file_id: Option<&str>) -> Result<(), ApiClientError> {
        let body = LyricsAssignRequestDto { lrc_file_id: lrc_file_id.map(str::to_owned) };
        self.send(Method::Put, &format!("/api/v1/tracks/{track_key}/lyrics/assignment"), Some(json_body(&body)), None, None).await.map(drop)
    }

    /// Admin only. Writes the lines back to the track's assigned .lrc file. Fails with an [`ApiError`] 409 (code
    /// `lyrics_read_only`) when the server's lyrics folder can't be written to.
    pub async fn save_lyrics(&self, track_key: &str, lines: Vec<LyricsSaveLineDto>) -> Result<(), ApiClientError> {
        self.send(Method::Put, &format!("/api/v1/tracks/{track_key}/lyrics"), Some(json_body(&LyricsSaveRequestDto { lines })), None, None).await.map(drop)
    }

    /// A plain GET of a path that is not JSON (audio, cover art, ...), optionally a byte range (inclusive ends).
    async fn fetch_raw(&self, path: &str, range: Option<(u64, u64)>, signal: Option<&AbortSignal>) -> Result<HttpResponse, ApiClientError> {
        let range_header = range.map(|(start, end)| format!("bytes={start}-{end}"));
        let extra: Vec<(&str, &str)> = range_header.iter().map(|r| ("range", r.as_str())).collect();
        let mut request = HttpRequest::new(Method::Get, format!("{}{path}", self.base_url));
        request.headers = self.auth_headers(&extra);
        let response = self.transport_send(request, signal).await?;
        Self::assert_compatible(&response)?;
        if !response.ok() {
            return Err(ApiClientError::Api(ApiError { status: response.status, message: status_message(&response), code: None }));
        }
        Ok(response)
    }

    /// The bytes behind a path (audio, cover art, ...).
    pub async fn fetch_bytes(&self, path: &str, range: Option<(u64, u64)>, signal: Option<&AbortSignal>) -> Result<Vec<u8>, ApiClientError> {
        Ok(self.fetch_raw(path, range, signal).await?.body)
    }

    /// Like [`fetch_bytes`](Self::fetch_bytes), but also says what the server called the file (for cover art:
    /// `image/webp`, ...).
    pub async fn fetch_file(&self, path: &str, signal: Option<&AbortSignal>) -> Result<(Vec<u8>, String), ApiClientError> {
        let response = self.fetch_raw(path, None, signal).await?;
        let content_type = response.header("content-type").unwrap_or("application/octet-stream").to_owned();
        Ok((response.body, content_type))
    }

    /// The text of a file at a path (e.g. lyrics).
    pub async fn fetch_text(&self, path: &str, signal: Option<&AbortSignal>) -> Result<String, ApiClientError> {
        Ok(utf8_decode(&self.fetch_bytes(path, None, signal).await?))
    }
}

/// `response.statusText || "HTTP <status>"`.
fn status_message(response: &HttpResponse) -> String {
    if response.status_text.is_empty() { format!("HTTP {}", response.status) } else { response.status_text.clone() }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::pin::pin;
    use std::rc::Rc;
    use std::task::{Context, Poll, Waker};

    use beatseam_core::http::{TransportError, block_on};
    use beatseam_core::tasks::{AbortHandle, Gate};

    use super::*;
    use crate::auth::SessionVia;

    /// Answers requests from a queue and records them; a `None` answer is a request that never completes (until
    /// dropped), like a stalled connection.
    struct Scripted {
        answers: RefCell<VecDeque<Option<HttpResponse>>>,
        sent: RefCell<Vec<HttpRequest>>,
        dropped_while_pending: Rc<RefCell<bool>>,
    }

    impl Scripted {
        fn new(answers: impl IntoIterator<Item = Option<HttpResponse>>) -> Self {
            Self { answers: RefCell::new(answers.into_iter().collect()), sent: RefCell::default(), dropped_while_pending: Rc::default() }
        }
    }

    impl HttpTransport for &Scripted {
        async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            self.sent.borrow_mut().push(request);
            match self.answers.borrow_mut().pop_front().expect("an answer was scripted") {
                Some(r) => Ok(r),
                None => {
                    struct OnDrop(Rc<RefCell<bool>>);
                    impl Drop for OnDrop {
                        fn drop(&mut self) {
                            *self.0.borrow_mut() = true;
                        }
                    }
                    let _flag = OnDrop(self.dropped_while_pending.clone());
                    Gate::new().wait().await; // never opens
                    unreachable!()
                }
            }
        }
    }

    fn json(status: u16, body: &str) -> Option<HttpResponse> {
        Some(HttpResponse::new(status, body))
    }

    fn json_with_versions(status: u16, body: &str, versions: &str) -> Option<HttpResponse> {
        Some(HttpResponse::new(status, body).with_header("X-Beatseam-Api-Versions", versions))
    }

    const STATUS: &str = r#"{"setupRequired":false,"setupTokenRequired":false}"#;
    const ME: &str = r#"{"id":"u","username":"u","displayName":"U","role":"USER","enrolled":true,"hasPassword":true,"totpEnabled":false,"passkeys":[],"recoveryCodesRemaining":0,"authenticatedAtMs":null,"freshUntilMs":null,"serverTimeMs":1,"via":"device"}"#;

    fn client(t: &Scripted) -> ApiClient<&Scripted> {
        ApiClient::new(ApiClientOptions::new("https://x"), t)
    }

    #[test]
    fn sends_the_version_it_speaks_on_every_request() {
        let t = Scripted::new([json(200, STATUS)]);
        block_on(client(&t).auth_status()).unwrap();
        assert!(t.sent.borrow()[0].headers.contains(&("X-Beatseam-Api".to_string(), API_VERSION.to_string())));
    }

    #[test]
    fn works_with_a_server_that_advertises_its_version_and_with_an_older_one_that_does_not() {
        let t = Scripted::new([json_with_versions(200, STATUS, "1"), json(200, STATUS)]);
        assert!(block_on(client(&t).auth_status()).is_ok());
        assert!(block_on(client(&t).auth_status()).is_ok());
    }

    #[test]
    fn refuses_a_server_that_has_moved_past_the_app_telling_the_person_to_update_the_app() {
        let t = Scripted::new([json_with_versions(200, STATUS, "2")]);
        let err = block_on(client(&t).auth_status()).unwrap_err();
        assert!(matches!(&err, ApiClientError::Version { problem: CompatibilityProblem::AppTooOld, message } if message.contains("newer version of the app")), "{err:?}");
        assert_eq!((err.status(), err.code()), (Some(426), Some("api_version_unsupported")));
    }

    #[test]
    fn reports_the_mismatch_ahead_of_the_error_the_server_also_sent() {
        let t = Scripted::new([json_with_versions(401, r#"{"message":"Sign in required"}"#, "2")]);
        assert!(matches!(block_on(client(&t).auth_status()), Err(ApiClientError::Version { .. })));
    }

    #[test]
    fn also_checks_downloads_of_bytes_and_files() {
        let wrong = || Some(HttpResponse::new(200, "x").with_header("X-Beatseam-Api-Versions", "2"));
        let t = Scripted::new([wrong(), wrong()]);
        assert!(matches!(block_on(client(&t).fetch_bytes("/a", None, None)), Err(ApiClientError::Version { .. })));
        assert!(matches!(block_on(client(&t).fetch_file("/a", None)), Err(ApiClientError::Version { .. })));
    }

    #[test]
    fn a_native_app_sends_its_bearer_token_and_a_cookie_app_sends_none() {
        let t = Scripted::new([json(200, STATUS), json(200, STATUS), json(200, STATUS)]);
        let with = ApiClient::new(ApiClientOptions::new("https://x").with_token(|| Some("secret".into())), &t);
        block_on(with.auth_status()).unwrap();
        block_on(client(&t).auth_status()).unwrap();
        let empty = ApiClient::new(ApiClientOptions::new("https://x").with_token(|| Some(String::new())), &t);
        block_on(empty.auth_status()).unwrap();
        let sent = t.sent.borrow();
        assert!(sent[0].headers.contains(&("authorization".to_string(), "Bearer secret".to_string())));
        assert!(!sent[1].headers.iter().any(|(k, _)| k == "authorization"));
        assert!(!sent[2].headers.iter().any(|(k, _)| k == "authorization"), "an empty token counts as none");
    }

    #[test]
    fn timeouts_default_to_30s_can_be_overridden_and_the_device_wait_gets_two_minutes() {
        let t = Scripted::new([json(200, STATUS), json(200, STATUS), json(200, r#"{"status":"pending"}"#)]);
        block_on(client(&t).auth_status()).unwrap();
        let mut options = ApiClientOptions::new("https://x");
        options.request_timeout_ms = Some(5_000);
        block_on(ApiClient::new(options, &t).auth_status()).unwrap();
        block_on(client(&t).device_wait("dc", None)).unwrap();
        let sent = t.sent.borrow();
        assert_eq!([sent[0].timeout_ms, sent[1].timeout_ms, sent[2].timeout_ms], [Some(30_000), Some(5_000), Some(120_000)]);
    }

    #[test]
    fn a_dead_network_and_a_stale_sync_cursor_are_their_own_errors() {
        struct Offline;
        impl HttpTransport for Offline {
            async fn send(&self, _r: HttpRequest) -> Result<HttpResponse, TransportError> {
                Err(TransportError("The server did not answer in time".into()))
            }
        }
        let c = ApiClient::new(ApiClientOptions::new("https://x"), Offline);
        assert_eq!(block_on(c.auth_status()), Err(ApiClientError::Network("The server did not answer in time".into())));

        let t = Scripted::new([json(410, r#"{"code":"resync_required","message":"too old"}"#)]);
        assert_eq!(block_on(client(&t).sync_changes("5", None)), Err(ApiClientError::ResyncRequired("too old".into())));
    }

    #[test]
    fn cancelling_through_the_signal_drops_the_pending_request() {
        let t = Scripted::new([None]);
        let handle = AbortHandle::new();
        let signal = handle.signal();
        let c = client(&t);
        let mut fut = pin!(c.device_wait("dc", Some(&signal)));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        assert!(!*t.dropped_while_pending.borrow());
        handle.abort();
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Ready(Err(ApiClientError::Aborted))));
        assert!(*t.dropped_while_pending.borrow(), "the request future was dropped, which is how a transport cancels");
    }

    #[test]
    fn the_device_sign_in_keeps_asking_while_pending_then_returns_the_token() {
        let code = r#"{"deviceCode":"dc","userCode":"ABCD-1234","verificationUri":"https://x/d","expiresInSeconds":600}"#;
        let approved = format!(r#"{{"status":"approved","token":"tok","me":{ME}}}"#);
        let t = Scripted::new([json(200, code), json(200, r#"{"status":"pending"}"#), json(200, r#"{"status":"pending"}"#), json(200, &approved)]);
        let mut progress = Vec::new();
        let result = block_on(client(&t).sign_in_with_device_code("Pixel", |p| progress.push(p), None)).unwrap();
        let DeviceSignInResult::SignedIn { token, me } = result else { panic!("{result:?}") };
        assert_eq!((token.as_str(), me.via), ("tok", SessionVia::Device));
        assert_eq!(progress.len(), 4, "the code, then one waiting per answer");
        assert!(matches!(&progress[0], DeviceSignInProgress::Code { user_code, .. } if user_code == "ABCD-1234"));
        assert_eq!(progress[1..], [DeviceSignInProgress::Waiting, DeviceSignInProgress::Waiting, DeviceSignInProgress::Waiting]);
        // Each wait names the device code the grant returned.
        assert!(t.sent.borrow()[1].body.as_deref().unwrap().contains(r#""deviceCode":"dc""#));
    }

    #[test]
    fn the_device_sign_in_reports_a_refusal_an_expiry_and_a_cancellation() {
        let code = r#"{"deviceCode":"dc","userCode":"U","verificationUri":"https://x","expiresInSeconds":1}"#;
        for (status, expected) in [("denied", DeviceSignInResult::Denied), ("expired", DeviceSignInResult::Expired)] {
            let t = Scripted::new([json(200, code), json(200, &format!(r#"{{"status":"{status}"}}"#))]);
            assert_eq!(block_on(client(&t).sign_in_with_device_code("P", |_| {}, None)).unwrap(), expected);
        }
        let handle = AbortHandle::new();
        handle.abort();
        let t = Scripted::new([json(200, code)]);
        let signal = handle.signal();
        assert_eq!(block_on(client(&t).sign_in_with_device_code("P", |_| {}, Some(&signal))), Err(ApiClientError::Aborted));
    }

    #[test]
    fn lyrics_are_none_on_a_404_but_other_failures_are_errors() {
        let t = Scripted::new([json(404, "{}"), json(500, r#"{"message":"oops"}"#)]);
        assert_eq!(block_on(client(&t).lyrics("k", None)), Ok(None));
        assert!(matches!(block_on(client(&t).lyrics("k", None)), Err(ApiClientError::Api(ApiError { status: 500, .. }))));
    }
}
