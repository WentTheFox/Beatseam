//! A [`TagReader`] for `beatseam-core` on top of `lofty`: title, artists, album and the first embedded picture of an audio
//! file's tags (ID3v1/ID3v2, MP4, FLAC/Vorbis comments, APE, ...). Reads from bytes already in memory; no file system.
//!
//! The TypeScript read tags with `jsmediatags` and set `title`, `artist`, `album` and `picture` from it. This keeps those
//! semantics (trimmed text, empty means none, multiple artists split on NUL and `/`, the first picture wins, an ID3v2.2
//! `JPG`/`PNG` image type mapped to a MIME type) and is compared against `jsmediatags` on real files by
//! `tests/jsmediatags_comparison.rs`.

use std::io::Cursor;

use beatseam_core::js_compat::js_trim;
use beatseam_core::metadata::{CoverArtBytes, ParsedTags, TagReader, normalize_mime_type, split_artists};
use lofty::config::{ParseOptions, ParsingMode};
use lofty::file::{FileType, TaggedFileExt};
use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::{ItemKey, Tag, TagType};

/// Reads tags with `lofty`.
#[derive(Clone, Copy, Debug, Default)]
pub struct LoftyTagReader;

fn clean(text: Option<&str>) -> Option<String> {
    text.map(js_trim).filter(|t| !t.is_empty()).map(str::to_owned)
}

fn artists(tag: &Tag) -> Vec<String> {
    // jsmediatags returns only the first value of the artist item: an ID3v2.4 frame holding several NUL-separated artists
    // (a whole credit list, on some files) reads as just its first name. Matched on purpose, so the same file shows the same
    // artist in the TypeScript and Rust apps; lofty would hand back every value. Within that value a "/" or NUL still
    // separates names, as in readTags.ts. (Showing every credited artist would be a deliberate follow-up behaviour change.)
    // For Vorbis comments (FLAC, Ogg) a repeated ARTIST field overwrites the earlier one in jsmediatags, so the last wins.
    let mut values = tag.get_strings(ItemKey::TrackArtist);
    let chosen = if tag.tag_type() == TagType::VorbisComments { values.last() } else { values.next() };
    chosen.map(split_artists).unwrap_or_default()
}

fn cover_art(tag: &Tag) -> Option<CoverArtBytes> {
    let picture = tag.pictures().first()?;
    if picture.data().is_empty() {
        return None;
    }
    let format = picture.mime_type().map(|m| m.as_str().to_owned()).unwrap_or_default();
    Some(CoverArtBytes { mime_type: normalize_mime_type(&format), data: picture.data().to_vec() })
}

impl TagReader for LoftyTagReader {
    fn read_tags(&self, file_bytes: &[u8]) -> Option<ParsedTags> {
        // Properties (duration, bitrate) are not needed - the duration comes from the container parser in beatseam-core - and a
        // relaxed parse still returns the tag of a file whose audio frames are damaged or missing.
        let options = ParseOptions::new().read_properties(false).parsing_mode(ParsingMode::Relaxed);
        let mut probe = Probe::new(Cursor::new(file_bytes)).options(options).guess_file_type().ok()?;
        // jsmediatags reads an ID3v2 tag from its header alone. Lofty guesses a type from the first audio frame, which a
        // truncated prefix (or a damaged file) may not have, so trust the header.
        if probe.file_type().is_none() && file_bytes.starts_with(b"ID3") {
            probe = probe.set_file_type(FileType::Mpeg);
        }
        let tagged = probe.read().ok()?;
        // No supported tag found (or a malformed one) is "no metadata for this file", not a failure.
        let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
        Some(ParsedTags {
            title: clean(tag.title().as_deref()),
            artists: artists(tag),
            album: clean(tag.album().as_deref()),
            cover_art: cover_art(tag),
        })
    }
}

#[cfg(test)]
mod tests {
    use beatseam_core::metadata::TagReader;

    use super::*;

    fn synchsafe(size: usize) -> [u8; 4] {
        [(size >> 21) as u8 & 0x7f, (size >> 14) as u8 & 0x7f, (size >> 7) as u8 & 0x7f, size as u8 & 0x7f]
    }

