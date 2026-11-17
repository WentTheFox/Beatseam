use crate::encoding::{InvalidBase64Url, base64_url_decode, base64_url_encode, utf8_decode};

/// Track ids are core's file ids (`${rootId}:${relativePath}`), which contain `/` and `:` - awkward in a URL path
/// segment (reverse proxies commonly reject an encoded `%2F`). The API therefore exposes them as an opaque base64url
/// "key" and the server decodes it back.
pub fn encode_track_key(file_id: &str) -> String {
    base64_url_encode(file_id.as_bytes())
}

pub fn decode_track_key(key: &str) -> Result<String, InvalidBase64Url> {
    Ok(utf8_decode(&base64_url_decode(key)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_awkward_file_ids() {
        for file_id in ["Music:Album/one.mp3", "Léa: 日本語/こんにちは ♪.flac", "a:b?c#d%2F e+f/=.mp3", ""] {
            let key = encode_track_key(file_id);
            assert!(key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'), "{key}");
            assert_eq!(decode_track_key(&key).unwrap(), file_id);
        }
    }

    #[test]
    fn a_key_with_a_foreign_character_is_an_error() {
        assert_eq!(decode_track_key("not a key!"), Err(InvalidBase64Url));
    }
}
