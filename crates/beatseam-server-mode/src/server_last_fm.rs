use std::future::Future;
use std::sync::Arc;

use beatseam_api::lastfm::{LastFmNowPlayingRequestDto, LastFmScrobbleEntryDto};
use beatseam_api::{ApiClient, ApiClientError};
use beatseam_core::http::{HttpTransport, TransportError};
use beatseam_core::scrobble::{LastFmCallError, LastFmError, NowPlayingTrack, ScrobbleEntry, ScrobbleSink};

/// Failed scrobbles for the server's Last.fm link wait here; cleared on sign-out (they belong to that account).
pub const SERVER_SCROBBLE_QUEUE_KEY: &str = "serverMode.scrobbleQueue";

/// The server's Last.fm endpoints.
pub trait ServerLastFmApi {
    fn now_playing(&self, track: LastFmNowPlayingRequestDto) -> impl Future<Output = Result<(), ApiClientError>>;
    fn scrobble(&self, entries: Vec<LastFmScrobbleEntryDto>) -> impl Future<Output = Result<(), ApiClientError>>;
}

impl<T: HttpTransport> ServerLastFmApi for ApiClient<T> {
    async fn now_playing(&self, track: LastFmNowPlayingRequestDto) -> Result<(), ApiClientError> {
        self.last_fm_now_playing(&track).await
    }
    fn scrobble(&self, entries: Vec<LastFmScrobbleEntryDto>) -> impl Future<Output = Result<(), ApiClientError>> {
        self.last_fm_scrobble(entries)
    }
}

/// Scrobbles through the server, which holds the user's Last.fm link (see the server's `LastFmService`).
pub struct ServerScrobbleSink<A> {
    get_api: Box<dyn Fn() -> Option<Arc<A>>>,
}

impl<A: ServerLastFmApi> ServerScrobbleSink<A> {
    pub fn new(get_api: impl Fn() -> Option<Arc<A>> + 'static) -> Self {
        Self { get_api: Box::new(get_api) }
    }

    fn api(&self) -> Result<Arc<A>, LastFmCallError> {
        (self.get_api)().ok_or_else(|| LastFmCallError::Transport(TransportError("Not signed in to a server".into())))
    }
}

impl<A: ServerLastFmApi> ScrobbleSink for ServerScrobbleSink<A> {
    fn queue_key(&self) -> &str {
        SERVER_SCROBBLE_QUEUE_KEY
    }

    async fn now_playing(&self, track: &NowPlayingTrack) -> Result<(), LastFmCallError> {
        let api = self.api()?;
        api.now_playing(LastFmNowPlayingRequestDto { artist: track.artist.clone(), track: track.title.clone(), album: track.album.clone(), duration_seconds: track.duration_seconds })
            .await
            .map_err(|e| LastFmCallError::Transport(TransportError(e.to_string())))
    }

    async fn scrobble(&self, entries: &[ScrobbleEntry]) -> Result<(), LastFmCallError> {
        let api = self.api()?;
        let dtos = entries.iter().map(|e| LastFmScrobbleEntryDto { artist: e.artist.clone(), track: e.track.clone(), timestamp: e.timestamp, album: e.album.clone(), duration_seconds: e.duration_seconds }).collect();
        api.scrobble(dtos).await.map_err(|error| match error {
            // Last.fm itself refused them: final. Anything else (server or Last.fm unreachable, not connected yet) is worth
            // retrying.
            ApiClientError::Api(api) if api.status == 422 => LastFmCallError::LastFm(LastFmError { message: api.message, code: Some(0) }),
            other => LastFmCallError::Transport(TransportError(other.to_string())),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use beatseam_api::ApiError;
    use beatseam_core::http::block_on;

    use super::*;

    #[derive(Default)]
    struct Fake {
        result: RefCell<Option<ApiClientError>>,
        scrobbled: RefCell<Vec<LastFmScrobbleEntryDto>>,
        played: RefCell<Vec<LastFmNowPlayingRequestDto>>,
    }

    impl ServerLastFmApi for Fake {
        async fn now_playing(&self, track: LastFmNowPlayingRequestDto) -> Result<(), ApiClientError> {
            self.played.borrow_mut().push(track);
            self.result.borrow_mut().take().map_or(Ok(()), Err)
        }
        async fn scrobble(&self, entries: Vec<LastFmScrobbleEntryDto>) -> Result<(), ApiClientError> {
            self.scrobbled.borrow_mut().extend(entries);
            self.result.borrow_mut().take().map_or(Ok(()), Err)
        }
    }

    fn entry() -> ScrobbleEntry {
        ScrobbleEntry { artist: "A".into(), track: "T".into(), timestamp: 5, album: Some("Alb".into()), duration_seconds: Some(200.0) }
    }

    fn sink(fake: &Arc<Fake>) -> ServerScrobbleSink<Fake> {
        let fake = fake.clone();
        ServerScrobbleSink::new(move || Some(fake.clone()))
    }

    #[test]
    fn sends_now_playing_and_scrobbles_through_the_server() {
        let fake = Arc::new(Fake::default());
        let s = sink(&fake);
        assert_eq!(s.queue_key(), "serverMode.scrobbleQueue");
        block_on(s.now_playing(&NowPlayingTrack { artist: "A".into(), title: "T".into(), album: None, duration_seconds: None })).unwrap();
        assert_eq!(fake.played.borrow()[0].track, "T");
        block_on(s.scrobble(&[entry()])).unwrap();
        assert_eq!((fake.scrobbled.borrow()[0].timestamp, fake.scrobbled.borrow()[0].album.clone()), (5, Some("Alb".into())));
    }

    #[test]
    fn a_422_is_last_fm_refusing_for_good_and_anything_else_is_worth_retrying() {
        let fake = Arc::new(Fake::default());
        *fake.result.borrow_mut() = Some(ApiClientError::Api(ApiError { status: 422, message: "bad track".into(), code: None }));
        assert_eq!(block_on(sink(&fake).scrobble(&[entry()])), Err(LastFmCallError::LastFm(LastFmError { message: "bad track".into(), code: Some(0) })));
        for error in [ApiClientError::Network("offline".into()), ApiClientError::Api(ApiError { status: 502, message: "down".into(), code: Some("lastfm_unreachable".into()) })] {
            *fake.result.borrow_mut() = Some(error);
            assert!(matches!(block_on(sink(&fake).scrobble(&[entry()])), Err(LastFmCallError::Transport(_))));
        }
    }

    #[test]
    fn signed_out_is_a_retryable_failure() {
        let s: ServerScrobbleSink<Fake> = ServerScrobbleSink::new(|| None);
        assert_eq!(block_on(s.scrobble(&[entry()])), Err(LastFmCallError::Transport(TransportError("Not signed in to a server".into()))));
        assert!(block_on(s.now_playing(&NowPlayingTrack { artist: "A".into(), title: "T".into(), album: None, duration_seconds: None })).is_err());
    }
}