    /// A minimal real ID3v2.3 tag (Latin-1 text frames, plus an optional APIC picture frame) followed by filler "audio".
    fn mp3_with_id3v2(title: Option<&str>, artist: Option<&str>, album: Option<&str>, cover: Option<(&str, &[u8])>) -> Vec<u8> {
        let mut frames = Vec::new();
        let mut add_frame = |id: &str, body: Vec<u8>| {
            frames.extend(id.bytes());
            frames.extend((body.len() as u32).to_be_bytes());
            frames.extend([0, 0]);
            frames.extend(body);
        };
        let text = |s: &str| [vec![0u8], s.bytes().collect()].concat(); // encoding byte 0 = Latin-1
        if let Some(t) = title {
            add_frame("TIT2", text(t));
        }
        if let Some(a) = artist {
            add_frame("TPE1", text(a));
        }
        if let Some(a) = album {
            add_frame("TALB", text(a));
        }
        if let Some((mime, bytes)) = cover {
            // encoding, null-terminated mime, picture type 3 ("Cover (front)"), empty null-terminated description, data
            add_frame("APIC", [vec![0], mime.bytes().collect(), vec![0, 3, 0], bytes.to_vec()].concat());
        }
        let mut out = vec![0x49, 0x44, 0x33, 3, 0, 0];
        out.extend(synchsafe(frames.len()));
        out.extend(frames);
        out.extend(vec![0u8; 64]);
        out
    }

    #[test]
    fn reads_title_artist_and_album_from_a_real_id3v2_3_tag() {
        let tags = LoftyTagReader.read_tags(&mp3_with_id3v2(Some("Song Title"), Some("The Artist"), Some("Great Album"), None)).expect("tags");
        assert_eq!((tags.title.as_deref(), tags.artists.as_slice(), tags.album.as_deref()), (Some("Song Title"), ["The Artist".to_string()].as_slice(), Some("Great Album")));
        assert_eq!(tags.cover_art, None);
    }

    #[test]
    fn extracts_embedded_cover_art_with_its_mime_type() {
        let image = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x01, 0x02, 0x03];
        let tags = LoftyTagReader.read_tags(&mp3_with_id3v2(Some("Song"), None, None, Some(("image/jpeg", &image)))).expect("tags");
        assert_eq!(tags.cover_art, Some(CoverArtBytes { mime_type: "image/jpeg".into(), data: image.to_vec() }));
    }

    #[test]
    fn splits_a_slash_delimited_multi_artist_frame_into_individual_names() {
        let tags = LoftyTagReader.read_tags(&mp3_with_id3v2(Some("Song"), Some("Artist One/Artist Two"), None, None)).expect("tags");
        assert_eq!(tags.artists, ["Artist One", "Artist Two"]);
    }

    #[test]
    fn a_file_with_no_tag_or_not_audio_at_all_yields_none() {
        assert_eq!(LoftyTagReader.read_tags(&[0u8; 64]), None);
        assert_eq!(LoftyTagReader.read_tags(b"just some text"), None);
        assert_eq!(LoftyTagReader.read_tags(&[]), None);
    }

    #[test]
    fn blank_text_counts_as_no_value() {
        let tags = LoftyTagReader.read_tags(&mp3_with_id3v2(Some("   "), Some(" / "), Some(""), None));
        if let Some(tags) = tags {
            assert_eq!((tags.title, tags.artists, tags.album), (None, vec![], None));
        }
    }

    #[test]
    fn works_as_the_tag_reader_of_a_metadata_service() {
        use std::sync::Arc;

        use beatseam_core::file_access::{FileRef, MemoryFileAccess};
        use beatseam_core::http::block_on;
        use beatseam_core::library_store::MemoryLibraryStore;
        use beatseam_core::metadata::MetadataService;

        // MemoryFileAccess serves text, so use ASCII-only bytes for the tag (the ID3 header and frames are all < 0x80
        // here) via a Latin-1 string.
        let bytes = mp3_with_id3v2(Some("Song Title"), Some("The Artist"), Some("Great Album"), None);
        assert!(bytes.iter().all(|b| *b < 0x80));
        let text = String::from_utf8(bytes).unwrap();
        let service = MetadataService::new(Arc::new(MemoryFileAccess::new([("a.mp3", text.as_str())])), Arc::new(MemoryLibraryStore::new()), Arc::new(LoftyTagReader));
        let file = FileRef { id: "a".into(), name: "a.mp3".into(), relative_path: "a.mp3".into(), size_bytes: text.len() as u64, last_modified_ms: 5.0 };
        let metadata = block_on(service.ensure_track_metadata(&file, false)).unwrap();
        assert_eq!((metadata.title.as_deref(), metadata.artists.as_slice(), metadata.album.as_deref()), (Some("Song Title"), ["The Artist".to_string()].as_slice(), Some("Great Album")));
    }
}
