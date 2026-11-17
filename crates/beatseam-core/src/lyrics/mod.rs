//! Port of `packages/core/src/lyrics`.

mod lrc;

pub use lrc::{LyricLine, ParsedLyrics, Tags, format_lrc, format_timestamp, parse_lrc, shift_lyric_times};
