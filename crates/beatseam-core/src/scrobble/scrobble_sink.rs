use super::lastfm::{LastFmCallError, LastFmClient, ScrobbleEntry};
use crate::http::HttpTransport;

pub struct NowPlayingTrack {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration_seconds: Option<f64>,
}

/// Where scrobbles and now-playing updates go once the tracker decides to send them: Last.fm directly (the
/// local-folder app, which holds its own session) or Beatseam's server (which holds the user's link). `scrobble` fails
/// with [`LastFmCallError::LastFm`] carrying a code when the service refused them for good (retrying would fail the
/// same way) and with [`LastFmCallError::Transport`] when trying again later could work.
pub trait ScrobbleSink {
    /// Where this sink's failed scrobbles wait for a retry; separate per sink so one account's plays never go to
    /// another's.
    fn queue_key(&self) -> &str;
    fn now_playing(&self, track: &NowPlayingTrack) -> impl Future<Output = Result<(), LastFmCallError>>;
    fn scrobble(&self, entries: &[ScrobbleEntry]) -> impl Future<Output = Result<(), LastFmCallError>>;
}

pub const LOCAL_SCROBBLE_QUEUE_KEY: &str = "scrobble.pendingQueue";

/// Straight to Last.fm with this device's own session.
pub struct LocalScrobbleSink<T: HttpTransport> {
    client: LastFmClient<T>,
    session_key: String,
}

impl<T: HttpTransport> LocalScrobbleSink<T> {
    pub fn new(client: LastFmClient<T>, session_key: impl Into<String>) -> Self {
        Self { client, session_key: session_key.into() }
    }
}

impl<T: HttpTransport> ScrobbleSink for LocalScrobbleSink<T> {
    fn queue_key(&self) -> &str {
        LOCAL_SCROBBLE_QUEUE_KEY
    }

    async fn now_playing(&self, track: &NowPlayingTrack) -> Result<(), LastFmCallError> {
        self.client.update_now_playing(&self.session_key, &track.artist, &track.title, track.album.as_deref(), track.duration_seconds).await
    }

    async fn scrobble(&self, entries: &[ScrobbleEntry]) -> Result<(), LastFmCallError> {
        self.client.scrobble_batch(&self.session_key, entries).await
    }
}

#[cfg(test)]
mod tests {
    use super::super::lastfm::LastFmCredentials;
    use crate::http::test_support::FakeTransport;
    use super::*;
    use crate::http::block_on;

    #[test]
    fn the_local_sink_uses_the_shared_queue_key_and_forwards_to_last_fm() {
        let transport = FakeTransport::json(200, "{}");
        let sink = LocalScrobbleSink::new(LastFmClient::new(LastFmCredentials::default(), &transport), "sk");
        assert_eq!(sink.queue_key(), "scrobble.pendingQueue");

        block_on(sink.now_playing(&NowPlayingTrack { artist: "A".into(), title: "T".into(), album: None, duration_seconds: Some(120.0) })).unwrap();
        block_on(sink.scrobble(&[ScrobbleEntry { artist: "A".into(), track: "T".into(), timestamp: 5, album: None, duration_seconds: None }])).unwrap();

        let requests = transport.requests.borrow();
        assert!(requests[0].body.as_ref().unwrap().contains("method=track.updateNowPlaying&sk=sk&artist=A&track=T&duration=120"));
        assert!(requests[1].body.as_ref().unwrap().contains("method=track.scrobble&sk=sk&artist%5B0%5D=A&track%5B0%5D=T&timestamp%5B0%5D=5"));
    }
}
