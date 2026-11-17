//! The unsynchronised lyrics frame (`USLT`, `ULT` in ID3v2.2) of an MP3's ID3v2 tag. Plenty of taggers put a whole `.lrc`
//! file in this frame, so the text is handed back as written and left to `parse_lrc`. Synchronised (`SYLT`) lyrics and
//! tags in other containers (FLAC, MP4) aren't read.
//!
//! Port of `readEmbeddedLyrics.ts`, bug-for-bug where the behaviour is observable: the hand-rolled UTF-8 decoder turns
//! stray continuation bytes into Latin-1 characters rather than U+FFFD, and offsets are 64-bit even on wasm32 (a corrupt
//! size field must not wrap). Where the TypeScript would build a string holding a lone UTF-16 surrogate, this yields
//! U+FFFD (a Rust `String` cannot hold one).

/// How many bytes an ID3v2 tag occupies at the start of `head` (header included), or 0 when there isn't one.
pub fn id3v2_tag_length(head: &[u8]) -> usize {
    if head.len() < 10 || head[0] != 0x49 || head[1] != 0x44 || head[2] != 0x33 {
        return 0;
    }
    (10 + synchsafe(head, 6)) as usize
}

fn byte(bytes: &[u8], i: usize) -> u8 {
    bytes.get(i).copied().unwrap_or(0)
}

fn synchsafe(bytes: &[u8], at: usize) -> u64 {
    u64::from(byte(bytes, at) & 0x7f) << 21 | u64::from(byte(bytes, at + 1) & 0x7f) << 14 | u64::from(byte(bytes, at + 2) & 0x7f) << 7 | u64::from(byte(bytes, at + 3) & 0x7f)
}

fn uint32(bytes: &[u8], at: usize) -> u64 {
    u64::from(u32::from_be_bytes([byte(bytes, at), byte(bytes, at + 1), byte(bytes, at + 2), byte(bytes, at + 3)]))
}

fn utf16(bytes: &[u8], big_endian_default: bool) -> String {
    let (mut big_endian, mut start) = (big_endian_default, 0);
    if bytes.len() >= 2 && bytes[0] == 0xff && bytes[1] == 0xfe {
        (big_endian, start) = (false, 2);
    } else if bytes.len() >= 2 && bytes[0] == 0xfe && bytes[1] == 0xff {
        (big_endian, start) = (true, 2);
    }
    let units: Vec<u16> = bytes[start..].chunks_exact(2).map(|p| if big_endian { u16::from_be_bytes([p[0], p[1]]) } else { u16::from_le_bytes([p[0], p[1]]) }).collect();
    String::from_utf16_lossy(&units)
}

fn utf8(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let extra = if b >= 0xf0 { 3 } else if b >= 0xe0 { 2 } else if b >= 0xc0 { 1 } else { 0 };
        let mut code = if extra == 0 { u32::from(b) } else { u32::from(b) & (0x3f >> extra) };
        for k in 1..=extra {
            code = (code << 6) | (u32::from(byte(bytes, i + k)) & 0x3f);
        }
        out.push(if code <= 0x10ffff { char::from_u32(code).unwrap_or('\u{fffd}') } else { '\u{fffd}' });
        i += extra + 1;
    }
    out
}

/// ID3 text encodings: 0 ISO-8859-1, 1 UTF-16 with BOM, 2 UTF-16BE, 3 UTF-8.
fn decode_text(bytes: &[u8], encoding: u8) -> String {
    match encoding {
        1 => utf16(bytes, false),
        2 => utf16(bytes, true),
        3 => utf8(bytes),
        _ => bytes.iter().map(|b| char::from(*b)).collect(),
    }
}

/// Where the descriptor's terminator ends, from `from`: one NUL byte, or an aligned NUL pair for the UTF-16 encodings.
fn after_terminator(bytes: &[u8], from: usize, encoding: u8) -> usize {
    let wide = encoding == 1 || encoding == 2;
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == 0 && (!wide || bytes.get(i + 1) == Some(&0)) {
            return i + if wide { 2 } else { 1 };
        }
        i += if wide { 2 } else { 1 };
    }
    bytes.len()
}

/// Undoes ID3's "unsynchronisation" (every `0xFF 0x00` stands for a lone `0xFF`).
fn unsynchronise(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        out.push(bytes[i]);
        if bytes[i] == 0xff && bytes.get(i + 1) == Some(&0) {
            i += 1;
        }
        i += 1;
    }
    out
}

