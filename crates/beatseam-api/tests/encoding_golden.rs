//! Differential test: `utf8_decode` against `TextDecoder` (malformed input becomes U+FFFD, never an error) and
//! `base64_url_encode`/`decode` against Node's `base64url`, over thousands of mostly-malformed byte strings
//! (`tools/gen-encoding-golden.mjs`).

use beatseam_api::encoding::{base64_url_decode, base64_url_encode, utf8_decode};
use serde_json::Value;

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

#[test]
fn utf8_and_base64url_match_node() {
    let cases: Value = serde_json::from_str(include_str!("data/encoding_golden.json")).unwrap();
    let cases = cases.as_array().unwrap();
    assert!(cases.len() > 4000);
    for c in cases {
        let bytes = unhex(c["hex"].as_str().unwrap());
        assert_eq!(utf8_decode(&bytes), c["decoded"].as_str().unwrap(), "decode {}", c["hex"]);
        let b64 = c["base64url"].as_str().unwrap();
        assert_eq!(base64_url_encode(&bytes), b64, "encode {}", c["hex"]);
        assert_eq!(base64_url_decode(b64).unwrap(), bytes, "decode {b64}");
    }
}
