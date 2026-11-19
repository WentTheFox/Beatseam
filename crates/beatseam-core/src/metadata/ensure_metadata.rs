use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use futures_util::join;

use super::content_hash::hash_bytes;
use super::parse_duration::{DURATION_PREFIX_BYTES, duration_prefix_bytes_needed, parse_duration_seconds};
use super::read_tags::{ParsedTags, TagReader};
use super::types::{CoverArtBytes, TrackMetadata};
use crate::file_access::{CallOptions, FileAccess, FileAccessError, FileRef};
use crate::http::{HttpRequest, HttpResponse, HttpTransport, Method, TransportError};
use crate::library_store::{LibraryStore, StoreError};
use crate::tasks::Gate;

/// Bumped whenever tag reading changes, so already-scanned files get re-read instead of keeping a stale result forever -
/// same role as `ANALYSIS_ALGORITHM_VERSION`. Shared with the TypeScript apps (it is stored in the same database), so it
/// only moves when the *behaviour* does. (v2: also extracts cover art. v3: downscales/cuts off oversized art instead of
/// storing it verbatim. v4: content hash. v5: duration. v6: duration for MP3s whose ID3 tag outgrows the default
/// duration prefix, which had stored null.)
pub const METADATA_PARSER_VERSION: u32 = 6;

/// Cover art is only ever displayed at small thumbnail sizes - shrink it toward this before storing.
pub const COVER_ART_MAX_DIMENSION_PX: u32 = 300;

/// Hard fallback for any track a resizer isn't available for, or doesn't manage to shrink enough: some embedded art is a
/// needlessly full-resolution scan (a real one seen in testing: ~1 MB raw) despite only ever being displayed at a small
/// thumbnail size - drop it entirely rather than let a handful of oversized outliers dominate the library's storage
/// footprint. A file over this simply gets no stored art, same as one with none.
const MAX_COVER_ART_BYTES: usize = 500_000;

/// Platform-specific cover art downscaling (web: Canvas; Android: the platform bitmap APIs; desktop: the OS imaging
/// stack, or the `image` crate) - implemented per platform so `beatseam-core` stays platform-agnostic and testable.
/// Optional: metadata reading falls back to a hard byte-size cutoff for any platform/track this doesn't shrink enough.
pub trait CoverArtResizer {
    /// Resizes/re-encodes image bytes so neither dimension exceeds `max_dimension_px`, returning `None` when resizing
    /// didn't happen (already small enough, or the platform couldn't decode/re-encode it) - the caller then falls back to
    /// the original bytes. The result is `(mime type, bytes)`.
    fn resize(&self, bytes: &[u8], mime_type: &str, max_dimension_px: u32) -> impl Future<Output = Option<(String, Vec<u8>)>>;
}

/// "No resizer": oversized art is simply dropped (the byte-size cutoff).
pub struct NoResizer;

impl CoverArtResizer for NoResizer {
    async fn resize(&self, _bytes: &[u8], _mime_type: &str, _max_dimension_px: u32) -> Option<(String, Vec<u8>)> {
        None
    }
}

/// "No HTTP": for adapters that never hand out a stream URL (everything but the server-backed one).
pub struct NoHttp;

