const AUDIO_FILE_EXTENSIONS: [&str; 8] = [".mp3", ".m4a", ".aac", ".flac", ".wav", ".ogg", ".opus", ".wma"];

/// Best-effort heuristic, not a guarantee of playability. Shared by playlist-from-folder generation (a folder full of
/// loose audio files has no other signal to filter on) and the "locate this missing file" picker (candidates are every
/// audio-looking file in the root, not just ones already referenced by a playlist).
pub fn is_audio_file_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    AUDIO_FILE_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_audio_extensions_case_insensitively() {
        for name in ["a.mp3", "A.MP3", "b.Flac", "c.m4a", "d.opus", "e.wma", "f.aac", "g.wav", "h.ogg", "dir.d/x.mp3"] {
            assert!(is_audio_file_name(name), "{name}");
        }
    }

    #[test]
    fn rejects_everything_else() {
        for name in ["a.txt", "a.m3u8", "a.mp3.bak", "mp3", "", ".mp3x"] {
            assert!(!is_audio_file_name(name), "{name}");
        }
    }

    #[test]
    fn a_bare_extension_counts_as_a_name_like_the_ts_ends_with_check() {
        assert!(is_audio_file_name(".mp3"));
    }
}
