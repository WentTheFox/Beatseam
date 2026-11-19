//! Port of `packages/core/src/lyrics`: LRC parsing and formatting, matching, and the loaders that find and assign `.lrc` files.

mod lrc;
mod lyrics_files;
mod match_library_lyrics;
mod match_lyrics;
mod match_translation_lines;

pub use lrc::{LyricLine, ParsedLyrics, Tags, format_lrc, format_timestamp, parse_lrc, shift_lyric_times};
pub use lyrics_files::{LyricsFileRef, LyricsLoader, decode_lyrics_file_index, encode_lyrics_file_index, lyrics_file_index_setting_key, lyrics_scopes_cache_key, scan_lyrics_root};
pub use match_library_lyrics::{LYRICS_MATCHED_COUNT_SETTING_KEY, MatchLibraryLyricsOptions, match_library_lyrics, match_library_lyrics_queued};
pub use match_lyrics::{LyricsCandidate, find_auto_lyrics_match, lyrics_stem};
pub use match_translation_lines::{TRANSLATION_MATCH_TOLERANCE_SECONDS, match_translation_lines};
