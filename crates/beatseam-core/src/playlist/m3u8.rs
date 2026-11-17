#[derive(Clone, Debug, Default, PartialEq)]
pub struct M3u8Entry {
    /// Path exactly as written in the playlist file, not yet resolved against the library root.
    pub raw_path: String,
    pub duration_seconds: Option<f64>,
    pub title: Option<String>,
}

impl M3u8Entry {
    pub fn path(raw_path: impl Into<String>) -> Self {
        Self { raw_path: raw_path.into(), ..Self::default() }
    }
}

// A playlist saved with a byte order mark must still have its `#EXTM3U` header recognised: `js_trim` strips the BOM,
// which Rust's own `trim` keeps.
use crate::js_compat::{js_round, js_trim};

/// The part of JavaScript's `Number(text)` the playlist format can hit: an empty or blank string is `0`, anything
/// that isn't a finite decimal number is `None`. (JS also accepts `0x`/`0b`/`0o` prefixes; not worth supporting in a
/// duration field.)
fn js_number(text: &str) -> Option<f64> {
    let t = js_trim(text);
    if t.is_empty() {
        return Some(0.0);
    }
    t.parse::<f64>().ok().filter(|n| n.is_finite())
}

/// Inverse of [`parse_m3u8`] - writes an `#EXTINF` directive ahead of an entry only when it actually carries
/// duration/title info, same as a plain (non-extended) m3u8 would look for entries that don't. Always writes the
/// `#EXTM3U` header, so a reader that only recognizes plain path-per-line playlists still works (it just ignores the
/// `#` lines) while one that understands `#EXTINF` gets the richer version.
pub fn format_m3u8(entries: &[M3u8Entry]) -> String {
    let mut out = String::from("#EXTM3U\n");
    for entry in entries {
        if entry.duration_seconds.is_some() || entry.title.is_some() {
            let duration = js_round(entry.duration_seconds.unwrap_or(-1.0));
            out.push_str(&format!("#EXTINF:{},{}\n", duration as i64, entry.title.as_deref().unwrap_or("")));
        }
        out.push_str(&entry.raw_path);
        out.push('\n');
    }
    out
}

/// Parses extended-M3U syntax (`#EXTM3U` / `#EXTINF` directives). Unknown `#` directives are ignored rather than
/// rejected, since players in the wild write a variety of vendor-specific tags we don't need to understand.
pub fn parse_m3u8(text: &str) -> Vec<M3u8Entry> {
    let mut entries = Vec::new();
    let mut pending_duration: Option<f64> = None;
    let mut pending_title: Option<String> = None;

    // JS splits on /\r\n|\r|\n/.
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    for raw_line in normalized.split('\n') {
        let line = js_trim(raw_line);
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let (duration_part, title) = match rest.split_once(',') {
                Some((d, t)) => (d, Some(js_trim(t)).filter(|t| !t.is_empty()).map(str::to_owned)),
                None => (rest, None),
            };
            pending_duration = js_number(duration_part);
            pending_title = title;
            continue;
        }

        if line.starts_with('#') {
            continue;
        }

        entries.push(M3u8Entry {
            raw_path: line.to_owned(),
            duration_seconds: pending_duration.take(),
            title: pending_title.take(),
        });
    }

    entries
}

/// `^[a-zA-Z]:/`
fn has_drive_prefix(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/'
}

/// Resolves an m3u8 entry's path against the playlist's own location, since playlists reference tracks relative to
/// themselves, not the library root.
///
/// Entries that look absolute (leading slash, or a Windows drive letter - playlists get authored on whatever OS was
/// handy) can't be resolved against an arbitrary user-granted root, so we fall back to treating the remainder as
/// root-relative rather than failing the entry outright.
pub fn resolve_m3u8_entry_path(playlist_relative_path: &str, raw_path: &str) -> String {
    let entry = raw_path.replace('\\', "/");
    let is_absolute = entry.starts_with('/') || has_drive_prefix(&entry);

    let playlist = playlist_relative_path.replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    if is_absolute {
        let without_drive = if has_drive_prefix(&entry) { &entry[2..] } else { entry.as_str() };
        parts.extend(without_drive.split('/'));
    } else {
        let mut dir: Vec<&str> = playlist.split('/').collect();
        dir.pop(); // the playlist file's own name
        parts.extend(dir);
        parts.extend(entry.split('/'));
    }

    let mut resolved: Vec<&str> = Vec::new();
    for part in parts {
        match part {
            "" | "." => {}
            ".." => {
                resolved.pop();
            }
            _ => resolved.push(part),
        }
    }
    resolved.join("/")
}

