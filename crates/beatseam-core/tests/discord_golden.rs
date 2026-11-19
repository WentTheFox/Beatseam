//! Differential test: SET_ACTIVITY frames byte-for-byte against the TypeScript (`tools/gen-discord-golden.mjs`).

use beatseam_core::discord::{DiscordNowPlayingInfo, NonceGenerator, build_set_activity_frame};
use serde_json::Value;

#[test]
fn frames_are_byte_identical_to_the_typescript() {
    let golden: Value = serde_json::from_str(include_str!("data/discord_golden.json")).unwrap();
    let now = golden["now"].as_i64().unwrap();
    let mut nonces = NonceGenerator::new(); // the TS counter is module-level, so it runs 1, 2, 3, ... across the cases
    let frames = golden["frames"].as_array().unwrap();
    assert!(frames.len() >= 10);
    for (i, case) in frames.iter().enumerate() {
        let info = case["info"].as_object().map(|o| DiscordNowPlayingInfo {
            title: o["title"].as_str().unwrap().into(),
            artist: o["artist"].as_str().unwrap().into(),
            started_at_unix_ms: o["startedAtUnixMs"].as_i64().unwrap(),
            album_art_url: o["albumArtUrl"].as_str().map(Into::into),
        });
        let got = build_set_activity_frame(info.as_ref(), &nonces.next(now));
        assert_eq!(got, case["frame"].as_str().unwrap(), "case {i}: {}", case["info"]);
    }
}
