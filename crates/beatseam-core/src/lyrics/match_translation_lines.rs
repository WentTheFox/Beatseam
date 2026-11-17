use std::collections::HashMap;

use super::lrc::{LyricLine, ParsedLyrics};

/// How far apart two lines' timestamps can be and still be considered "the same line" across a native/translation
/// pair. Native and translation come from independently-sourced lrclib uploads (different transcribers), so their
/// timestamp grids can drift beyond simple rounding - not just clock-skew-style small offsets - hence a full second
/// rather than something tighter.
pub const TRANSLATION_MATCH_TOLERANCE_SECONDS: f64 = 1.0;

/// Attaches each native line's matching translation line (by timestamp, within
/// [`TRANSLATION_MATCH_TOLERANCE_SECONDS`]) as that line's `translation` - native lines are returned unchanged if the
/// translation itself isn't synced, since there's no reliable way to align a plain-text translation to individual
/// timed native lines.
///
/// Matching is a sequential (monotonic) two-pointer walk over both lists in their existing time order, NOT a free
/// "closest match anywhere in the list" search - both lists are the same song's lines in the same chronological
/// order, so once translation line J is used for native line I, no native line after I is ever allowed to match a
/// translation line before J. Two bugs found on-device with hand-built test files motivated this:
///
/// 1. Two native lines close together in time could both independently find the same single translation line as
///    their closest candidate, duplicating one translated line under two different native lines.
/// 2. A plain "globally closest pair wins, greedily" assignment still let a LATER native line's coincidentally
///    closer match steal a translation line out from under the EARLIER native line it actually belonged to.
///
/// Advancing the pointer past whatever was just used means an earlier line's match is claimed before a later line
/// ever gets a chance to see (and steal) it.
pub fn match_translation_lines(native: &ParsedLyrics, translation: &ParsedLyrics) -> ParsedLyrics {
    if !translation.synced {
        return native.clone();
    }

    // translation.lines is already sorted ascending by time (parse_lrc's contract for a synced file) - the pointer
    // walk below depends on that.
    let translation_lines: Vec<(f64, &str)> =
        translation.lines.iter().filter_map(|l| l.time_seconds.map(|t| (t, l.text.as_str()))).collect();

    let mut translation_by_native_index: HashMap<usize, &str> = HashMap::new();
    let mut pointer = 0;
    for (native_index, native_line) in native.lines.iter().enumerate() {
        let Some(native_time) = native_line.time_seconds else { continue };

        // Skip translation lines so far in the past they can't match this (or any later) native line - both lists
        // only move forward.
        while pointer + 1 < translation_lines.len() && translation_lines[pointer].0 < native_time - TRANSLATION_MATCH_TOLERANCE_SECONDS {
            pointer += 1;
        }

        let mut best: Option<(usize, f64)> = None;
        for (i, (candidate_time, _)) in translation_lines.iter().enumerate().skip(pointer) {
            if *candidate_time > native_time + TRANSLATION_MATCH_TOLERANCE_SECONDS {
                break; // sorted ascending - nothing further out can be closer
            }
            let delta = (candidate_time - native_time).abs();
            if best.is_none_or(|(_, best_delta)| delta < best_delta) {
                best = Some((i, delta));
            }
        }

        if let Some((index, _)) = best {
            translation_by_native_index.insert(native_index, translation_lines[index].1);
            pointer = index + 1; // never reconsider this (or any earlier) translation line for a later native line
        }
    }

    ParsedLyrics {
        synced: native.synced,
        tags: native.tags.clone(),
        lines: native
            .lines
            .iter()
            .enumerate()
            .map(|(index, line)| LyricLine {
                translation: match line.time_seconds {
                    None => line.translation.clone(),
                    Some(_) => translation_by_native_index.get(&index).map(|t| t.to_string()),
                },
                ..line.clone()
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::Tags;

    fn lyrics(synced: bool, lines: &[(Option<f64>, &str)]) -> ParsedLyrics {
        ParsedLyrics { synced, tags: Tags::new(), lines: lines.iter().map(|(t, s)| LyricLine::new(*t, *s)).collect() }
    }

    fn translations(p: &ParsedLyrics) -> Vec<Option<&str>> {
        p.lines.iter().map(|l| l.translation.as_deref()).collect()
    }

    #[test]
    fn attaches_the_closest_translation_line_within_tolerance() {
        let native = lyrics(true, &[(Some(10.0), "native one"), (Some(20.0), "native two")]);
        let translation = lyrics(true, &[(Some(10.4), "translated one"), (Some(20.9), "translated two")]);
        let result = match_translation_lines(&native, &translation);
        assert_eq!(translations(&result), [Some("translated one"), Some("translated two")]);
        assert_eq!(result.lines[0].text, "native one");
        assert_eq!(result.lines[1].time_seconds, Some(20.0));
    }

    #[test]
    fn leaves_translation_empty_when_nothing_is_within_tolerance() {
        let native = lyrics(true, &[(Some(10.0), "native")]);
        let translation = lyrics(true, &[(Some(15.0), "too far")]);
        assert_eq!(translations(&match_translation_lines(&native, &translation)), [None]);
    }

    #[test]
    fn picks_the_closest_candidate_when_multiple_are_within_tolerance() {
        let native = lyrics(true, &[(Some(10.0), "native")]);
        let translation = lyrics(true, &[(Some(10.2), "closer"), (Some(10.9), "farther")]);
        assert_eq!(translations(&match_translation_lines(&native, &translation)), [Some("closer")]);
    }

    #[test]
    fn never_assigns_the_same_translation_line_to_two_different_native_lines() {
        // Two native lines 1.3 s apart, both within tolerance of the one translation line between them.
        let native = lyrics(true, &[(Some(14.63), "native A"), (Some(15.96), "native B")]);
        let translation = lyrics(true, &[(Some(15.2), "shared candidate")]);
        let result = match_translation_lines(&native, &translation);
        assert_eq!(translations(&result), [Some("shared candidate"), None]);
    }

    #[test]
    fn never_lets_a_later_native_line_steal_an_earlier_ones_rightful_match() {
        // The one translation line is 0.84 s from A and 1.99 s from B (out of tolerance), so A must keep it and B gets
        // nothing. (The on-device bug had B only 0.09 s away, so a greedy global-closest pass gave it to B instead.)
        let native = lyrics(true, &[(Some(18.66), "native A"), (Some(21.49), "native B")]);
        let translation = lyrics(true, &[(Some(19.5), "A's translation")]);
        assert_eq!(translations(&match_translation_lines(&native, &translation)), [Some("A's translation"), None]);
    }

    #[test]
    fn gives_a_later_native_line_the_next_closest_translation_once_an_earlier_one_claims_the_nearest() {
        let native = lyrics(true, &[(Some(10.0), "native one"), (Some(10.3), "native two")]);
        let translation = lyrics(true, &[(Some(10.05), "closest to one"), (Some(10.9), "next closest")]);
        assert_eq!(translations(&match_translation_lines(&native, &translation)), [Some("closest to one"), Some("next closest")]);
    }

    #[test]
    fn returns_native_unchanged_when_the_translation_is_unsynced() {
        let native = lyrics(true, &[(Some(10.0), "native")]);
        let translation = lyrics(false, &[(None, "plain translation")]);
        assert_eq!(match_translation_lines(&native, &translation), native);
    }

    #[test]
    fn leaves_an_unsynced_native_line_untouched() {
        let native = lyrics(false, &[(None, "native plain")]);
        let translation = lyrics(true, &[(Some(0.0), "translated")]);
        let result = match_translation_lines(&native, &translation);
        assert_eq!(result.lines, native.lines);
    }

    #[test]
    fn empty_translation_and_tolerance_boundaries_do_not_panic() {
        let native = lyrics(true, &[(Some(1.0), "a")]);
        assert_eq!(translations(&match_translation_lines(&native, &lyrics(true, &[]))), [None]);
        // Exactly one tolerance away still counts (the break is `>`, not `>=`).
        let on_the_edge = lyrics(true, &[(Some(2.0), "edge")]);
        assert_eq!(translations(&match_translation_lines(&native, &on_the_edge)), [Some("edge")]);
    }
}