impl HttpTransport for NoHttp {
    async fn send(&self, _request: HttpRequest) -> Result<HttpResponse, TransportError> {
        Err(TransportError("this metadata service has no HTTP transport".into()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetadataError {
    FileAccess(FileAccessError),
    Store(StoreError),
    /// Another caller was reading this same file and was dropped before it finished.
    ReadAbandoned,
}

impl fmt::Display for MetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileAccess(e) => e.fmt(f),
            Self::Store(e) => e.fmt(f),
            Self::ReadAbandoned => f.write_str("the read this call was waiting on was abandoned"),
        }
    }
}

impl std::error::Error for MetadataError {}

impl From<FileAccessError> for MetadataError {
    fn from(e: FileAccessError) -> Self {
        Self::FileAccess(e)
    }
}

impl From<StoreError> for MetadataError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

/// True if a stored [`TrackMetadata`] can still be trusted for the given file - see its field docs and the analysis
/// freshness check (the same pattern, for analysis results).
pub fn is_metadata_fresh(existing: Option<&TrackMetadata>, size_bytes: u64, last_modified_ms: f64) -> bool {
    existing.is_some_and(|m| m.size_bytes == size_bytes && m.last_modified_ms == last_modified_ms && m.parser_version == METADATA_PARSER_VERSION)
}

/// Weaker than [`is_metadata_fresh`] - doesn't need the file's current size/mtime, just whether the metadata came from
/// the current parser version. The UI displays a result immediately even when it's from an older version (so
/// title/artist don't regress to the filename while a background rescan catches up to this track), so code gating some
/// OTHER lazy fetch on "metadata is confirmed current" (cover art) needs this instead of a plain `Some` check - the
/// stale-but-present case is exactly when that other fetch would race ahead of the rescan and never retry.
pub fn is_metadata_current(metadata: Option<&TrackMetadata>) -> bool {
    metadata.is_some_and(|m| m.parser_version == METADATA_PARSER_VERSION)
}

/// One read in progress, which later callers for the same file join.
struct Flight {
    done: Gate,
    result: Mutex<Option<Result<TrackMetadata, MetadataError>>>,
}

/// Reads and caches track metadata: tags, duration, content hash and cover art. Create one per app and share it - it
/// deduplicates concurrent reads of the same file.
///
/// Two independent library scans over overlapping tracks (a background refresh and a Rescan, both idle-chunked) can land on
/// the same file at roughly the same time, especially right after a [`METADATA_PARSER_VERSION`] bump makes a real
/// library's entire backlog "not fresh" at once. Without joining them, both would independently read/parse/hash the same
/// file concurrently (observed on a large self-hosted library: two notification rows advancing in lockstep over the same
/// tracks). Keyed by file id only, not by `force_refresh`: a forced call joining an in-flight non-forced read still gets a
/// genuinely fresh result, since that read only exists because a real read was already underway.
pub struct MetadataService<F, S, T, H = NoHttp, R = NoResizer> {
    file_access: Arc<F>,
    store: Arc<S>,
    tags: Arc<T>,
    http: Arc<H>,
    resizer: Arc<R>,
    in_flight: Mutex<HashMap<String, Arc<Flight>>>,
}

impl<F: FileAccess, S: LibraryStore, T: TagReader> MetadataService<F, S, T> {
    /// A service with no HTTP transport and no resizer (the local-file case).
    pub fn new(file_access: Arc<F>, store: Arc<S>, tags: Arc<T>) -> Self {
        Self::with_parts(file_access, store, tags, Arc::new(NoHttp), Arc::new(NoResizer))
    }
}

impl<F: FileAccess, S: LibraryStore, T: TagReader, H: HttpTransport, R: CoverArtResizer> MetadataService<F, S, T, H, R> {
    /// `http` is used only to fetch header prefixes for adapters that give out a stream URL; `resizer` downscales
    /// oversized embedded art before it is stored.
    pub fn with_parts(file_access: Arc<F>, store: Arc<S>, tags: Arc<T>, http: Arc<H>, resizer: Arc<R>) -> Self {
        Self { file_access, store, tags, http, resizer, in_flight: Mutex::new(HashMap::new()) }
    }

    pub fn store(&self) -> &Arc<S> {
        &self.store
    }

    /// Returns the track's cached metadata if still fresh ([`is_metadata_fresh`]), otherwise reads the file's bytes, parses
    /// whatever tags it has (a file with none still gets a stored result - no title/album, no artists - so callers can tell
    /// "scanned, no tags" apart from "not scanned yet" without re-reading the file every time), and persists it.
    ///
    /// `force_refresh` skips the freshness short-circuit and always re-reads/re-parses/re-hashes - the only way to catch a
    /// file whose content changed but whose size and mtime happen not to (a sync tool preserving timestamps, e.g.). Not
    /// used by the passive background scan; only a caller with an explicit, user-initiated reason to suspect this exact
    /// file changed should set it.
    // The lock is released (`drop(in_flight)`) before the await; clippy can't see that through the early return.
    #[allow(clippy::await_holding_lock)]
    pub async fn ensure_track_metadata(&self, file: &FileRef, force_refresh: bool) -> Result<TrackMetadata, MetadataError> {
        let flight = {
            let mut in_flight = self.in_flight.lock().unwrap();
            if let Some(existing) = in_flight.get(&file.id) {
                let existing = existing.clone();
                drop(in_flight);
                existing.done.wait().await;
                return existing.result.lock().unwrap().clone().unwrap_or(Err(MetadataError::ReadAbandoned));
            }
            let flight = Arc::new(Flight { done: Gate::new(), result: Mutex::new(None) });
            in_flight.insert(file.id.clone(), flight.clone());
            flight
        };
        // Whether this call finishes or is dropped, the flight is removed and its waiters are released.
        let _end = FlightEnd { service: &self.in_flight, id: file.id.clone(), flight: flight.clone() };
        let result = self.read_uncached(file, force_refresh).await;
        *flight.result.lock().unwrap() = Some(result.clone());
        result
    }