/// Inverse of [`resolve_m3u8_entry_path`] - expresses a root-relative target path as a path relative to the
/// playlist's own location, for writing a corrected entry back into the m3u8. Always a plain forward-slashed relative
/// path, never absolute - a rewritten entry doesn't need to preserve whatever absolute-path quirk the original
/// happened to be written with, now that the target's real root-relative location is known.
pub fn relativize_m3u8_entry_path(playlist_relative_path: &str, target_relative_path: &str) -> String {
    let mut playlist_dir: Vec<&str> = playlist_relative_path.split('/').collect();
    playlist_dir.pop();
    let target: Vec<&str> = target_relative_path.split('/').collect();

    let mut common = 0;
    while common < playlist_dir.len() && common + 1 < target.len() && playlist_dir[common] == target[common] {
        common += 1;
    }

    let ups = playlist_dir.len() - common;
    let downs = target[common..].join("/");
    // No ../ hops needed: prefix with ./ rather than leaving a bare path, matching the convention real-world .m3u8
    // files are written with, so a rewritten entry doesn't stand out from its neighbours.
    if ups == 0 {
        return format!("./{downs}");
    }
    format!("{}{downs}", "../".repeat(ups))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, duration: Option<f64>, title: Option<&str>) -> M3u8Entry {
        M3u8Entry { raw_path: path.into(), duration_seconds: duration, title: title.map(Into::into) }
    }

    // ---- parseM3u8 ----

    #[test]
    fn parses_extinf_duration_and_title_alongside_the_path() {
        let text = ["#EXTM3U", "#EXTINF:215,Artist - Track One", "Track One.mp3", "#EXTINF:-1,Artist - Track Two", "subdir/Track Two.mp3"].join("\n");
        assert_eq!(
            parse_m3u8(&text),
            vec![
                entry("Track One.mp3", Some(215.0), Some("Artist - Track One")),
                entry("subdir/Track Two.mp3", Some(-1.0), Some("Artist - Track Two")),
            ]
        );
    }

    #[test]
    fn accepts_bare_paths_with_no_extinf_and_ignores_unknown_directives() {
        let text = ["#EXTM3U", "#EXT-X-SOMETHING-VENDOR-SPECIFIC", "plain.mp3", "", "  "].join("\n");
        assert_eq!(parse_m3u8(&text), vec![entry("plain.mp3", None, None)]);
    }

    #[test]
    fn handles_crlf_line_endings() {
        assert_eq!(parse_m3u8("#EXTM3U\r\ntrack.mp3\r\n"), vec![entry("track.mp3", None, None)]);
    }

    // Behaviours of the JS implementation that the TS tests don't pin but real files hit.

    #[test]
    fn handles_lone_cr_line_endings() {
        assert_eq!(parse_m3u8("#EXTM3U\ra.mp3\rb.mp3"), vec![entry("a.mp3", None, None), entry("b.mp3", None, None)]);
    }

    #[test]
    fn strips_a_byte_order_mark_like_js_trim_does() {
        assert_eq!(parse_m3u8("\u{feff}#EXTM3U\n#EXTINF:5,T\nx.mp3"), vec![entry("x.mp3", Some(5.0), Some("T"))]);
    }

    #[test]
    fn extinf_edge_cases_match_js_number() {
        // Number("") is 0, not NaN, so a missing duration before the comma is 0; text that isn't a number is dropped.
        assert_eq!(parse_m3u8("#EXTINF:,Only Title\na.mp3"), vec![entry("a.mp3", Some(0.0), Some("Only Title"))]);
        assert_eq!(parse_m3u8("#EXTINF:abc,T\na.mp3"), vec![entry("a.mp3", None, Some("T"))]);
        assert_eq!(parse_m3u8("#EXTINF:215\na.mp3"), vec![entry("a.mp3", Some(215.0), None)]);
        assert_eq!(parse_m3u8("#EXTINF: 12.5 , Spaced \na.mp3"), vec![entry("a.mp3", Some(12.5), Some("Spaced"))]);
        assert_eq!(parse_m3u8("#EXTINF:Infinity,T\na.mp3"), vec![entry("a.mp3", None, Some("T"))]);
    }

    #[test]
    fn extinf_applies_to_one_entry_only_and_a_title_keeps_later_commas() {
        let parsed = parse_m3u8("#EXTINF:1,A, B\na.mp3\nb.mp3");
        assert_eq!(parsed, vec![entry("a.mp3", Some(1.0), Some("A, B")), entry("b.mp3", None, None)]);
    }

    // ---- resolveM3u8EntryPath ----

    #[test]
    fn resolves_a_relative_entry_against_the_playlist_directory() {
        assert_eq!(resolve_m3u8_entry_path("Playlists/Mix.m3u8", "Track.mp3"), "Playlists/Track.mp3");
    }

    #[test]
    fn resolves_entries_that_reference_sibling_directories_via_dotdot() {
        assert_eq!(resolve_m3u8_entry_path("Playlists/Mix.m3u8", "../Artist/Album/Track.mp3"), "Artist/Album/Track.mp3");
    }

    #[test]
    fn normalizes_backslashes_from_windows_authored_playlists() {
        assert_eq!(resolve_m3u8_entry_path("Playlists\\Mix.m3u8", "Artist\\Track.mp3"), "Playlists/Artist/Track.mp3");
    }

    #[test]
    fn falls_back_to_root_relative_for_absolute_looking_entries() {
        assert_eq!(resolve_m3u8_entry_path("Playlists/Mix.m3u8", "/Music/Artist/Track.mp3"), "Music/Artist/Track.mp3");
        assert_eq!(resolve_m3u8_entry_path("Playlists/Mix.m3u8", "C:/Music/Artist/Track.mp3"), "Music/Artist/Track.mp3");
        assert_eq!(resolve_m3u8_entry_path("Playlists/Mix.m3u8", "d:\\Music\\Track.mp3"), "Music/Track.mp3");
    }

    #[test]
    fn resolves_a_playlist_at_the_root_with_no_directory_prefix() {
        assert_eq!(resolve_m3u8_entry_path("Mix.m3u8", "Track.mp3"), "Track.mp3");
    }

    #[test]
    fn dotdot_past_the_root_stops_at_the_root() {
        assert_eq!(resolve_m3u8_entry_path("Mix.m3u8", "../../Track.mp3"), "Track.mp3");
    }

    // ---- relativizeM3u8EntryPath ----

    #[test]
    fn is_the_inverse_of_resolve_for_a_sibling_directory_move() {
        let target = "Artist/Album/Track.mp3";
        assert_eq!(relativize_m3u8_entry_path("Playlists/Mix.m3u8", target), "../Artist/Album/Track.mp3");
        assert_eq!(resolve_m3u8_entry_path("Playlists/Mix.m3u8", &relativize_m3u8_entry_path("Playlists/Mix.m3u8", target)), target);
    }

    #[test]
    fn stays_in_the_same_directory_as_the_playlist_prefixed_with_dot_slash() {
        assert_eq!(relativize_m3u8_entry_path("Mix.m3u8", "Track.mp3"), "./Track.mp3");
    }

    #[test]
    fn descends_into_a_subfolder_relative_to_the_playlist_prefixed_with_dot_slash() {
        assert_eq!(relativize_m3u8_entry_path("Mix.m3u8", "Artist/Track.mp3"), "./Artist/Track.mp3");
    }

    #[test]
    fn round_trips_the_dot_slash_prefix_back_through_resolve() {
        let target = "Artist/Track.mp3";
        assert_eq!(resolve_m3u8_entry_path("Mix.m3u8", &relativize_m3u8_entry_path("Mix.m3u8", target)), target);
    }

    #[test]
    fn shares_a_common_prefix_without_unnecessary_dotdot_hops() {
        assert_eq!(relativize_m3u8_entry_path("Music/Playlists/Mix.m3u8", "Music/Artist/Track.mp3"), "../Artist/Track.mp3");
    }

    // ---- formatM3u8 ----

    #[test]
    fn writes_a_plain_path_per_line_entry_with_no_extinf_when_neither_field_is_given() {
        assert_eq!(format_m3u8(&[M3u8Entry::path("Track.mp3")]), "#EXTM3U\nTrack.mp3\n");
    }

    #[test]
    fn writes_an_extinf_directive_when_duration_or_title_are_given() {
        assert_eq!(
            format_m3u8(&[entry("Track One.mp3", Some(215.0), Some("Artist - Track One"))]),
            "#EXTM3U\n#EXTINF:215,Artist - Track One\nTrack One.mp3\n"
        );
    }

    #[test]
    fn roundtrips_through_parse() {
        let entries = vec![entry("Track One.mp3", Some(215.0), Some("Artist - Track One")), M3u8Entry::path("Track Two.mp3")];
        assert_eq!(parse_m3u8(&format_m3u8(&entries)), entries);
    }

    #[test]
    fn title_only_writes_minus_one_and_duration_only_writes_an_empty_title() {
        assert_eq!(format_m3u8(&[entry("a.mp3", None, Some("T"))]), "#EXTM3U\n#EXTINF:-1,T\na.mp3\n");
        assert_eq!(format_m3u8(&[entry("a.mp3", Some(7.0), None)]), "#EXTM3U\n#EXTINF:7,\na.mp3\n");
    }

    #[test]
    fn durations_round_like_math_round() {
        assert_eq!(format_m3u8(&[entry("a", Some(2.5), None)]), "#EXTM3U\n#EXTINF:3,\na\n");
        assert_eq!(format_m3u8(&[entry("a", Some(-1.5), None)]), "#EXTM3U\n#EXTINF:-1,\na\n"); // Rust's round() gives -2
        assert_eq!(format_m3u8(&[entry("a", Some(214.4), None)]), "#EXTM3U\n#EXTINF:214,\na\n");
    }
}
