//! Differential test: version comparison and checksum parsing against the TypeScript
//! (`tools/gen-appupdate-golden.mjs` -> `tests/data/appupdate_golden.json`).

use std::cell::RefCell;

use beatseam_core::app_update::{check_for_update, parse_checksums_file};
use beatseam_core::http::{HttpRequest, HttpResponse, HttpTransport, TransportError, block_on};
use serde_json::{Value, json};

struct Canned(RefCell<String>);

impl HttpTransport for &Canned {
    async fn send(&self, _request: HttpRequest) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse { status: 200, body: self.0.borrow().clone().into_bytes() })
    }
}

fn golden() -> Value {
    serde_json::from_str(include_str!("data/appupdate_golden.json")).unwrap()
}

#[test]
fn update_detection_matches_the_typescript() {
    let golden = golden();
    let cases = golden["updateCases"].as_array().unwrap();
    assert!(cases.len() > 100);
    let transport = Canned(RefCell::new(String::new()));
    let mut newer = 0;
    for c in cases {
        let (tag, current) = (c["tag"].as_str().unwrap(), c["current"].as_str().unwrap());
        *transport.0.borrow_mut() = json!({"tag_name": tag, "html_url": "https://example.test/r", "assets": [{"name": "app.apk", "browser_download_url": "https://example.test/app.apk"}]}).to_string();
        let got = block_on(check_for_update(&&transport, current)).expect("a well-formed release never errors").is_some();
        assert_eq!(c["result"], json!(got), "latest {tag:?} vs current {current:?}");
        newer += usize::from(got);
    }
    assert!(newer > 10, "only {newer} 'newer' cases - the comparison is barely exercised");
}

#[test]
fn checksum_parsing_matches_the_typescript() {
    let golden = golden();
    let cases = golden["checksumCases"].as_array().unwrap();
    assert!(cases.len() > 300);
    let mut found = 0;
    for c in cases {
        let (text, name) = (c["text"].as_str().unwrap(), c["name"].as_str().unwrap());
        let got = parse_checksums_file(text, name);
        assert_eq!(c["result"].as_str().map(str::to_owned), got, "text {text:?}, name {name:?}");
        found += usize::from(got.is_some());
    }
    assert!(found >= 15, "only {found} lookups found a hash");
}
