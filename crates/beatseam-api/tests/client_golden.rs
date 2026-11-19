//! Differential test: what `ApiClient` sends and how it reports failures, against the TypeScript client
//! (`tools/gen-api-golden.mjs` -> `tests/data/client_golden.json`).

use std::cell::RefCell;

use beatseam_api::lastfm::LastFmNowPlayingRequestDto;
use beatseam_api::mutations::MutationDto;
use beatseam_api::{ApiClient, ApiClientError, ApiClientOptions, ListTracksQuery, LyricsSaveLineDto};
use beatseam_core::http::{HttpRequest, HttpResponse, HttpTransport, Method, TransportError, block_on};
use serde_json::{Number, Value};

fn golden() -> Value {
    serde_json::from_str(include_str!("data/client_golden.json")).unwrap()
}

fn dto_sample(name: &str) -> Value {
    let dto: Value = serde_json::from_str(include_str!("data/dto_golden.json")).unwrap();
    dto.as_array().unwrap().iter().find(|c| c["name"] == name).unwrap_or_else(|| panic!("no dto sample {name}"))["valid"][0]["input"].clone()
}

/// What the TypeScript's fake `fetch` answered for each call (the answer doesn't affect the request, it only has to be
/// something the call can parse).
fn canned(call: &str) -> (Vec<u8>, &'static str) {
    let json = |v: Value| (v.to_string().into_bytes(), "application/json");
    match call {
        "authStatus" => json(dto_sample("authStatus")),
        "me" => json(dto_sample("me")),
        "deviceCode" => json(dto_sample("deviceCodeResult")),
        "deviceWait" => json(dto_sample("deviceWaitResult")),
        "lastFmStatus" | "lastFmComplete" | "lastFmDisconnect" => json(dto_sample("lastFmStatus")),
        "lastFmConnect" => json(dto_sample("lastFmConnectResult")),
        "listTracks" => json(dto_sample("page")),
        "syncChanges" => json(dto_sample("syncChangesResponse")),
        "syncSnapshot" => json(dto_sample("syncSnapshotResponse")),
        "mutations" => json(dto_sample("mutationsResponse")),
        "lyrics" => json(dto_sample("lyrics")),
        "lyricsReports" => json(serde_json::json!([])),
        "lyricsSource" => json(dto_sample("lyricsSource")),
        "fetchFile" => (b"bytes".to_vec(), "image/webp"),
        c if c.starts_with("fetch") => (b"bytes".to_vec(), "application/json"),
        _ => json(serde_json::json!({})),
    }
}

struct Recorder {
    sent: RefCell<Vec<HttpRequest>>,
    answer: (Vec<u8>, &'static str),
}

impl HttpTransport for &Recorder {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.sent.borrow_mut().push(request);
        Ok(HttpResponse::new(200, self.answer.0.clone()).with_header("content-type", self.answer.1))
    }
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

fn normalize(v: Value) -> Value {
    match v {
        Value::Number(n) => Value::Number(Number::from_f64(n.as_f64().unwrap()).unwrap()),
        Value::Array(items) => Value::Array(items.into_iter().map(normalize).collect()),
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, normalize(v))).collect()),
        other => other,
    }
}

