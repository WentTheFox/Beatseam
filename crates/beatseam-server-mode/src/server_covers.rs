use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};

use beatseam_api::{ApiClient, ApiClientError, encode_track_key};
use beatseam_core::http::HttpTransport;
use beatseam_core::library_store::{LibraryStore, StoreError};
use beatseam_core::metadata::CoverArtBytes;
use beatseam_core::tasks::Gate;

/// Why a cover could not be fetched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoverFetchError {
    /// The server has no art for this track.
    NotFound,
    /// The server can't be reached.
    Network,
    Other(String),
}

/// The server's cover endpoint.
pub trait CoverFetch {
    /// The art's bytes and content type, if there is a server connection; `None` when signed out.
    fn fetch_cover(&self, file_id: &str) -> impl Future<Output = Option<Result<(Vec<u8>, String), CoverFetchError>>>;
}

impl<T: HttpTransport> CoverFetch for ApiClient<T> {
    async fn fetch_cover(&self, file_id: &str) -> Option<Result<(Vec<u8>, String), CoverFetchError>> {
        Some(self.fetch_file(&self.cover_path(&encode_track_key(file_id)), None).await.map_err(|e| match e {
            ApiClientError::Api(api) if api.status == 404 => CoverFetchError::NotFound,
            ApiClientError::Network(_) => CoverFetchError::Network,
            other => CoverFetchError::Other(other.to_string()),
        }))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoverError {
    Store(StoreError),
    Fetch(String),
}

impl fmt::Display for CoverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => e.fmt(f),
            Self::Fetch(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for CoverError {}

impl From<StoreError> for CoverError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

struct Flight {
    done: Gate,
    result: Mutex<Option<Result<Option<String>, CoverError>>>,
}

#[derive(Default)]
struct State {
    in_flight: HashMap<String, Arc<Flight>>,
    confirmed_none: HashSet<String>,
}

/// Cover art for a library that comes from a server: fetched the first time something asks for it, then kept like any other
/// cover. The sync deliberately doesn't download art up front: a library has thousands of tracks and the screen only ever
/// shows a few dozen. Where the TypeScript wrapped the whole store in a proxy, callers here ask this for covers instead of
/// the store directly. Share one per app: it holds the in-flight fetches and the "no art" answers of this session.
pub struct ServerCovers<F> {
    fetcher: F,
    is_active: Box<dyn Fn() -> bool>,
    state: Mutex<State>,
}

struct FlightEnd<'a> {
    state: &'a Mutex<State>,
    file_id: String,
    flight: Arc<Flight>,
}

impl Drop for FlightEnd<'_> {
    fn drop(&mut self) {
        self.state.lock().unwrap().in_flight.remove(&self.file_id);
        self.flight.done.open();
    }
}

impl<F: CoverFetch> ServerCovers<F> {
    pub fn new(fetcher: F, is_active: impl Fn() -> bool + 'static) -> Self {
        Self { fetcher, is_active: Box::new(is_active), state: Mutex::new(State::default()) }
    }

    /// The stored cover's URI, fetching it from the server first when there is none and server mode is on. `None` for a
    /// track with no art, or when the server can't be asked right now (not remembered, so it is tried again next time).
    pub async fn get_cover_art<S: LibraryStore>(&self, store: &S, file_id: &str) -> Result<Option<String>, CoverError> {
        let stored = store.get_cover_art(file_id).await?;
        if stored.is_some() || !(self.is_active)() {
            return Ok(stored);
        }
        let running = self.state.lock().unwrap().in_flight.get(file_id).cloned();
        if let Some(flight) = running {
            flight.done.wait().await;
            return flight.result.lock().unwrap().clone().unwrap_or(Ok(None));
        }
        let flight = Arc::new(Flight { done: Gate::new(), result: Mutex::new(None) });
        self.state.lock().unwrap().in_flight.insert(file_id.to_owned(), flight.clone());
        let _end = FlightEnd { state: &self.state, file_id: file_id.to_owned(), flight: flight.clone() };
        let result = self.fetch_cover(store, file_id).await;
        *flight.result.lock().unwrap() = Some(result.clone());
        result
    }

