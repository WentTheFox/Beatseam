use super::types::TrackMetadata;
use crate::library_store::{TrackRecord, track_display_name};

/// "Title • Artist" when metadata has a title, falling back to the bare filename until it's scanned or when the file has
/// no tags.
pub fn format_track_title(metadata: Option<&TrackMetadata>, track: &TrackRecord) -> String {
    let Some(title) = metadata.and_then(|m| m.title.as_deref()).filter(|t| !t.is_empty()) else { return track_display_name(track).to_owned() };
    let artists = metadata.map(|m| m.artists.join(", ")).unwrap_or_default();
    if artists.is_empty() { title.to_owned() } else { format!("{title} • {artists}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> TrackRecord {
        TrackRecord { file_id: "f".into(), root_id: "r".into(), relative_path: "Dir/file name.mp3".into(), size_bytes: 1, last_modified_ms: 1.0, missing: false }
    }

    fn meta(title: Option<&str>, artists: &[&str]) -> TrackMetadata {
        TrackMetadata { file_id: "f".into(), title: title.map(str::to_owned), artists: artists.iter().map(|a| a.to_string()).collect(), album: None, duration_seconds: None, size_bytes: 1, last_modified_ms: 1.0, parser_version: 1, content_hash: None }
    }

    #[test]
    fn titles_and_artists_or_the_file_name_as_a_fallback() {
        assert_eq!(format_track_title(Some(&meta(Some("Song"), &["A", "B"])), &track()), "Song • A, B");
        assert_eq!(format_track_title(Some(&meta(Some("Song"), &[])), &track()), "Song");
        assert_eq!(format_track_title(Some(&meta(None, &["A"])), &track()), "file name.mp3");
        assert_eq!(format_track_title(Some(&meta(Some(""), &["A"])), &track()), "file name.mp3");
        assert_eq!(format_track_title(None, &track()), "file name.mp3");
    }
}