    async fn read_uncached(&self, file: &FileRef, force_refresh: bool) -> Result<TrackMetadata, MetadataError> {
        let existing = self.store.get_metadata(&file.id).await?;
        if !force_refresh && is_metadata_fresh(existing.as_ref(), file.size_bytes, file.last_modified_ms) {
            return Ok(existing.expect("fresh implies present"));
        }

        // Prefer a ranged HTTP read over the file's full bytes when the adapter can give us one (`stream_url`): metadata
        // scanning otherwise means downloading every track's entire audio payload just to read a header, which is both
        // wasted bandwidth and (self-hosted, over a real network) the dominant cost of an initial library scan. The
        // direct-bytes path hashes those bytes itself and slices its own prefix for the duration parser (both free - the
        // bytes are already in hand); the stream-URL path asks the adapter for a hash instead (the server hashes its own
        // local disk copy, so the client never downloads the file just for this) and fetches a small Range request for
        // the header prefix - still far cheaper than a full download, just not free.
        let (tags, content_hash, duration_seconds): (Option<ParsedTags>, Option<String>, Option<f64>) = if let Some(url) = self.file_access.stream_url(file) {
            let (hash, prefix) = join!(self.file_access.content_hash(file), self.fetch_prefix(&url, DURATION_PREFIX_BYTES));
            let mut prefix = prefix;
            // A huge ID3 tag pushes the first MP3 frame past the default prefix - fetch just enough more.
            if let Some(p) = &prefix {
                let needed = duration_prefix_bytes_needed(p, &file.name);
                if needed > p.len() {
                    prefix = self.fetch_prefix(&url, needed).await;
                }
            }
            let duration = prefix.as_deref().and_then(|p| parse_duration_seconds(p, &file.name, file.size_bytes));
            // Tags come from the same prefix (the TypeScript's `jsmediatags` issued its own ranged reads for the header and
            // footer chunks): a tag that lies beyond the prefix is not found.
            let tags = prefix.as_deref().and_then(|p| self.tags.read_tags(p));
            (tags, hash?, duration)
        } else {
            let bytes = self.file_access.read_file_bytes(file, CallOptions::default()).await?;
            let hash = hash_bytes(&bytes);
            let prefix_len = duration_prefix_bytes_needed(&bytes, &file.name).min(bytes.len());
            let duration = parse_duration_seconds(&bytes[..prefix_len], &file.name, file.size_bytes);
            (self.tags.read_tags(&bytes), Some(hash), duration)
        };

        let result = TrackMetadata {
            file_id: file.id.clone(),
            title: tags.as_ref().and_then(|t| t.title.clone()),
            artists: tags.as_ref().map(|t| t.artists.clone()).unwrap_or_default(),
            album: tags.as_ref().and_then(|t| t.album.clone()),
            duration_seconds,
            size_bytes: file.size_bytes,
            last_modified_ms: file.last_modified_ms,
            parser_version: METADATA_PARSER_VERSION,
            content_hash,
        };
        // Cover art first, metadata second - deliberately in this order. Once the metadata's parser version reads as
        // current, `is_metadata_current` is the signal the cover-art lookup gates itself on, and that lookup permanently
        // caches whatever it finds (including a genuine "no art") with no retry. Writing metadata first would leave a
        // window where a poll could observe "current metadata, art not written yet" and cache a false negative forever.
        // Always written, even when `None` - clears stale art from a prior scan if the file changed and no longer has
        // any (or never did).
        let art = self.resolve_cover_art(tags.and_then(|t| t.cover_art)).await;
        self.store.put_cover_art(&file.id, art).await?;
        self.store.put_metadata(result.clone()).await?;
        Ok(result)
    }

