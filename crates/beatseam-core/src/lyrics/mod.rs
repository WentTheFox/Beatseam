//! Port of `packages/core/src/lyrics` (the pure parts; the file-access-driven loaders come with the `FileAccess` trait).

mod lrc;
mod match_lyrics;
mod match_translation_lines;

pub use lrc::{LyricLine, ParsedLyrics, Tags, format_lrc, format_timestamp, parse_lrc, shift_lyric_times};
pub use match_lyrics::{LyricsCandidate, find_auto_lyrics_match, lyrics_stem};
pub use match_translation_lines::{TRANSLATION_MATCH_TOLERANCE_SECONDS, match_translation_lines};