/// Runs one recorded call against the Rust client.
fn run(client: &ApiClient<&Recorder>, call: &str, args: &Value) -> Result<(), ApiClientError> {
    let key = "a2V5";
    block_on(async {
        match call {
            "authStatus" => client.auth_status().await.map(drop),
            "me" => client.me().await.map(drop),
            "deviceCode" => client.device_code(s(&args[0])).await.map(drop),
            "deviceWait" => client.device_wait(s(&args[0]), None).await.map(drop),
            "logout" => client.logout().await,
            "signOutThisDevice" => client.sign_out_this_device().await,
            "lastFmStatus" => client.last_fm_status().await.map(drop),
            "lastFmConnect" => client.last_fm_connect().await.map(drop),
            "lastFmComplete" => client.last_fm_complete().await.map(drop),
            "lastFmDisconnect" => client.last_fm_disconnect().await.map(drop),
            "lastFmNowPlaying" => client.last_fm_now_playing(&serde_json::from_value::<LastFmNowPlayingRequestDto>(args[0].clone()).unwrap()).await,
            "lastFmScrobble" => client.last_fm_scrobble(serde_json::from_value(args[0].clone()).unwrap()).await,
            "listTracks" => client.list_tracks(&serde_json::from_value::<ListTracksQuery>(args[0].clone()).unwrap()).await.map(drop),
            "syncChanges" => client.sync_changes(s(&args[0]), args[1].as_u64().map(|n| n as u32)).await.map(drop),
            "syncSnapshot" => client.sync_snapshot(args[0].as_str(), args[1].as_u64().map(|n| n as u32)).await.map(drop),
            "mutations" => client.mutations(serde_json::from_value::<Vec<MutationDto>>(args[0].clone()).unwrap()).await.map(drop),
            "lyrics" => client.lyrics(s(&args[0]), None).await.map(drop),
            "reportLyrics" => client.report_lyrics(s(&args[0]), args[1].as_str()).await,
            "lyricsReports" => client.lyrics_reports().await.map(drop),
            "resolveLyricsReport" => client.resolve_lyrics_report(s(&args[0])).await,
            "lyricsSource" => client.lyrics_source(s(&args[0])).await.map(drop),
            "assignLyrics" => client.assign_lyrics(s(&args[0]), args[1].as_str()).await,
            "saveLyrics" => client.save_lyrics(s(&args[0]), serde_json::from_value::<Vec<LyricsSaveLineDto>>(args[1].clone()).unwrap()).await,
            "fetchBytes" => {
                let range = args[1].as_array().map(|r| (r[0].as_u64().unwrap(), r[1].as_u64().unwrap()));
                client.fetch_bytes(s(&args[0]), range, None).await.map(drop)
            }
            "fetchFile" => client.fetch_file(s(&args[0]), None).await.map(drop),
            "fetchText" => client.fetch_text(s(&args[0]), None).await.map(drop),
            other => panic!("unmapped call {other} (key {key})"),
        }
    })
}

#[test]
fn every_request_matches_the_typescript_client() {
    let golden = golden();
    let cases = golden["requests"].as_array().unwrap();
    assert_eq!(cases.len(), 76);
    for case in cases {
        let (call, args) = (s(&case["call"]), &case["args"]);
        let with_token = case["withToken"].as_bool().unwrap();
        let recorder = Recorder { sent: RefCell::new(Vec::new()), answer: canned(call) };
        let mut options = ApiClientOptions::new("https://music.example.com/");
        if with_token {
            options = options.with_token(|| Some("tok-1".to_string()));
        }
        let client = ApiClient::new(options, &recorder);
        let result = run(&client, call, args);
        assert!(result.is_ok(), "{call} {args}: {result:?}");

        let sent = recorder.sent.borrow();
        let want = case["sent"].as_array().unwrap();
        assert_eq!(sent.len(), want.len(), "{call} {args}");
        for (got, want) in sent.iter().zip(want) {
            let ctx = format!("{call} {args} (token: {with_token})");
            assert_eq!(got.method.as_str(), s(&want["method"]), "{ctx}");
            assert_eq!(got.url, s(&want["url"]), "{ctx}");
            let headers: Vec<(String, String)> = want["headers"].as_array().unwrap().iter().map(|h| (s(&h[0]).to_owned(), s(&h[1]).to_owned())).collect();
            assert_eq!(got.headers, headers, "{ctx}");
            match (&got.body, want["body"].as_str()) {
                (None, None) => {}
                (Some(got), Some(want)) => {
                    let (got, want): (Value, Value) = (serde_json::from_str(got).unwrap(), serde_json::from_str(want).unwrap());
                    // The now-playing request's album/duration are nullable-and-optional on the server: the TypeScript sends an explicit
                    // null where the Rust DTO leaves the key out. Same meaning, so compare without those nulls.
                    let strip = |mut v: Value| {
                        if call == "lastFmNowPlaying" {
                            for key in ["album", "durationSeconds"] {
                                if v.get(key).is_some_and(Value::is_null) {
                                    v.as_object_mut().unwrap().remove(key);
                                }
                            }
                        }
                        normalize(v)
                    };
                    assert_eq!(strip(got), strip(want), "{ctx}: body");
                }
                (got, want) => panic!("{ctx}: body {got:?} vs {want:?}"),
            }
        }
    }
}