    /// Downscales art via the resizer when it's larger than the byte cut-off (the resizer itself treats "already small
    /// enough" as a no-op, returning `None` - this only calls it at all to avoid the overhead when the cut-off alone will
    /// pass it through untouched anyway), falls back to the raw bytes if it declines, and finally drops anything still over
    /// [`MAX_COVER_ART_BYTES`]. Returns raw bytes, not an encoded string - it's up to each store adapter how to persist and
    /// serve them.
    async fn resolve_cover_art(&self, art: Option<CoverArtBytes>) -> Option<CoverArtBytes> {
        let CoverArtBytes { mut mime_type, mut data } = art?;
        if data.len() > MAX_COVER_ART_BYTES
            && let Some((resized_mime, resized)) = self.resizer.resize(&data, &mime_type, COVER_ART_MAX_DIMENSION_PX).await {
                mime_type = resized_mime;
                data = resized;
            }
        (data.len() <= MAX_COVER_ART_BYTES).then_some(CoverArtBytes { mime_type, data })
    }

    /// Fetches up to `max_bytes` from the start of `url` via a Range request - used to get the duration parser a prefix
    /// without downloading the whole file. The server's static file mount honours Range, so this is a real partial
    /// transfer there; the body is truncated regardless, in case some other source ignores Range and returns everything.
    async fn fetch_prefix(&self, url: &str, max_bytes: usize) -> Option<Vec<u8>> {
        let mut request = HttpRequest::new(Method::Get, url);
        request.headers.push(("Range".into(), format!("bytes=0-{}", max_bytes - 1)));
        let response = self.http.send(request).await.ok()?;
        if !response.ok() {
            return None;
        }
        let mut body = response.body;
        body.truncate(max_bytes);
        Some(body)
    }
}

struct FlightEnd<'a> {
    service: &'a Mutex<HashMap<String, Arc<Flight>>>,
    id: String,
    flight: Arc<Flight>,
}

