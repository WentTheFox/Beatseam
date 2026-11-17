//! Small JavaScript semantics the ported code has to reproduce exactly, because values it computes are persisted or
//! compared across the TypeScript and Rust apps.

/// The characters JavaScript treats as whitespace in `String.prototype.trim` and in regular expressions' `\s`: tab, LF,
/// VT, FF, CR, space, NBSP, U+1680, U+2000-U+200A, LS, PS, U+202F, U+205F, U+3000 and the byte order mark U+FEFF.
/// (Rust's `char::is_whitespace` differs in two places: it also accepts U+0085, which JavaScript does not, and does not
/// accept U+FEFF, which JavaScript does.)
pub fn js_is_whitespace(c: char) -> bool {
    matches!(c, '\u{9}'..='\u{d}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

/// `String.prototype.trim`.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(js_is_whitespace)
}

/// `Math.round`: halves round up (towards +infinity), unlike `f64::round`, which rounds them away from zero (`-1.5` is
/// `-1` in JavaScript and `-2` in Rust).
pub fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_is_exactly_the_set_node_reports() {
        // Every BMP code point that `"x".trim() === ""` / `/\s/.test(x)` accepts in Node 24.
        let expected: Vec<u32> = [0x9, 0xa, 0xb, 0xc, 0xd, 0x20, 0xa0, 0x1680]
            .into_iter()
            .chain(0x2000..=0x200a)
            .chain([0x2028, 0x2029, 0x202f, 0x205f, 0x3000, 0xfeff])
            .collect();
        let actual: Vec<u32> = (0..=0xffffu32).filter(|c| char::from_u32(*c).is_some_and(js_is_whitespace)).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn trim_keeps_nel_and_strips_the_bom() {
        assert_eq!(js_trim("\u{feff} a \u{85}"), "a \u{85}");
        assert_eq!(js_trim("\u{3000}\u{a0}x\u{2028}"), "x");
        assert_eq!(js_trim(""), "");
    }

    #[test]
    fn round_matches_math_round() {
        assert_eq!([js_round(2.5), js_round(-1.5), js_round(-2.5), js_round(0.49), js_round(-0.5)], [3.0, -1.0, -2.0, 0.0, 0.0]);
    }
}
