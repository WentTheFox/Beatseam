/// Raw cover art bytes, not yet turned into any particular storage/display representation - each platform's
/// `LibraryStore` adapter decides how to persist this and what kind of URI to hand back (a data: URI on Android and
/// Windows, a blob: object URL on web).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverArtBytes {
    pub mime_type: String,
    pub data: Vec<u8>,
}

/// ID3 (or other tag format) metadata read out of an audio file, tied to the file it was read from.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackMetadata {
    pub file_id: String,
    pub title: Option<String>,
    /// Multiple artists as stored (e.g. ID3v2.4's null-separated TPE1, or v2.3's "/"-separated form) split into
    /// individual names; empty when the file has no artist tag.
    pub artists: Vec<String>,
    pub album: Option<String>,
    /// Seconds, parsed from the audio container's own header (FLAC STREAMINFO, MP4 mvhd, MP3 Xing/VBRI or a bitrate
    /// estimate - see `parse_duration_seconds`) rather than an ID3 tag (unreliable - the optional TLEN frame is rarely
    /// set) or a full decode (too expensive to do for every track in a background scan). `None` for a format that
    /// parser doesn't cover (OGG/Opus/WMA - their duration lives at the end of the file) or one it couldn't
    /// confidently parse.
    pub duration_seconds: Option<f64>,
    /// The track record's size/modified time at the time metadata was read: a file edited in place keeps the same
    /// file id, so these are what actually catch a changed file.
    pub size_bytes: u64,
    pub last_modified_ms: i64,
    /// The metadata parser's version at read time - a mismatch means the parser changed since, so the result is stale
    /// even though the file itself didn't.
    pub parser_version: u32,
    /// A real content hash of the file at read time: `hash_bytes` (FNV-1a over the full bytes) for a platform that
    /// reads them directly, or a server-computed sha256 for server-streamed files. The two algorithms differing is
    /// fine: this is only ever compared against a prior value from the same adapter for the same file. `size_bytes`
    /// and `last_modified_ms` are what the freshness check actually gates a re-read on; this exists for the case that
    /// check can't catch (content changed but size and mtime happen not to, e.g. a sync tool preserving timestamps),
    /// which `force_refresh` re-derives.
    pub content_hash: Option<String>,
}
