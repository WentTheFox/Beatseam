/// Filename without its extension, case-folded - the basis for auto-matching a track to a same-named .lrc file.
///
/// The extension is the part after the last `.`, if it is non-empty and has no path separator in it (the JS regex
/// `\.[^./\\]+$`), so `".hidden"` is all extension and `"a.b/c"` has none.
pub fn lyrics_stem(filename: &str) -> String {
    let without_extension = match filename.rfind('.') {
        Some(dot) if filename.len() > dot + 1 && !filename[dot + 1..].contains(['/', '\\']) => &filename[..dot],
        _ => filename,
    };
    crate::js_compat::js_trim(without_extension).to_lowercase()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LyricsCandidate {
    pub file_id: String,
    pub name: String,
}

/// Auto-assigns a track to a same-stemmed .lrc file (e.g. "Track.mp3" <-> "Track.lrc"), the way most desktop players
/// match sidecar lyrics. One .lrc file may legitimately match multiple tracks (different quality/format files for the
/// same song), so this only needs to be unambiguous from the *track's* side: if more than one .lrc candidate shares
/// the track's stem, there's no principled way to prefer one over another automatically, so it returns `None` and
/// leaves the choice to a manual override instead of guessing.
pub fn find_auto_lyrics_match<'a>(track_filename: &str, candidates: &'a [LyricsCandidate]) -> Option<&'a LyricsCandidate> {
    let stem = lyrics_stem(track_filename);
    let mut matches = candidates.iter().filter(|c| lyrics_stem(&c.name) == stem);
    match (matches.next(), matches.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: &str, name: &str) -> LyricsCandidate {
        LyricsCandidate { file_id: id.into(), name: name.into() }
    }

    #[test]
    fn stem_strips_the_extension_and_case_folds() {
        assert_eq!(lyrics_stem("Track One.mp3"), "track one");
        assert_eq!(lyrics_stem("Track One.LRC"), "track one");
    }

    #[test]
    fn stem_leaves_extension_less_names_alone() {
        assert_eq!(lyrics_stem("Track One"), "track one");
    }

    // JS regex edge cases: `\.[^./\\]+$`
    #[test]
    fn stem_edge_cases_match_the_js_regex() {
        assert_eq!(lyrics_stem(".hidden"), ""); // the whole name is an extension
        assert_eq!(lyrics_stem("a.b.c"), "a.b"); // only the last extension goes
        assert_eq!(lyrics_stem("trailing."), "trailing."); // an empty extension is not one
        assert_eq!(lyrics_stem("dir.d/file"), "dir.d/file"); // the dot is in a directory part
        assert_eq!(lyrics_stem("  Padded .mp3 "), "padded"); // the extension is "mp3 " (trailing space included), then trimmed
        assert_eq!(lyrics_stem("Stra\u{df}e.flac"), "stra\u{df}e");
        assert_eq!(lyrics_stem("\u{130}.mp3"), "i\u{307}"); // JS: "İ".toLowerCase() is "i̇"
    }

    #[test]
    fn matches_a_track_to_its_same_stemmed_lrc_file() {
        let candidates = [cand("a", "Track One.lrc"), cand("b", "Track Two.lrc")];
        assert_eq!(find_auto_lyrics_match("Track One.flac", &candidates), Some(&candidates[0]));
    }

    #[test]
    fn is_case_insensitive() {
        let candidates = [cand("a", "track one.lrc")];
        assert_eq!(find_auto_lyrics_match("TRACK ONE.mp3", &candidates), Some(&candidates[0]));
    }

    #[test]
    fn returns_none_when_nothing_matches() {
        assert_eq!(find_auto_lyrics_match("Track One.mp3", &[cand("a", "Other Song.lrc")]), None);
    }

    #[test]
    fn returns_none_rather_than_guessing_when_multiple_lrc_files_share_the_same_stem() {
        let candidates = [cand("a", "Track One.lrc"), cand("b", "Track One.lrc")];
        assert_eq!(find_auto_lyrics_match("Track One.mp3", &candidates), None);
    }

    #[test]
    fn lets_one_lrc_file_match_multiple_different_tracks() {
        let candidates = [cand("a", "Track One.lrc")];
        assert_eq!(find_auto_lyrics_match("Track One.mp3", &candidates), Some(&candidates[0]));
        assert_eq!(find_auto_lyrics_match("Track One.flac", &candidates), Some(&candidates[0]));
    }
}
