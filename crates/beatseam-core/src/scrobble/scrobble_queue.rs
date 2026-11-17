use serde_json::Value;

use super::lastfm::ScrobbleEntry;

/// Cap on how many failed scrobbles get held for retry - Last.fm's own `track.scrobble` batch limit is 50 per call, so
/// this just needs to comfortably outlast a stretch of being offline without growing the persisted settings value
/// unboundedly; oldest entries are dropped first once it's exceeded.
pub const MAX_QUEUED_SCROBBLES: usize = 200;

/// The queue entry shape check from the TypeScript (`isScrobbleEntry`): artist/track strings, a numeric timestamp, and
/// album/duration each a string/number or null. Notably a *missing* album or duration key is invalid there (it must be
/// `null`), so an entry written without them is dropped.
fn decode_entry(value: &Value) -> Option<ScrobbleEntry> {
    let v = value.as_object()?;
    let album = match v.get("album")? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        _ => return None,
    };
    let duration_seconds = match v.get("durationSeconds")? {
        Value::Null => None,
        Value::Number(n) => Some(n.as_f64()?),
        _ => return None,
    };
    // JSON numbers are doubles in the TypeScript; a fractional timestamp (never written by the app) is floored.
    let timestamp = v.get("timestamp")?.as_f64()?.floor() as i64;
    Some(ScrobbleEntry { artist: v.get("artist")?.as_str()?.to_owned(), track: v.get("track")?.as_str()?.to_owned(), timestamp, album, duration_seconds })
}

/// Parses the queue persisted via the library store's generic settings store - corrupt/foreign JSON is treated as an
/// empty queue rather than reported, since losing a handful of not-yet-submitted scrobbles is harmless.
pub fn decode_scrobble_queue(raw: Option<&str>) -> Vec<ScrobbleEntry> {
    let Some(raw) = raw.filter(|r| !r.is_empty()) else { return Vec::new() };
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Array(items)) => items.iter().filter_map(decode_entry).collect(),
        _ => Vec::new(),
    }
}

/// `String(n)` for a finite number, the way JavaScript prints it: shortest round-trip digits, no fraction on integers,
/// and exponent form below 1e-6 and from 1e21 up (`1e-7`, `1.5e+21`). (`JSON.stringify` writes `null` for NaN/Infinity;
/// durations never are, so this maps them to `null` too.)
fn js_number(n: f64) -> String {
    if !n.is_finite() {
        return "null".into();
    }
    if n == 0.0 {
        return "0".into();
    }
    if n.abs() >= 1e21 || n.abs() < 1e-6 {
        let formatted = format!("{n:e}"); // e.g. "1.5e-7", "1e21"
        let (mantissa, exponent) = formatted.split_once('e').expect("exponent form");
        let sign = if exponent.starts_with('-') { "" } else { "+" };
        return format!("{mantissa}e{sign}{exponent}");
    }
    format!("{n}")
}

/// `JSON.stringify(entries)`: same key order as the TypeScript (`artist`, `track`, `timestamp`, `album`,
/// `durationSeconds`), so what one app version writes the other reads and, ideally, byte-identical.
pub fn encode_scrobble_queue(entries: &[ScrobbleEntry]) -> String {
    let string = |s: &str| serde_json::to_string(s).expect("a string always serializes");
    let items: Vec<String> = entries
        .iter()
        .map(|e| {
            format!(
                r#"{{"artist":{},"track":{},"timestamp":{},"album":{},"durationSeconds":{}}}"#,
                string(&e.artist),
                string(&e.track),
                e.timestamp,
                e.album.as_deref().map_or("null".to_string(), string),
                e.duration_seconds.map_or("null".to_string(), js_number),
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

pub fn append_to_scrobble_queue(existing: &[ScrobbleEntry], entry: ScrobbleEntry) -> Vec<ScrobbleEntry> {
    let mut next = existing.to_vec();
    next.push(entry);
    if next.len() > MAX_QUEUED_SCROBBLES {
        next.drain(..next.len() - MAX_QUEUED_SCROBBLES);
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> ScrobbleEntry {
        ScrobbleEntry { artist: "A".into(), track: "T".into(), timestamp: 1, album: None, duration_seconds: Some(200.0) }
    }

    #[test]
    fn round_trips_through_encode_decode() {
        let entries = vec![entry(), ScrobbleEntry { album: Some("Alb \"q\" é".into()), duration_seconds: None, timestamp: 1_700_000_000, ..entry() }];
        assert_eq!(decode_scrobble_queue(Some(&encode_scrobble_queue(&entries))), entries);
    }

    #[test]
    fn encodes_exactly_like_json_stringify() {
        assert_eq!(encode_scrobble_queue(&[entry()]), r#"[{"artist":"A","track":"T","timestamp":1,"album":null,"durationSeconds":200}]"#);
        assert_eq!(encode_scrobble_queue(&[]), "[]");
        let fractional = ScrobbleEntry { duration_seconds: Some(200.5), ..entry() };
        assert!(encode_scrobble_queue(&[fractional]).ends_with(r#""durationSeconds":200.5}]"#));
    }

    #[test]
    fn treats_missing_corrupt_or_foreign_json_as_an_empty_queue() {
        for raw in [None, Some(""), Some("not json"), Some(r#"{"not":"an array"}"#), Some(r#"[{"garbage":true}]"#), Some("null"), Some("3")] {
            assert_eq!(decode_scrobble_queue(raw), vec![], "{raw:?}");
        }
    }

    #[test]
    fn keeps_valid_entries_and_drops_invalid_ones_in_a_mixed_array() {
        let raw = r#"[{"artist":"A","track":"T","timestamp":5,"album":null,"durationSeconds":null},{"artist":1},{"artist":"B","track":"U","timestamp":"6","album":null,"durationSeconds":null},{"artist":"C","track":"V","timestamp":7,"album":"X","durationSeconds":30}]"#;
        let decoded = decode_scrobble_queue(Some(raw));
        assert_eq!(decoded.iter().map(|e| e.artist.as_str()).collect::<Vec<_>>(), ["A", "C"]);
    }

    #[test]
    fn an_entry_missing_its_album_or_duration_key_is_invalid_like_in_the_ts() {
        assert_eq!(decode_scrobble_queue(Some(r#"[{"artist":"A","track":"T","timestamp":1}]"#)), vec![]);
        assert_eq!(decode_scrobble_queue(Some(r#"[{"artist":"A","track":"T","timestamp":1,"album":null}]"#)), vec![]);
    }

    #[test]
    fn caps_the_queue_length_dropping_the_oldest_entries_first() {
        let mut queue = Vec::new();
        for i in 0..MAX_QUEUED_SCROBBLES + 10 {
            queue = append_to_scrobble_queue(&queue, ScrobbleEntry { timestamp: i as i64, ..entry() });
        }
        assert_eq!(queue.len(), MAX_QUEUED_SCROBBLES);
        assert_eq!(queue[0].timestamp, 10);
        assert_eq!(queue.last().unwrap().timestamp, MAX_QUEUED_SCROBBLES as i64 + 9);
    }
}
