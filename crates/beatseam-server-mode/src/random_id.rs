const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// A random url-safe id (22 characters by default, ~130 bits) for things the client names itself - queued operations and
/// playlists created offline. Not a secret: uniqueness is what matters. `fill` supplies random bytes (the platform's
/// secure source; the core crate has no randomness dependency).
pub fn random_id(fill: &mut dyn FnMut(&mut [u8]), length: usize) -> String {
    let mut bytes = vec![0u8; length];
    fill(&mut bytes);
    bytes.iter().map(|b| ALPHABET[(b & 63) as usize] as char).collect()
}

pub const DEFAULT_ID_LENGTH: usize = 22;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_each_byte_to_a_url_safe_character() {
        let mut next = 0u8;
        let id = random_id(
            &mut |buf| {
                for b in buf {
                    *b = next;
                    next = next.wrapping_add(1);
                }
            },
            DEFAULT_ID_LENGTH,
        );
        assert_eq!(id, "ABCDEFGHIJKLMNOPQRSTUV");
        assert_eq!(random_id(&mut |buf| buf.fill(255), 3), "___");
        assert_eq!(random_id(&mut |buf| buf.fill(62), 2), "--");
    }
}
