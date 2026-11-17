//! Port of `packages/core/src/metadata` (the pure parts; tag reading, scanning and `ensureMetadata` come with the
//! `FileAccess`/`LibraryStore` traits).

mod base64;
mod content_hash;
mod ensure_metadata;
mod format_duration;
mod parse_duration;
mod read_embedded_lyrics;
mod read_tags;
mod scan_library_metadata;
mod types;

pub use base64::encode_base64;
pub use content_hash::hash_bytes;
pub use ensure_metadata::{COVER_ART_MAX_DIMENSION_PX, CoverArtResizer, METADATA_PARSER_VERSION, MetadataError, MetadataService, NoHttp, NoResizer, is_metadata_current, is_metadata_fresh};
pub use format_duration::format_duration;
pub use parse_duration::{DURATION_PREFIX_BYTES, MAX_DURATION_PREFIX_BYTES, duration_prefix_bytes_needed, parse_duration_seconds};
pub use read_embedded_lyrics::{id3v2_tag_length, read_embedded_lyrics};
pub use read_tags::{ParsedTags, TagReader, normalize_mime_type, split_artists};
pub use scan_library_metadata::{IdleDeadline, IdleScheduler, ImmediateIdle, MetadataTask, ScanLibraryMetadataOptions, ScanMetadataProgress, YieldIdle, scan_library_metadata, scan_library_metadata_queued, track_to_file_ref};
pub use types::{CoverArtBytes, TrackMetadata};