/// The unsynchronised lyrics of the ID3v2 tag at the start of `tag`, or `None` when it has none or isn't parseable. Pass
/// at least [`id3v2_tag_length`]`(head)` bytes.
pub fn read_embedded_lyrics(tag: &[u8]) -> Option<String> {
    let total = id3v2_tag_length(tag);
    if total == 0 {
        return None;
    }
    let major = tag[3];
    if !(2..=4).contains(&major) {
        return None;
    }
    let flags = tag[5];
    let mut body: Vec<u8> = tag[10..total.min(tag.len())].to_vec();
    if flags & 0x80 != 0 && major < 4 {
        body = unsynchronise(&body);
    }
    let mut at: u64 = 0;
    if flags & 0x40 != 0 && major >= 3 && body.len() >= 4 {
        at = if major == 4 { synchsafe(&body, 0) } else { 4 + uint32(&body, 0) };
    }

    let id_length: u64 = if major == 2 { 3 } else { 4 };
    let header_length: u64 = if major == 2 { 6 } else { 10 };
    let len = body.len() as u64;
    while at + header_length <= len && body[at as usize] != 0 {
        let a = at as usize;
        let id: String = body[a..a + id_length as usize].iter().map(|b| char::from(*b)).collect();
        let size = match major {
            2 => u64::from(body[a + 3]) << 16 | u64::from(body[a + 4]) << 8 | u64::from(body[a + 5]),
            4 => synchsafe(&body, a + 4),
            _ => uint32(&body, a + 4),
        };
        let data_start = at + header_length;
        if data_start + size > len {
            return None;
        }
        if id == "USLT" || id == "ULT" {
            let data = &body[data_start as usize..(data_start + size) as usize];
            if data.len() < 5 {
                return None;
            }
            let encoding = data[0];
            // 1 encoding byte, 3 language bytes, the descriptor, then the lyrics themselves.
            let text = decode_text(&data[after_terminator(data, 4, encoding)..], encoding);
            let text = text.trim_end_matches('\0').to_owned();
            return if crate::js_compat::js_trim(&text).is_empty() { None } else { Some(text) };
        }
        at = data_start + size;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synchsafe_bytes(n: usize) -> [u8; 4] {
        [(n >> 21) as u8 & 0x7f, (n >> 14) as u8 & 0x7f, (n >> 7) as u8 & 0x7f, n as u8 & 0x7f]
    }

    /// An ID3v2.3 tag holding one USLT frame.
    fn v23_tag(encoding: u8, descriptor: &[u8], lyrics: &[u8]) -> Vec<u8> {
        let mut data = vec![encoding];
        data.extend(b"eng");
        data.extend(descriptor);
        data.extend(lyrics);
        let mut frame = b"USLT".to_vec();
        frame.extend((data.len() as u32).to_be_bytes());
        frame.extend([0, 0]);
        frame.extend(data);
        let mut tag = b"ID3".to_vec();
        tag.extend([3, 0, 0]);
        tag.extend(synchsafe_bytes(frame.len()));
        tag.extend(frame);
        tag
    }

    #[test]
    fn reads_latin1_lyrics_after_an_empty_descriptor() {
        assert_eq!(read_embedded_lyrics(&v23_tag(0, b"\0", b"[00:01.00]Hi")).as_deref(), Some("[00:01.00]Hi"));
    }

    #[test]
    fn reads_utf8_and_utf16_lyrics_with_a_descriptor() {
        assert_eq!(read_embedded_lyrics(&v23_tag(3, b"desc\0", "héllo 日本".as_bytes())).as_deref(), Some("héllo 日本"));
        let mut utf16le = vec![0xff, 0xfe];
        utf16le.extend("héllo".encode_utf16().flat_map(u16::to_le_bytes));
        let mut descriptor = vec![0xff, 0xfe];
        descriptor.extend("d".encode_utf16().flat_map(u16::to_le_bytes));
        descriptor.extend([0, 0]);
        assert_eq!(read_embedded_lyrics(&v23_tag(1, &descriptor, &utf16le)).as_deref(), Some("héllo"));
    }

    #[test]
    fn a_tag_without_lyrics_or_with_blank_ones_or_not_a_tag_at_all_is_none() {
        assert_eq!(read_embedded_lyrics(&v23_tag(0, b"\0", b"  \n ")), None);
        assert_eq!(read_embedded_lyrics(b"not an id3 tag"), None);
        assert_eq!(read_embedded_lyrics(&[]), None);
        assert_eq!(read_embedded_lyrics(b"ID3\x05\x00\x00\x00\x00\x00\x00"), None, "unsupported major version");
        assert_eq!(id3v2_tag_length(b"ID3\x03\x00\x00\x00\x00\x02\x01"), 10 + 257);
        assert_eq!(id3v2_tag_length(b"ID"), 0);
    }

    #[test]
    fn a_frame_that_claims_more_bytes_than_the_tag_has_is_none_not_a_panic() {
        let mut tag = v23_tag(0, b"\0", b"abc");
        let n = tag.len();
        tag[14..18].copy_from_slice(&1_000_000u32.to_be_bytes());
        assert_eq!(read_embedded_lyrics(&tag), None);
        assert_eq!(read_embedded_lyrics(&tag[..n - 3]), None);
        for cut in 0..n {
            let _ = read_embedded_lyrics(&tag[..cut]);
        }
    }

    #[test]
    fn the_utf8_decoder_keeps_the_typescripts_leniency_for_stray_bytes() {
        // A lone continuation byte becomes the matching Latin-1 character, not U+FFFD.
        assert_eq!(read_embedded_lyrics(&v23_tag(3, b"\0", &[0x41, 0x80, 0x42])).as_deref(), Some("A\u{80}B"));
    }
}
