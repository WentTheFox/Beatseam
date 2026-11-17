#[derive(Clone, Debug, PartialEq)]
pub struct LyricLine {
    /// Seconds from track start, or `None` for an unsynced (plain-text) line.
    pub time_seconds: Option<f64>,
    pub text: String,
    /// The matching line from a sibling translation .lrc file, if one is assigned and lines up within tolerance (see
    /// `matchTranslationLines`). Always `None` on a plain `parse_lrc` result; only the merge step fills it in.
    ///
    /// The TypeScript type is `string | null | undefined` and consumers treat null and undefined alike ("no
    /// translation to show"), so both collapse to `None` here. If the wire format ever has to tell them apart, that is
    /// a `beatseam-api` concern, not this crate's.
    pub translation: Option<String>,
}

impl LyricLine {
    pub fn new(time_seconds: Option<f64>, text: impl Into<String>) -> Self {
        Self { time_seconds, text: text.into(), translation: None }
    }
}

/// Metadata tags (`[ti:]`, `[ar:]`, `[al:]`, `[by:]`, `[offset:]`, ...), keys lowercase, in first-seen order, last
/// value wins if a key repeats. Unknown tags are kept too - callers decide what to use. (A JS object, which the
/// TypeScript uses, behaves the same way for these keys.)
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tags(Vec<(String, String)>);