    async fn fetch_cover<S: LibraryStore>(&self, store: &S, file_id: &str) -> Result<Option<String>, CoverError> {
        if self.state.lock().unwrap().confirmed_none.contains(file_id) {
            return Ok(None);
        }
        match self.fetcher.fetch_cover(file_id).await {
            None => Ok(None),
            Some(Ok((bytes, content_type))) => {
                let mime_type = content_type.split(';').next().unwrap_or_default().trim().to_owned();
                store.put_cover_art(file_id, Some(CoverArtBytes { mime_type, data: bytes })).await?;
                Ok(store.get_cover_art(file_id).await?)
            }
            Some(Err(CoverFetchError::NotFound)) => {
                // A track with no art: don't ask again this session.
                self.state.lock().unwrap().confirmed_none.insert(file_id.to_owned());
                Ok(None)
            }
            Some(Err(CoverFetchError::Network)) => Ok(None), // offline: not remembered, so it's tried again next time
            Some(Err(CoverFetchError::Other(message))) => Err(CoverError::Fetch(message)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;

    use beatseam_core::http::block_on;
    use beatseam_core::library_store::MemoryLibraryStore;
    use beatseam_core::tasks::test_executor::Pool;

    use super::*;

    #[derive(Default)]
    struct Fake {
        script: RefCell<VecDeque<Option<Result<(Vec<u8>, String), CoverFetchError>>>>,
        calls: Cell<usize>,
        hold: RefCell<Option<Gate>>,
    }

    #[derive(Clone, Default)]
    struct Shared(Rc<Fake>);

    impl CoverFetch for Shared {
        async fn fetch_cover(&self, _file_id: &str) -> Option<Result<(Vec<u8>, String), CoverFetchError>> {
            self.0.calls.set(self.0.calls.get() + 1);
            let hold = self.0.hold.borrow().clone();
            if let Some(g) = hold {
                g.wait().await;
            }
            self.0.script.borrow_mut().pop_front().unwrap_or(Some(Ok((vec![1, 2, 3], "image/jpeg; charset=x".into()))))
        }
    }

    fn covers(fake: &Shared, active: bool) -> ServerCovers<Shared> {
        ServerCovers::new(fake.clone(), move || active)
    }

    #[test]
    fn fetches_once_stores_with_a_clean_mime_type_and_serves_the_stored_one_after() {
        let (fake, store) = (Shared::default(), MemoryLibraryStore::new());
        let c = covers(&fake, true);
        let first = block_on(c.get_cover_art(&store, "a")).unwrap();
        assert!(first.is_some());
        assert_eq!(block_on(c.get_cover_art(&store, "a")).unwrap(), first);
        assert_eq!(fake.0.calls.get(), 1);
    }

    #[test]
    fn does_nothing_outside_server_mode_or_when_signed_out() {
        let (fake, store) = (Shared::default(), MemoryLibraryStore::new());
        assert_eq!(block_on(covers(&fake, false).get_cover_art(&store, "a")).unwrap(), None);
        assert_eq!(fake.0.calls.get(), 0);
        fake.0.script.borrow_mut().push_back(None);
        assert_eq!(block_on(covers(&fake, true).get_cover_art(&store, "a")).unwrap(), None);
    }

    #[test]
    fn a_track_without_art_is_asked_about_once_but_being_offline_is_not_remembered() {
        let (fake, store) = (Shared::default(), MemoryLibraryStore::new());
        let c = covers(&fake, true);
        fake.0.script.borrow_mut().push_back(Some(Err(CoverFetchError::NotFound)));
        assert_eq!(block_on(c.get_cover_art(&store, "none")).unwrap(), None);
        assert_eq!(block_on(c.get_cover_art(&store, "none")).unwrap(), None);
        assert_eq!(fake.0.calls.get(), 1, "confirmed none for this session");

        fake.0.script.borrow_mut().push_back(Some(Err(CoverFetchError::Network)));
        assert_eq!(block_on(c.get_cover_art(&store, "off")).unwrap(), None);
        assert!(block_on(c.get_cover_art(&store, "off")).unwrap().is_some(), "tried again, and this time it arrived");
    }

    #[test]
    fn other_failures_are_reported() {
        let (fake, store) = (Shared::default(), MemoryLibraryStore::new());
        fake.0.script.borrow_mut().push_back(Some(Err(CoverFetchError::Other("boom".into()))));
        assert_eq!(block_on(covers(&fake, true).get_cover_art(&store, "a")).unwrap_err(), CoverError::Fetch("boom".into()));
    }

    #[test]
    fn concurrent_askers_share_one_fetch() {
        let (fake, store, pool) = (Shared::default(), Rc::new(MemoryLibraryStore::new()), Pool::new());
        *fake.0.hold.borrow_mut() = Some(Gate::new());
        let c = Rc::new(covers(&fake, true));
        let ask = || {
            let (c, s) = (c.clone(), store.clone());
            pool.spawn(async move { c.get_cover_art(&*s, "a").await })
        };
        let (one, two) = (ask(), ask());
        pool.run_until_stalled();
        assert_eq!(fake.0.calls.get(), 1);
        fake.0.hold.borrow().clone().unwrap().open();
        pool.run_until_stalled();
        let (a, b) = (one.take().unwrap().unwrap(), two.take().unwrap().unwrap());
        assert!(a.is_some() && a == b);
        assert_eq!(fake.0.calls.get(), 1);
    }
}