impl Drop for FlightEnd<'_> {
    fn drop(&mut self) {
        self.service.lock().unwrap().remove(&self.id);
        self.flight.done.open();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::file_access::{DirectoryEntry, GrantedRoot, MemoryFileAccess, RootKind};
    use crate::http::block_on;
    use crate::library_store::MemoryLibraryStore;
    use crate::tasks::test_executor::{Pool, yield_now};

    /// A stand-in tag format for these tests (the real parsers live behind `TagReader`): text lines `title=`, `artist=`,
    /// `album=`, and `cover=<mime>:<byte count>`.
    fn fake_tags(bytes: &[u8]) -> Option<ParsedTags> {
        let text = std::str::from_utf8(bytes).ok()?;
        let mut tags = ParsedTags::default();
        let mut any = false;
        for line in text.lines() {
            match line.split_once('=') {
                Some(("title", v)) => (tags.title, any) = (Some(v.into()), true),
                Some(("artist", v)) => (tags.artists, any) = (crate::metadata::split_artists(v), true),
                Some(("album", v)) => (tags.album, any) = (Some(v.into()), true),
                Some(("cover", v)) => {
                    let (mime, n) = v.split_once(':')?;
                    (tags.cover_art, any) = (Some(CoverArtBytes { mime_type: mime.into(), data: (0..n.parse::<usize>().ok()?).map(|i| (i % 251) as u8).collect() }), true);
                }
                _ => {}
            }
        }
        any.then_some(tags)
    }

    fn file(id: &str, size: u64, mtime: f64) -> FileRef {
        FileRef { id: id.into(), name: format!("{id}.mp3"), relative_path: format!("{id}.mp3"), size_bytes: size, last_modified_ms: mtime }
    }

    type Svc<R = NoResizer> = MetadataService<MemoryFileAccess, MemoryLibraryStore, fn(&[u8]) -> Option<ParsedTags>, NoHttp, R>;

    fn service(files: &[(&str, &str)]) -> Svc {
        MetadataService::with_parts(Arc::new(MemoryFileAccess::new(files.iter().copied())), Arc::new(MemoryLibraryStore::new()), Arc::new(fake_tags as fn(&[u8]) -> Option<ParsedTags>), Arc::new(NoHttp), Arc::new(NoResizer))
    }

    fn ensure(s: &Svc, f: &FileRef, force: bool) -> TrackMetadata {
        block_on(s.ensure_track_metadata(f, force)).unwrap()
    }

    #[test]
    fn reads_title_artist_and_album_and_persists_the_result() {
        let s = service(&[("a.mp3", "title=Song Title\nartist=The Artist\nalbum=Great Album")]);
        let result = ensure(&s, &file("a", 1000, 5.0), false);
        assert_eq!((result.title.as_deref(), result.artists.as_slice(), result.album.as_deref()), (Some("Song Title"), ["The Artist".to_string()].as_slice(), Some("Great Album")));
        assert_eq!(block_on(s.store().get_metadata("a")).unwrap(), Some(result.clone()));
        assert_eq!(result.parser_version, METADATA_PARSER_VERSION);
        assert_eq!((result.size_bytes, result.last_modified_ms), (1000, 5.0));
    }

    #[test]
    fn stores_the_duration_of_an_mp3_whose_id3_tag_is_larger_than_the_default_prefix() {
        // Regression: a big embedded cover (900 KB of tag) pushes the first MPEG frame past the 512 KB prefix, which
        // used to store a null duration for a perfectly normal song.
        let tag_size = 900_000usize;
        let audio: Vec<u8> = [vec![0xff, 0xfb, 0x90, 0x00], vec![0; 1000]].concat(); // a CBR 128 kbps/44.1 kHz frame header, no Xing
        let mut bytes = vec![0u8; 10 + tag_size + audio.len()];
        bytes[..10].copy_from_slice(&[0x49, 0x44, 0x33, 3, 0, 0, (tag_size >> 21) as u8 & 0x7f, (tag_size >> 14) as u8 & 0x7f, (tag_size >> 7) as u8 & 0x7f, tag_size as u8 & 0x7f]);
        bytes[10 + tag_size..].copy_from_slice(&audio);
        // MemoryFileAccess stores text, so hand the bytes over through a one-off file access instead.
        struct Bytes(Vec<u8>);
        impl FileAccess for Bytes {
            async fn request_root(&self, _k: RootKind) -> Result<Option<GrantedRoot>, FileAccessError> {
                Ok(None)
            }
            async fn list_granted_roots(&self) -> Result<Vec<GrantedRoot>, FileAccessError> {
                Ok(vec![])
            }
            async fn revoke_root(&self, _r: &str) -> Result<(), FileAccessError> {
                Ok(())
            }
            async fn list_directory(&self, _r: &str, _p: Option<&str>, _o: CallOptions) -> Result<Vec<DirectoryEntry>, FileAccessError> {
                Ok(vec![])
            }
            async fn read_file_bytes(&self, _f: &FileRef, _o: CallOptions) -> Result<Vec<u8>, FileAccessError> {
                Ok(self.0.clone())
            }
            async fn read_file_text(&self, _f: &FileRef, _o: CallOptions) -> Result<String, FileAccessError> {
                Ok(String::new())
            }
            async fn write_file_text(&self, _r: &str, _p: &str, _c: &str) -> Result<(), FileAccessError> {
                Ok(())
            }
        }
        let s = MetadataService::new(Arc::new(Bytes(bytes.clone())), Arc::new(MemoryLibraryStore::new()), Arc::new(|_: &[u8]| None::<ParsedTags>));
        let big = FileRef { id: "big".into(), name: "big.mp3".into(), relative_path: "big.mp3".into(), size_bytes: bytes.len() as u64, last_modified_ms: 1.0 };
        let result = block_on(s.ensure_track_metadata(&big, false)).unwrap();
        // CBR estimate: the 1004 bytes after the tag at 128 kbps.
        assert!((result.duration_seconds.unwrap() - 1004.0 * 8.0 / 128_000.0).abs() < 1e-4);
    }

    #[test]
    fn extracts_embedded_cover_art_as_a_data_uri() {
        let s = service(&[("a.mp3", "title=Song\ncover=image/jpeg:8")]);
        ensure(&s, &file("a", 1000, 5.0), false);
        let art = block_on(s.store().get_cover_art("a")).unwrap().unwrap();
        assert!(art.starts_with("data:image/jpeg;base64,"), "{art}");
    }

    #[test]
    fn drops_art_over_the_size_cutoff_when_no_resizer_is_given() {
        let s = service(&[("a.mp3", "title=Song\ncover=image/jpeg:600000")]);
        ensure(&s, &file("a", 1000, 5.0), false);
        assert_eq!(block_on(s.store().get_cover_art("a")).unwrap(), None);
    }

    /// Records what it was asked and answers with a canned result.
    struct Recording {
        calls: Mutex<Vec<(String, usize, u32)>>,
        answer: Option<(String, Vec<u8>)>,
    }

    impl CoverArtResizer for Recording {
        async fn resize(&self, bytes: &[u8], mime_type: &str, max_dimension_px: u32) -> Option<(String, Vec<u8>)> {
            self.calls.lock().unwrap().push((mime_type.into(), bytes.len(), max_dimension_px));
            self.answer.clone()
        }
    }

    fn with_resizer(files: &[(&str, &str)], resizer: Recording) -> (Svc<Recording>, Arc<Recording>) {
        let resizer = Arc::new(resizer);
        (MetadataService::with_parts(Arc::new(MemoryFileAccess::new(files.iter().copied())), Arc::new(MemoryLibraryStore::new()), Arc::new(fake_tags as fn(&[u8]) -> Option<ParsedTags>), Arc::new(NoHttp), resizer.clone()), resizer)
    }

    #[test]
    fn asks_the_resizer_to_shrink_art_over_the_cutoff_and_stores_the_result() {
        let (s, r) = with_resizer(&[("a.mp3", "title=Song\ncover=image/jpeg:600000")], Recording { calls: Mutex::default(), answer: Some(("image/png".into(), vec![9, 9, 9])) });
        block_on(s.ensure_track_metadata(&file("a", 1000, 5.0), false)).unwrap();
        assert_eq!(*r.calls.lock().unwrap(), [("image/jpeg".to_string(), 600_000, COVER_ART_MAX_DIMENSION_PX)]);
        assert_eq!(block_on(s.store().get_cover_art("a")).unwrap().as_deref(), Some("data:image/png;base64,CQkJ"));
    }

    #[test]
    fn still_drops_art_if_the_resizer_declines_and_the_original_is_over_the_cutoff() {
        let (s, _) = with_resizer(&[("a.mp3", "title=Song\ncover=image/jpeg:600000")], Recording { calls: Mutex::default(), answer: None });
        block_on(s.ensure_track_metadata(&file("a", 1000, 5.0), false)).unwrap();
        assert_eq!(block_on(s.store().get_cover_art("a")).unwrap(), None);
    }

    #[test]
    fn does_not_call_the_resizer_for_art_already_under_the_cutoff() {
        let (s, r) = with_resizer(&[("a.mp3", "title=Song\ncover=image/jpeg:3")], Recording { calls: Mutex::default(), answer: None });
        block_on(s.ensure_track_metadata(&file("a", 1000, 5.0), false)).unwrap();
        assert!(r.calls.lock().unwrap().is_empty());
        assert!(block_on(s.store().get_cover_art("a")).unwrap().unwrap().starts_with("data:image/jpeg;base64,"));
    }

    #[test]
    fn clears_previously_stored_art_when_a_rescanned_file_no_longer_has_any() {
        let s = service(&[("a.mp3", "title=Song\ncover=image/png:3")]);
        ensure(&s, &file("a", 1000, 5.0), false);
        assert!(block_on(s.store().get_cover_art("a")).unwrap().is_some());
        s.file_access.add_file("a.mp3", "title=Song");
        ensure(&s, &file("a", 2000, 42.0), false);
        assert_eq!(block_on(s.store().get_cover_art("a")).unwrap(), None);
    }

    #[test]
    fn splits_a_slash_delimited_multi_artist_value() {
        let s = service(&[("a.mp3", "title=Song\nartist=Artist One/Artist Two")]);
        assert_eq!(ensure(&s, &file("a", 1000, 5.0), false).artists, ["Artist One", "Artist Two"]);
    }

    #[test]
    fn persists_a_scanned_no_tags_result_for_a_file_with_no_tag() {
        let s = service(&[("a.mp3", "no tags here")]);
        let result = ensure(&s, &file("a", 1000, 5.0), false);
        assert_eq!((result.title, result.artists, result.album), (None, vec![], None));
        assert!(block_on(s.store().get_metadata("a")).unwrap().is_some(), "stored, so callers can tell it from 'not scanned yet'");
    }

    #[test]
    fn returns_the_cached_result_without_re_reading_when_size_and_mtime_match() {
        let s = service(&[("a.mp3", "title=Song")]);
        let first = ensure(&s, &file("a", 1000, 5.0), false);
        s.file_access.remove_file("a.mp3"); // a read would now fail, so a hit proves the file was not read
        assert_eq!(ensure(&s, &file("a", 1000, 5.0), false), first);
    }

    #[test]
    fn re_reads_when_the_file_has_changed() {
        let s = service(&[("a.mp3", "title=Old Title")]);
        ensure(&s, &file("a", 1000, 5.0), false);
        s.file_access.add_file("a.mp3", "title=New Title");
        let result = ensure(&s, &file("a", 2000, 42.0), false);
        assert_eq!((result.title.as_deref(), result.size_bytes, result.last_modified_ms), (Some("New Title"), 2000, 42.0));
    }

    #[test]
    fn freshness_needs_the_current_parser_version_and_an_exact_size_and_mtime() {
        let stale = TrackMetadata { file_id: "a".into(), title: Some("x".into()), artists: vec![], album: None, duration_seconds: None, size_bytes: 1000, last_modified_ms: 5.0, parser_version: METADATA_PARSER_VERSION - 1, content_hash: None };
        assert!(!is_metadata_fresh(Some(&stale), 1000, 5.0));
        assert!(!is_metadata_current(Some(&stale)));
        let current = TrackMetadata { parser_version: METADATA_PARSER_VERSION, ..stale };
        assert!(is_metadata_fresh(Some(&current), 1000, 5.0) && is_metadata_current(Some(&current)));
        assert!(!is_metadata_fresh(Some(&current), 1001, 5.0) && !is_metadata_fresh(Some(&current), 1000, 5.5));
        assert!(!is_metadata_fresh(None, 1000, 5.0) && !is_metadata_current(None));
    }

    #[test]
    fn stores_a_content_hash_of_the_full_file_bytes() {
        let s = service(&[("a.mp3", "title=Song Title")]);
        assert_eq!(ensure(&s, &file("a", 1000, 5.0), false).content_hash, Some(hash_bytes(b"title=Song Title")));
    }

    #[test]
    fn force_refresh_re_reads_and_re_hashes_even_when_size_and_mtime_still_match() {
        let s = service(&[("a.mp3", "title=Old Title")]);
        ensure(&s, &file("a", 1000, 5.0), false);
        s.file_access.add_file("a.mp3", "title=Swapped Title"); // same size/mtime as far as the caller's ref says
        let unforced = ensure(&s, &file("a", 1000, 5.0), false);
        assert_eq!(unforced.title.as_deref(), Some("Old Title"));
        let forced = ensure(&s, &file("a", 1000, 5.0), true);
        assert_eq!(forced.title.as_deref(), Some("Swapped Title"));
        assert_eq!(forced.content_hash, Some(hash_bytes(b"title=Swapped Title")));
        assert_ne!(forced.content_hash, unforced.content_hash);
    }

    #[test]
    fn dedupes_two_concurrent_calls_for_the_same_file_into_a_single_real_read() {
        // Counts reads, and takes a scheduler turn per read so the second call arrives while the first is in flight.
        struct Counting {
            inner: MemoryFileAccess,
            reads: Arc<AtomicUsize>,
        }
        impl FileAccess for Counting {
            async fn request_root(&self, k: RootKind) -> Result<Option<GrantedRoot>, FileAccessError> {
                self.inner.request_root(k).await
            }
            async fn list_granted_roots(&self) -> Result<Vec<GrantedRoot>, FileAccessError> {
                self.inner.list_granted_roots().await
            }
            async fn revoke_root(&self, r: &str) -> Result<(), FileAccessError> {
                self.inner.revoke_root(r).await
            }
            async fn list_directory(&self, r: &str, p: Option<&str>, o: CallOptions) -> Result<Vec<DirectoryEntry>, FileAccessError> {
                self.inner.list_directory(r, p, o).await
            }
            async fn read_file_bytes(&self, f: &FileRef, o: CallOptions) -> Result<Vec<u8>, FileAccessError> {
                self.reads.fetch_add(1, Ordering::SeqCst);
                yield_now().await;
                self.inner.read_file_bytes(f, o).await
            }
            async fn read_file_text(&self, f: &FileRef, o: CallOptions) -> Result<String, FileAccessError> {
                self.inner.read_file_text(f, o).await
            }
            async fn write_file_text(&self, r: &str, p: &str, c: &str) -> Result<(), FileAccessError> {
                self.inner.write_file_text(r, p, c).await
            }
        }
        let reads = Arc::new(AtomicUsize::new(0));
        let s = Arc::new(MetadataService::new(Arc::new(Counting { inner: MemoryFileAccess::new([("a.mp3", "title=Song Title")]), reads: reads.clone() }), Arc::new(MemoryLibraryStore::new()), Arc::new(fake_tags as fn(&[u8]) -> Option<ParsedTags>)));
        let pool = Pool::new();
        let (s1, s2) = (s.clone(), s.clone());
        let first = pool.spawn(async move { s1.ensure_track_metadata(&file("a", 1000, 5.0), false).await });
        let second = pool.spawn(async move { s2.ensure_track_metadata(&file("a", 1000, 5.0), false).await });
        pool.run_until_stalled();
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        let (first, second) = (first.take().unwrap().unwrap(), second.take().unwrap().unwrap());
        assert_eq!(first, second);
        assert_eq!(first.title.as_deref(), Some("Song Title"));
    }

    #[test]
    fn a_waiter_is_told_when_the_read_it_joined_was_dropped() {
        struct Stalls;
        impl FileAccess for Stalls {
            async fn request_root(&self, _k: RootKind) -> Result<Option<GrantedRoot>, FileAccessError> {
                Ok(None)
            }
            async fn list_granted_roots(&self) -> Result<Vec<GrantedRoot>, FileAccessError> {
                Ok(vec![])
            }
            async fn revoke_root(&self, _r: &str) -> Result<(), FileAccessError> {
                Ok(())
            }
            async fn list_directory(&self, _r: &str, _p: Option<&str>, _o: CallOptions) -> Result<Vec<DirectoryEntry>, FileAccessError> {
                Ok(vec![])
            }
            async fn read_file_bytes(&self, _f: &FileRef, _o: CallOptions) -> Result<Vec<u8>, FileAccessError> {
                Gate::new().wait().await; // never completes
                Ok(vec![])
            }
            async fn read_file_text(&self, _f: &FileRef, _o: CallOptions) -> Result<String, FileAccessError> {
                Ok(String::new())
            }
            async fn write_file_text(&self, _r: &str, _p: &str, _c: &str) -> Result<(), FileAccessError> {
                Ok(())
            }
        }
        let s = Arc::new(MetadataService::new(Arc::new(Stalls), Arc::new(MemoryLibraryStore::new()), Arc::new(fake_tags as fn(&[u8]) -> Option<ParsedTags>)));
        let pool = Pool::new();
        let (s1, s2) = (s.clone(), s.clone());
        let leader = pool.spawn(async move { s1.ensure_track_metadata(&file("a", 1, 1.0), false).await });
        let waiter = pool.spawn(async move { s2.ensure_track_metadata(&file("a", 1, 1.0), false).await });
        pool.run_until_stalled();
        assert!(waiter.take().is_none(), "still waiting on the stalled read");
        leader.cancel();
        pool.run_until_stalled();
        assert_eq!(waiter.take(), Some(Err(MetadataError::ReadAbandoned)));
    }
}