impl Tags {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let (key, value) = (key.into(), value.into());
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => self.0.push((key, value)),
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<const N: usize> From<[(&str, &str); N]> for Tags {
    fn from(pairs: [(&str, &str); N]) -> Self {
        let mut tags = Tags::new();
        for (k, v) in pairs {
            tags.set(k, v);
        }
        tags
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParsedLyrics {
    /// True when at least one `[mm:ss.xx]` timestamp was found - false means `lines` is just the file's text, one line
    /// each, with no timing.
    pub synced: bool,
    pub tags: Tags,
    /// Sorted by time when `synced`; file order otherwise.
    pub lines: Vec<LyricLine>,
}

/// JavaScript's `String.prototype.trim` (Unicode whitespace plus the byte order mark).
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// JavaScript's `Math.round`: halves round up, unlike `f64::round`.
fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// JavaScript's `Number.prototype.toFixed(2)` for a non-negative finite number. It rounds an exact tie up (`1.125` is
/// `"1.13"`), where Rust's `{:.2}` rounds it to even (`"1.12"`); non-ties are the same in both.
fn to_fixed_2(x: f64) -> String {
    let eighths = x * 8.0;
    let is_exact_tie = eighths.fract() == 0.0 && (eighths as i64) % 2 != 0;
    if is_exact_tie {
        let n = (x * 100.0 + 0.5).floor() as i64;
        return format!("{}.{:02}", n / 100, n % 100);
    }
    format!("{x:.2}")
}

/// `83.4` -> `"01:23.40"` - the `[mm:ss.xx]` timestamp format `parse_lrc` reads back.
pub fn format_timestamp(total_seconds: f64) -> String {
    let minutes = (total_seconds / 60.0).floor();
    let seconds = total_seconds - minutes * 60.0;
    format!("{:0>2}:{:0>5}", (minutes as i64).to_string(), to_fixed_2(seconds))
}

/// One `[mm:ss.xx]` / `[mm:ss]` tag (`\[(\d{1,3}):(\d{2}(?:\.\d{1,3})?)\]`, ASCII digits only) found in a line.
struct TimestampTag {
    /// Byte offset just past the `]`.
    end: usize,
    seconds: f64,
}

/// Tries to read a timestamp tag starting at byte `at`, which must be a `[`.
fn timestamp_at(line: &str, at: usize) -> Option<TimestampTag> {
    let b = line.as_bytes();
    let digits_from = |i: usize| b[i..].iter().take_while(|c| c.is_ascii_digit()).count();

    let m_start = at + 1;
    let m_len = digits_from(m_start);
    if !(1..=3).contains(&m_len) || b.get(m_start + m_len) != Some(&b':') {
        return None;
    }
    let s_start = m_start + m_len + 1;
    if digits_from(s_start) < 2 {
        return None;
    }
    let mut s_end = s_start + 2;
    if b.get(s_end) == Some(&b'.') {
        let frac = digits_from(s_end + 1);
        if !(1..=3).contains(&frac) {
            return None;
        }
        s_end += 1 + frac;
    }
    if b.get(s_end) != Some(&b']') {
        return None;
    }
    // digits only, so these parse
    let minutes: f64 = line[m_start..m_start + m_len].parse().ok()?;
    let seconds: f64 = line[s_start..s_end].parse().ok()?;
    Some(TimestampTag { end: s_end + 1, seconds: minutes * 60.0 + seconds })
}

/// `matchAll(TIMESTAMP_TAG)`: non-overlapping, left to right.
fn timestamp_tags(line: &str) -> Vec<TimestampTag> {
    let mut found = Vec::new();
    let mut i = 0;
    while let Some(off) = line[i..].find('[') {
        let at = i + off;
        match timestamp_at(line, at) {
            Some(tag) => {
                i = tag.end;
                found.push(tag);
            }
            None => i = at + 1,
        }
    }
    found
}

/// `^\[([a-zA-Z]+):(.*)\]$` on an already-trimmed line: (key, value). `.` does not match line terminators.
fn metadata_tag(line: &str) -> Option<(&str, &str)> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    let key_len = inner.bytes().take_while(u8::is_ascii_alphabetic).count();
    if key_len == 0 || inner.as_bytes().get(key_len) != Some(&b':') {
        return None;
    }
    let value = &inner[key_len + 1..];
    if value.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
        return None;
    }
    Some((&inner[..key_len], value))
}

/// Parses standard LRC lyrics (`[mm:ss.xx]text`, one or more timestamps per line for repeated lines/choruses). Falls
/// back to treating the whole file as unsynced plain-text lyrics (one line per non-empty input line) when no
/// timestamp tag is found anywhere, so a plaintext lyrics file still displays instead of being rejected outright.
pub fn parse_lrc(content: &str) -> ParsedLyrics {
    let mut tags = Tags::new();
    let mut lines: Vec<LyricLine> = Vec::new();
    let mut saw_timestamp = false;

    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    for raw_line in normalized.split('\n') {
        let line = js_trim(raw_line);
        if line.is_empty() {
            continue;
        }

        let timestamps = timestamp_tags(line);
        if timestamps.is_empty() {
            match metadata_tag(line) {
                Some((key, value)) => tags.set(key.to_ascii_lowercase(), js_trim(value)),
                None => lines.push(LyricLine::new(None, line)),
            }
            continue;
        }

        saw_timestamp = true;
        let text = js_trim(&line[timestamps.last().expect("non-empty").end..]);
        for tag in &timestamps {
            lines.push(LyricLine::new(Some(tag.seconds), text));
        }
    }

    if !saw_timestamp {
        return ParsedLyrics { synced: false, tags, lines };
    }

    // An untimed line mixed in with timed ones (formatLrc's own doc says that is an expected shape) sorts after
    // everything that has a time, so a trailing untimed line stays where it visually belongs; the sort is stable, so
    // several untimed lines keep their relative order. (Treating "no time" as 0 would teleport them to the front.)
    lines.sort_by(|a, b| {
        a.time_seconds.unwrap_or(f64::INFINITY).total_cmp(&b.time_seconds.unwrap_or(f64::INFINITY))
    });
    ParsedLyrics { synced: true, tags, lines }
}

/// Moves every timed line by `delta_seconds` (negative = earlier), for a file whose timings are consistent with each
/// other but all start too early or late. Untimed lines stay untimed, and a line that would land before the start of
/// the track is held at 0 rather than dropped.
pub fn shift_lyric_times(lines: &[LyricLine], delta_seconds: f64) -> Vec<LyricLine> {
    lines
        .iter()
        .map(|line| match line.time_seconds {
            None => line.clone(),
            Some(t) => LyricLine {
                time_seconds: Some((js_round((t + delta_seconds) * 1000.0) / 1000.0).max(0.0)),
                ..line.clone()
            },
        })
        .collect()
}

/// Inverse of [`parse_lrc`], for writing a freshly hand-synced result back out - a line with no time is written as
/// plain unsynced text (`parse_lrc` already tolerates a mix of timed and plain lines in one file; `synced` there just
/// means "at least one timestamp was found anywhere", not "every line has one"). `translation` is never written - it
/// only ever exists on a merged in-memory `ParsedLyrics`, not a single real .lrc file on disk.
pub fn format_lrc(lines: &[LyricLine], tags: &Tags) -> String {
    let mut out = String::new();
    for (key, value) in tags.iter() {
        out.push_str(&format!("[{key}:{value}]\n"));
    }
    for line in lines {
        match line.time_seconds {
            None => out.push_str(&line.text),
            Some(t) => out.push_str(&format!("[{}]{}", format_timestamp(t), line.text)),
        }
        out.push('\n');
    }
    // The TS joins with "\n" and appends one: an empty input is just "\n", not "".
    if out.is_empty() {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(t: Option<f64>, text: &str) -> LyricLine {
        LyricLine::new(t, text)
    }

    // ---- parseLrc (ported from lrc.test.ts) ----

    #[test]
    fn parses_timestamps_and_tags_sorted_by_time() {
        let text = ["[ti:Track One]", "[ar:Some Artist]", "[00:12.50]Second line", "[00:00.00]First line"].join("\n");
        assert_eq!(
            parse_lrc(&text),
            ParsedLyrics {
                synced: true,
                tags: Tags::from([("ti", "Track One"), ("ar", "Some Artist")]),
                lines: vec![line(Some(0.0), "First line"), line(Some(12.5), "Second line")],
            }
        );
    }

    #[test]
    fn expands_a_line_with_multiple_timestamps_into_one_entry_per_timestamp() {
        assert_eq!(
            parse_lrc("[00:10.00][00:40.00]Chorus line"),
            ParsedLyrics {
                synced: true,
                tags: Tags::new(),
                lines: vec![line(Some(10.0), "Chorus line"), line(Some(40.0), "Chorus line")],
            }
        );
    }

    #[test]
    fn accepts_timestamps_with_no_centiseconds_and_with_three_digit_centiseconds() {
        let text = ["[01:02]No centiseconds", "[00:01.500]Three digit centiseconds"].join("\n");
        assert_eq!(
            parse_lrc(&text),
            ParsedLyrics {
                synced: true,
                tags: Tags::new(),
                lines: vec![line(Some(1.5), "Three digit centiseconds"), line(Some(62.0), "No centiseconds")],
            }
        );
    }

    #[test]
    fn falls_back_to_unsynced_plain_text_lines_when_no_timestamp_tag_is_found() {
        let text = ["First line", "", "Second line", "[Chorus]", "Third line"].join("\n");
        assert_eq!(
            parse_lrc(&text),
            ParsedLyrics {
                synced: false,
                tags: Tags::new(),
                lines: vec![line(None, "First line"), line(None, "Second line"), line(None, "[Chorus]"), line(None, "Third line")],
            }
        );
    }

    #[test]
    fn keeps_a_trailing_untimed_line_at_the_end_not_sorted_to_the_front() {
        let text = ["[00:04.00]First line", "[00:08.00]Second line", "Third line, no timestamp yet"].join("\n");
        assert_eq!(
            parse_lrc(&text),
            ParsedLyrics {
                synced: true,
                tags: Tags::new(),
                lines: vec![line(Some(4.0), "First line"), line(Some(8.0), "Second line"), line(None, "Third line, no timestamp yet")],
            }
        );
    }

    #[test]
    fn keeps_multiple_untimed_lines_in_their_original_order_after_the_timed_ones() {
        let text = ["Untimed A", "[00:04.00]Timed", "Untimed B"].join("\n");
        assert_eq!(parse_lrc(&text).lines, vec![line(Some(4.0), "Timed"), line(None, "Untimed A"), line(None, "Untimed B")]);
    }

    #[test]
    fn handles_crlf_line_endings_and_blank_lines() {
        let text = "[00:00.00]Line one\r\n\r\n[00:05.00]Line two\r\n";
        assert_eq!(
            parse_lrc(text),
            ParsedLyrics {
                synced: true,
                tags: Tags::new(),
                lines: vec![line(Some(0.0), "Line one"), line(Some(5.0), "Line two")],
            }
        );
    }

    // ---- parseLrc edge cases the TS tests don't pin (each checked against the JS regexes) ----

    #[test]
    fn timestamp_syntax_is_exactly_the_js_regex() {
        // 4-digit minutes, 1-digit seconds, 4-digit fraction, a bare dot and non-ASCII digits are not timestamps.
        for not_a_timestamp in ["[1234:56]x", "[01:5]x", "[00:01.5000]x", "[00:01.]x", "[\u{0661}:\u{0662}\u{0663}]x", "[00:01 ]x", "[00:01"] {
            let p = parse_lrc(not_a_timestamp);
            assert!(!p.synced, "{not_a_timestamp}");
        }
        // 3-digit minutes are fine, and a 1-digit fraction is one tenth.
        assert_eq!(parse_lrc("[100:00.5]x").lines, vec![line(Some(6000.5), "x")]);
    }

    #[test]
    fn text_is_whatever_follows_the_last_timestamp_even_with_stray_brackets() {
        assert_eq!(parse_lrc("[00:01.00] a [00:02.00] b").lines, vec![line(Some(1.0), "b"), line(Some(2.0), "b")]);
        assert_eq!(parse_lrc("[00:01.00]").lines, vec![line(Some(1.0), "")]);
    }

    #[test]
    fn metadata_tags_lowercase_the_key_trim_the_value_and_last_one_wins() {
        let p = parse_lrc("[TI: First ]\n[Ar:x]\n[ti:Second]\n[00:01.00]a");
        assert_eq!(p.tags, Tags::from([("ti", "Second"), ("ar", "x")]));
        assert_eq!(p.tags.get("ti"), Some("Second"));
    }

    #[test]
    fn bracketed_text_that_is_not_a_tag_stays_a_lyric_line() {
        let p = parse_lrc("[00:01.00]a\n[Chorus]\n[x1:y]\n[:y]");
        assert_eq!(p.tags, Tags::new());
        assert_eq!(p.lines.len(), 4);
        assert_eq!(p.lines[0], line(Some(1.0), "a"));
    }

    #[test]
    fn a_line_with_a_timestamp_is_never_a_tag_even_if_it_looks_like_one() {
        let p = parse_lrc("[ar:foo [00:01.00]]");
        assert_eq!(p.tags, Tags::new());
        assert_eq!(p.lines, vec![line(Some(1.0), "]")]);
    }

    #[test]
    fn empty_input_is_unsynced_with_no_lines() {
        assert_eq!(parse_lrc(""), ParsedLyrics { synced: false, tags: Tags::new(), lines: vec![] });
    }

    // ---- formatLrc ----

    #[test]
    fn writes_tags_followed_by_timestamp_text_lines() {
        let text = format_lrc(&[line(Some(0.0), "First line"), line(Some(72.5), "Second line")], &Tags::from([("ti", "Track One")]));
        assert_eq!(text, "[ti:Track One]\n[00:00.00]First line\n[01:12.50]Second line\n");
    }

    #[test]
    fn writes_a_line_with_no_time_as_plain_unsynced_text() {
        assert_eq!(format_lrc(&[line(None, "Not yet timed")], &Tags::new()), "Not yet timed\n");
    }

    #[test]
    fn round_trips_through_parse() {
        let lines = vec![line(Some(0.0), "First line"), line(Some(5.25), "Second line")];
        assert_eq!(parse_lrc(&format_lrc(&lines, &Tags::new())).lines, lines);
    }

    #[test]
    fn nothing_to_write_is_a_single_newline_like_the_ts() {
        assert_eq!(format_lrc(&[], &Tags::new()), "\n");
    }

    #[test]
    fn timestamps_format_like_js_to_fixed() {
        assert_eq!(format_timestamp(83.4), "01:23.40");
        assert_eq!(format_timestamp(0.0), "00:00.00");
        assert_eq!(format_timestamp(5.0), "00:05.00");
        assert_eq!(format_timestamp(600.0), "10:00.00");
        assert_eq!(format_timestamp(6000.5), "100:00.50");
        // Exact binary ties round up in JS (1.125.toFixed(2) is "1.13"; Rust's {:.2} gives "1.12").
        assert_eq!(format_timestamp(1.125), "00:01.13");
        assert_eq!(format_timestamp(2.375), "00:02.38");
        // Not ties: both round to the nearest.
        assert_eq!(format_timestamp(1.005), "00:01.00"); // 1.005 is really 1.00499999999999989...
        assert_eq!(format_timestamp(59.999), "00:60.00"); // JS does not carry into the minutes either
    }

    // ---- shiftLyricTimes ----

    #[test]
    fn moves_timed_lines_leaves_untimed_ones_alone_and_holds_anything_negative_at_zero() {
        let mut later = line(Some(10.1), "later");
        later.translation = Some("x".into());
        let lines = vec![line(Some(0.5), "early"), line(None, "plain"), later.clone()];

        let shifted = shift_lyric_times(&lines, -1.0);
        let mut expected_later = line(Some(9.1), "later");
        expected_later.translation = Some("x".into());
        assert_eq!(shifted, vec![line(Some(0.0), "early"), line(None, "plain"), expected_later]);
        assert_eq!(shift_lyric_times(&lines, 0.2)[2].time_seconds, Some(10.3));
    }
}
