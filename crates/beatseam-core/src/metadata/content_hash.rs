/// FNV-1a (32-bit) over the given bytes, as an 8-character lowercase hex string - the same algorithm/house style as the
/// apps' `coverArtFileName` (there over a string, here over raw file bytes). Not cryptographic - this is purely a "did
/// this file's content change" signal tied to `TrackMetadata::content_hash`, not a security boundary, so FNV-1a's real
/// but astronomically small collision risk for a single file's before/after content is an acceptable tradeoff for
/// staying a simple, dependency-free loop fast enough to run on multi-MB audio files.
///
/// The value must match what the TypeScript apps stored, since a changed hash means "file changed" and triggers a
/// re-read.
pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut hash: u32 = 0x811c9dc5;
    for b in bytes {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{hash:08x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_deterministic_for_the_same_bytes() {
        assert_eq!(hash_bytes(&[1, 2, 3, 4, 5]), hash_bytes(&[1, 2, 3, 4, 5]));
    }

    #[test]
    fn differs_for_different_content_of_the_same_length() {
        assert_ne!(hash_bytes(&[1, 2, 3]), hash_bytes(&[1, 2, 4]));
    }

    #[test]
    fn differs_for_the_empty_vs_non_empty_input() {
        assert_ne!(hash_bytes(&[]), hash_bytes(&[0]));
    }

    #[test]
    fn returns_a_fixed_width_8_char_lowercase_hex_string() {
        for h in [hash_bytes(&[1, 2, 3]), hash_bytes(&[])] {
            assert_eq!(h.len(), 8);
            assert!(h.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)));
        }
    }

    #[test]
    fn matches_the_published_fnv1a_test_vectors_and_the_ts_output() {
        // Reference 32-bit FNV-1a values; the TypeScript gives the same (checked in Node).
        assert_eq!(hash_bytes(b""), "811c9dc5");
        assert_eq!(hash_bytes(b"a"), "e40c292c");
        assert_eq!(hash_bytes(b"foobar"), "bf9cf968");
    }
}
