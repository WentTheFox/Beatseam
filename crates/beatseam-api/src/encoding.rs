//! UTF-8 and base64url (`packages/api/src/encoding.ts`). The TypeScript hand-rolled these because React Native's
//! JavaScript engine lacks `TextEncoder`/`atob`; in Rust UTF-8 is built in, so this is the base64url half plus the one
//! decoding rule that matters for compatibility.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Bytes that are not valid UTF-8 become U+FFFD, never an error - exactly what the WHATWG `TextDecoder` does (Rust's
/// lossy decoding uses the same "maximal subpart" replacement rule, which `tests/encoding_golden.rs` confirms).
pub fn utf8_decode(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// base64url without padding, as `Buffer#toString('base64url')` writes it.
pub fn base64_url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let (a, b, c) = (chunk[0], chunk.get(1).copied(), chunk.get(2).copied());
        out.push(ALPHABET[(a >> 2) as usize] as char);
        out.push(ALPHABET[(((a & 3) << 4) | (b.unwrap_or(0) >> 4)) as usize] as char);
        if let Some(b) = b {
            out.push(ALPHABET[(((b & 15) << 2) | (c.unwrap_or(0) >> 6)) as usize] as char);
        }
        if let Some(c) = c {
            out.push(ALPHABET[(c & 63) as usize] as char);
        }
    }
    out
}

/// A character outside the base64url alphabet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidBase64Url;

impl std::fmt::Display for InvalidBase64Url {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Invalid base64url")
    }
}

impl std::error::Error for InvalidBase64Url {}

/// Fails on characters outside the base64url alphabet. Trailing `=` padding, if present, is ignored, and (like the
/// TypeScript) bits left over after the last whole byte are ignored rather than checked.
pub fn base64_url_decode(text: &str) -> Result<Vec<u8>, InvalidBase64Url> {
    let clean = text.trim_end_matches('=');
    let mut bytes = Vec::with_capacity(clean.len() * 3 / 4);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for ch in clean.chars() {
        let value = ALPHABET.iter().position(|a| char::from(*a) == ch).ok_or(InvalidBase64Url)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push(((buffer >> bits) & 0xff) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_length() {
        for len in 0..40usize {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + len) as u8).collect();
            assert_eq!(base64_url_decode(&base64_url_encode(&bytes)).unwrap(), bytes, "{len}");
        }
    }

    #[test]
    fn uses_the_url_safe_alphabet_without_padding() {
        assert_eq!(base64_url_encode(&[0xfb, 0xff, 0xfe]), "-__-");
        assert_eq!(base64_url_encode(&[65]), "QQ");
        assert_eq!(base64_url_encode(&[]), "");
    }

    #[test]
    fn rejects_characters_outside_the_alphabet_and_ignores_padding() {
        assert_eq!(base64_url_decode("ab+/"), Err(InvalidBase64Url));
        assert_eq!(base64_url_decode("a b"), Err(InvalidBase64Url));
        assert_eq!(base64_url_decode("QQ=="), Ok(vec![65]));
    }

    #[test]
    fn malformed_utf8_becomes_replacement_characters() {
        assert_eq!(utf8_decode(&[0x41, 0x80, 0x42]), "A\u{fffd}B");
        assert_eq!(utf8_decode(&[0xe2, 0x82]), "\u{fffd}");
    }
}
