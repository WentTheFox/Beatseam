const BASE64_CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with `=` padding. The TypeScript carried its own encoder because a global `btoa` couldn't be assumed
/// across the React Native engines; in Rust this is a dependency-free helper (used for data: URIs of cover art and the
/// JNI/Kotlin hand-off) that keeps `beatseam-core` light for wasm.
pub fn encode_base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let (b0, b1, b2) = (chunk[0], chunk.get(1).copied(), chunk.get(2).copied());
        out.push(BASE64_CHARS[(b0 >> 2) as usize] as char);
        out.push(BASE64_CHARS[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        match b1 {
            None => out.push('='),
            Some(b1) => out.push(BASE64_CHARS[(((b1 & 0x0f) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char),
        }
        match b2 {
            None => out.push('='),
            Some(b2) => out.push(BASE64_CHARS[(b2 & 0x3f) as usize] as char),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_rfc_4648_test_vectors() {
        for (input, expected) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(encode_base64(input.as_bytes()), expected, "{input:?}");
        }
    }

    #[test]
    fn encodes_every_byte_value() {
        let all: Vec<u8> = (0..=255).collect();
        let encoded = encode_base64(&all);
        assert_eq!(encoded.len(), 344);
        // Reference from Node's Buffer.toString('base64').
        assert!(encoded.starts_with("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwd"));
        assert!(encoded.ends_with("+fr7/P3+/w=="));
    }
}
