//! Differential test for the scrobble port: md5 digests, the exact HTTP requests each Last.fm call builds, and the
//! scrobble queue's JSON, all compared with what the original TypeScript produced
//! (`tools/gen-scrobble-golden.mjs` -> `tests/data/scrobble_golden.json`).

use std::cell::RefCell;

use beatseam_core::http::{HttpRequest, HttpResponse, HttpTransport, Method, TransportError, block_on};
use beatseam_core::scrobble::{
    LastFmClient, LastFmCredentials, ScrobbleEntry, build_auth_url, build_web_auth_url, decode_scrobble_queue, encode_scrobble_queue, md5,
};
use serde_json::Value;

fn golden() -> Value {
    serde_json::from_str(include_str!("data/scrobble_golden.json")).unwrap()
}

#[test]
fn md5_matches_the_typescript() {
    let golden = golden();
    let cases = golden["md5Cases"].as_array().unwrap();
    assert!(cases.len() > 200);
    for c in cases {
        assert_eq!(md5(c["input"].as_str().unwrap()), c["md5"].as_str().unwrap(), "input {:?}", c["input"]);
    }
}

fn entry(v: &Value) -> ScrobbleEntry {
    ScrobbleEntry {
        artist: v["artist"].as_str().unwrap().into(),
        track: v["track"].as_str().unwrap().into(),
        timestamp: v["timestamp"].as_i64().unwrap(),
        album: v["album"].as_str().map(Into::into),
        duration_seconds: v["durationSeconds"].as_f64(),
    }
}

/// Answers every request with a body that satisfies all four calls, recording what was sent.
struct Recorder(RefCell<Vec<HttpRequest>>);

impl HttpTransport for &Recorder {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.0.borrow_mut().push(request);
        Ok(HttpResponse { status: 200, body: br#"{"token":"tok","session":{"key":"sk","name":"user"}}"#.to_vec() })
    }
}

#[test]
fn last_fm_requests_match_the_typescript_byte_for_byte() {
    let golden = golden();
    let cases = golden["lastfmCases"].as_array().unwrap();
    assert_eq!(cases.len(), 25);
    for c in cases {
        let recorder = Recorder(RefCell::new(Vec::new()));
        let client = LastFmClient::new(LastFmCredentials { api_key: "test key&1".into(), api_secret: "sécret".into() }, &recorder);
        let (call, args) = (c["call"].as_str().unwrap(), &c["args"]);
        match call {
            "getAuthToken" => drop(block_on(client.get_auth_token()).unwrap()),
            "getSession" => drop(block_on(client.get_session(args["token"].as_str().unwrap())).unwrap()),
            "updateNowPlaying" => block_on(client.update_now_playing(
                args["sessionKey"].as_str().unwrap(),
                args["artist"].as_str().unwrap(),
                args["track"].as_str().unwrap(),
                args["album"].as_str(),
                args["durationSeconds"].as_f64(),
            ))
            .unwrap(),
            "scrobbleBatch" => {
                let entries: Vec<_> = args["entries"].as_array().unwrap().iter().map(entry).collect();
                block_on(client.scrobble_batch(args["sessionKey"].as_str().unwrap(), &entries)).unwrap();
            }
            other => panic!("unknown call {other}"),
        }

        let sent = recorder.0.borrow();
        let want = c["requests"].as_array().unwrap();
        assert_eq!(sent.len(), want.len(), "{call} {args}");
        for (got, want) in sent.iter().zip(want) {
            let method = if want["method"] == "POST" { Method::Post } else { Method::Get };
            assert_eq!(got.method, method, "{call} {args}");
            assert_eq!(got.url, want["url"].as_str().unwrap(), "{call} {args}");
            let headers: Vec<(String, String)> =
                want["headers"].as_array().unwrap().iter().map(|h| (h[0].as_str().unwrap().into(), h[1].as_str().unwrap().into())).collect();
            assert_eq!(got.headers, headers, "{call} {args}");
            assert_eq!(got.body.as_deref(), want["body"].as_str(), "{call} {args}");
        }
    }
}

#[test]
fn auth_urls_match_the_typescript() {
    let golden = golden();
    let credentials = LastFmCredentials { api_key: "test key&1".into(), api_secret: "sécret".into() };
    for u in golden["urls"].as_array().unwrap() {
        let token = u["token"].as_str().unwrap();
        assert_eq!(build_auth_url(&credentials, token), u["auth"].as_str().unwrap());
        assert_eq!(build_web_auth_url(&credentials, &format!("https://x.example/cb?t={token}&u=1")), u["web"].as_str().unwrap());
    }
}

#[test]
fn scrobble_queue_encoding_matches_json_stringify() {
    let golden = golden();
    for c in golden["queueEncode"].as_array().unwrap() {
        let entries: Vec<_> = c["entries"].as_array().unwrap().iter().map(entry).collect();
        assert_eq!(encode_scrobble_queue(&entries), c["encoded"].as_str().unwrap(), "{}", c["entries"]);
    }
}

#[test]
fn scrobble_queue_decoding_matches_the_typescript() {
    let golden = golden();
    for c in golden["queueDecode"].as_array().unwrap() {
        let raw = c["raw"].as_str();
        let want: Vec<_> = c["entries"].as_array().unwrap().iter().map(entry).collect();
        assert_eq!(decode_scrobble_queue(raw), want, "{raw:?}");
    }
}
