//! Differential test: `read_embedded_lyrics` and `id3v2_tag_length` against the TypeScript over hand-built ID3v2.2/2.3/2.4
//! tags (every text encoding, unsynchronisation, extended headers) and thousands of truncated or randomly corrupted ones
//! (`tools/gen-embedded-lyrics-golden.mjs`).

use beatseam_core::metadata::{id3v2_tag_length, read_embedded_lyrics};
use serde_json::Value;

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

#[test]
fn embedded_lyrics_match_the_typescript() {
    let cases: Value = serde_json::from_str(include_str!("data/embedded_lyrics_golden.json")).unwrap();
    let cases = cases.as_array().unwrap();
    assert!(cases.len() > 600);
    let mut with_lyrics = 0;
    for (i, c) in cases.iter().enumerate() {
        let bytes = unhex(c["hex"].as_str().unwrap());
        assert_eq!(id3v2_tag_length(&bytes) as u64, c["length"].as_u64().unwrap(), "case {i}: length of {}", c["hex"]);
        let got = read_embedded_lyrics(&bytes);
        assert_eq!(got.as_deref(), c["lyrics"].as_str(), "case {i}: {}", c["hex"]);
        with_lyrics += usize::from(got.is_some());
    }
    assert!(with_lyrics > 100, "only {with_lyrics} cases produced lyrics");
}
