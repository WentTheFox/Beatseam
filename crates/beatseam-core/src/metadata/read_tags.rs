use super::types::CoverArtBytes;
use crate::js_compat::js_trim;

/// What reading an audio file's tags yields (the fields the library screen displays).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParsedTags {
    pub title: Option<String>,
    pub artists: Vec<String>,
    pub album: Option<String>,
    /// Raw, un-resized - the metadata pipeline downstream of this resizes it, applies the size cut-off and stores it.
    pub cover_art: Option<CoverArtBytes>,
}

/// Reads ID3v1/ID3v2/MP4/FLAC tags from an audio file's bytes. The real parsers are platform-independent but are chosen
/// later (the TypeScript uses `jsmediatags`; the Rust port will use a maintained crate rather than hand-rolling a binary
/// tag parser, since the format's edge cases - synchsafe sizes, extended headers, half a dozen text encodings, three tag
/// format generations - are exactly what a library exists to get right). `None` means "no supported tag found (or a
/// malformed one)", which is "no metadata for this file", not a failure.
pub trait TagReader {
    fn read_tags(&self, file_bytes: &[u8]) -> Option<ParsedTags>;
}

impl<F: Fn(&[u8]) -> Option<ParsedTags>> TagReader for F {
    fn read_tags(&self, file_bytes: &[u8]) -> Option<ParsedTags> {
        self(file_bytes)
    }
}

/// Splits a multi-artist tag value into individual names: ID3v2.4 separates multiple TPE1 values with NUL, v2.3
/// conventionally uses "/".
pub fn split_artists(raw: &str) -> Vec<String> {
    raw.split(['\0', '/']).map(js_trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

/// ID3v2.3+/MP4/FLAC give a real MIME type; only ID3v2.2's 3-char image type ("JPG"/"PNG") doesn't.
pub fn normalize_mime_type(format: &str) -> String {
    if format.contains('/') {
        return format.to_owned();
    }
    match format.to_uppercase().as_str() {
        "JPG" | "JPEG" => "image/jpeg".into(),
        "PNG" => "image/png".into(),
        // Most embedded art is jpeg; a wrong guess here just fails to decode rather than crashing anything.
        _ => "image/jpeg".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_artists_on_nul_and_slash_trimming_and_dropping_empties() {
        assert_eq!(split_artists("A\0B"), ["A", "B"]);
        assert_eq!(split_artists("A / B/C"), ["A", "B", "C"]);
        assert_eq!(split_artists(" \0 / "), Vec::<String>::new());
        assert_eq!(split_artists("Solo"), ["Solo"]);
        assert_eq!(split_artists(""), Vec::<String>::new());
    }

    #[test]
    fn normalizes_picture_formats_to_mime_types() {
        assert_eq!(normalize_mime_type("image/png"), "image/png");
        assert_eq!(normalize_mime_type("JPG"), "image/jpeg");
        assert_eq!(normalize_mime_type("jpeg"), "image/jpeg");
        assert_eq!(normalize_mime_type("png"), "image/png");
        assert_eq!(normalize_mime_type("GIF"), "image/jpeg");
        assert_eq!(normalize_mime_type(""), "image/jpeg");
    }

    #[test]
    fn a_closure_is_a_tag_reader() {
        let reader = |bytes: &[u8]| Some(ParsedTags { title: Some(bytes.len().to_string()), ..Default::default() });
        assert_eq!(reader.read_tags(b"abc").unwrap().title.as_deref(), Some("3"));
    }
}