#[test]
fn the_path_helpers_match_the_typescript() {
    let golden = golden();
    let h = &golden["pathHelpers"];
    let recorder = Recorder { sent: RefCell::new(Vec::new()), answer: canned("") };
    let client = ApiClient::new(ApiClientOptions::new("https://music.example.com"), &recorder);
    assert_eq!(client.stream_path("KEY"), s(&h["streamPath"]));
    assert_eq!(client.cover_path("KEY"), s(&h["coverPath"]));
    assert_eq!(client.stream_path_for_file_id("Music:Album/é.mp3"), s(&h["streamPathForFileId"]));
    let trailing = ApiClient::new(ApiClientOptions::new("https://music.example.com//"), &recorder);
    assert_eq!(trailing.url("/x"), s(&h["url"]));
}

#[test]
fn error_handling_matches_the_typescript() {
    let golden = golden();
    let cases = golden["errorCases"].as_array().unwrap();
    assert_eq!(cases.len(), 18);
    for case in cases {
        let (label, call) = (s(&case["label"]), s(&case["call"]));
        struct Canned(Option<HttpResponse>);
        impl HttpTransport for &Canned {
            async fn send(&self, _r: HttpRequest) -> Result<HttpResponse, TransportError> {
                self.0.clone().ok_or_else(|| TransportError("fetch failed".into()))
            }
        }
        let spec = &case["spec"];
        let transport = Canned((!spec.is_string()).then(|| {
            let mut r = HttpResponse::new(spec["status"].as_u64().unwrap() as u16, s(&spec["body"]));
            r.status_text = spec["statusText"].as_str().unwrap_or("").to_owned();
            r.headers.push(("content-type".into(), "application/json".into()));
            for (k, v) in spec["headers"].as_object().into_iter().flatten() {
                r.headers.push((k.clone(), s(v).to_owned()));
            }
            r
        }));
        let client = ApiClient::new(ApiClientOptions::new("https://music.example.com"), &transport);
        let got: Result<Option<()>, ApiClientError> = block_on(async {
            match call {
                "me" => client.me().await.map(|_| Some(())),
                "lyrics" => client.lyrics("k", None).await.map(|l| l.map(drop)),
                "fetchBytes" => client.fetch_bytes("/x", None, None).await.map(|_| Some(())),
                other => panic!("{other}"),
            }
        });

        let kind = s(&case["kind"]);
        match (&got, kind) {
            (Ok(Some(())), "ok") => assert_eq!(case["value"], "value", "{label}"),
            (Ok(None), "ok") => assert!(case["value"].is_null(), "{label}"),
            (Err(ApiClientError::Api(e)), "ApiError") => {
                assert_eq!(Some(e.status as u64), case["status"].as_u64(), "{label}");
                assert_eq!(e.code.as_deref(), case["code"].as_str(), "{label}");
                assert_eq!(e.message, s(&case["message"]), "{label}");
            }
            (Err(ApiClientError::ResyncRequired(m)), "ResyncRequiredError") => assert_eq!(m, s(&case["message"]), "{label}"),
            (Err(ApiClientError::Version { message, problem }), "ApiVersionError") => {
                assert_eq!(message, s(&case["message"]), "{label}");
                let want = match s(&case["problem"]) {
                    "app_too_old" => beatseam_api::CompatibilityProblem::AppTooOld,
                    _ => beatseam_api::CompatibilityProblem::ServerTooOld,
                };
                assert_eq!(*problem, want, "{label}");
            }
            (Err(ApiClientError::Network(_)), "NetworkError") => {}
            (Err(ApiClientError::Decode(_)), "ZodError" | "SyntaxError" | "$ZodError") => {}
            _ => panic!("{label}: TS gave {kind}, Rust gave {got:?}"),
        }
    }
}

#[test]
fn a_method_enum_round_trips_its_name() {
    assert_eq!([Method::Get, Method::Post, Method::Put, Method::Delete].map(Method::as_str), ["GET", "POST", "PUT", "DELETE"]);
}
