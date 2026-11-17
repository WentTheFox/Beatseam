use std::fmt;

use serde_json::Value;

use crate::http::{HttpRequest, HttpTransport, Method, TransportError};
use crate::js_compat::js_trim;

/// "owner/repo" - shared between the About links and the update checker, rather than a second copy drifting out of
/// sync with the first.
pub const GITHUB_REPO: &str = "WentTheFox/Beatseam";
pub const GITHUB_REPO_URL: &str = "https://github.com/WentTheFox/Beatseam";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableUpdate {
    /// The release's git tag, e.g. "v0.2.0".
    pub version: String,
    pub html_url: String,
    pub apk_asset: ReleaseAsset,
    /// `checksums.txt`, if the release has one (the release workflow always attaches one). Verified against the
    /// downloaded APK's own computed hash before installing - `None` only for a release published some other way.
    pub checksums_asset: Option<ReleaseAsset>,
}

/// Why [`check_for_update`] could not answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateCheckError {
    Transport(TransportError),
    /// GitHub answered with a non-2xx status.
    Http(u16),
    /// A 2xx answer that wasn't the release JSON this expects.
    BadResponse,
}

impl fmt::Display for UpdateCheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(e) => e.fmt(f),
            Self::Http(status) => write!(f, "GitHub releases check failed: {status}"),
            Self::BadResponse => f.write_str("GitHub releases check returned an unexpected response"),
        }
    }
}

impl std::error::Error for UpdateCheckError {}

/// `[major, minor, patch]`, or `None` for anything that isn't a clean "v1.2.3" (or "1.2.3") - notably a dev build's
/// `git describe` output like "v0.1.0-2-gabc123", which this deliberately can't parse as a version to compare against.
/// Components are compared as JavaScript numbers (f64), like the TypeScript.
fn parse_semver(version: &str) -> Option<[f64; 3]> {
    let v = js_trim(version);
    let v = v.strip_prefix('v').unwrap_or(v);
    let mut parts = v.split('.');
    let mut out = [0.0; 3];
    for slot in &mut out {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

fn is_newer(candidate: &str, current: &str) -> bool {
    let (Some(c), Some(b)) = (parse_semver(candidate), parse_semver(current)) else { return false };
    for i in 0..3 {
        if c[i] != b[i] {
            return c[i] > b[i];
        }
    }
    false
}

fn asset(value: &Value) -> Option<ReleaseAsset> {
    Some(ReleaseAsset { name: value.get("name")?.as_str()?.to_owned(), browser_download_url: value.get("browser_download_url")?.as_str()?.to_owned() })
}

/// Checks GitHub's Releases API for a version newer than `current_version` (a git-describe string: "v1.2.3" exactly at
/// a tagged release build, or "v1.2.3-N-gHASH" for a dev build N commits past the last tag). A dev build's version
/// string can't be cleanly parsed as a plain semver, so it's treated as "nothing to compare against" (never reports an
/// update) rather than guessing - this check is only meaningful for an actual tagged release build in practice (the
/// Android release APK), which always has a clean "vX.Y.Z" version.
///
/// `Ok(None)` when already up to date, when there's no newer release, or when the latest release has no APK asset
/// attached (shouldn't happen for a release the workflow created, but a manually-published release might not have one).
///
/// Sends a `User-Agent` header, which GitHub's API requires and `fetch` adds on its own in the TypeScript. Asset entries
/// without a name and download URL are skipped, where the TypeScript would have thrown on them.
pub async fn check_for_update<T: HttpTransport>(transport: &T, current_version: &str) -> Result<Option<AvailableUpdate>, UpdateCheckError> {
    let request = HttpRequest {
        method: Method::Get,
        url: format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest"),
        headers: vec![("User-Agent".into(), "Beatseam".into())],
        body: None,
        timeout_ms: None,
    };
    let response = transport.send(request).await.map_err(UpdateCheckError::Transport)?;
    if !response.ok() {
        return Err(UpdateCheckError::Http(response.status));
    }
    let data: Value = serde_json::from_slice(&response.body).map_err(|_| UpdateCheckError::BadResponse)?;
    let tag_name = data.get("tag_name").and_then(Value::as_str).ok_or(UpdateCheckError::BadResponse)?;
    if !is_newer(tag_name, current_version) {
        return Ok(None);
    }
    let assets: Vec<ReleaseAsset> = data.get("assets").and_then(Value::as_array).map(|a| a.iter().filter_map(asset).collect()).ok_or(UpdateCheckError::BadResponse)?;
    let Some(apk_asset) = assets.iter().find(|a| a.name.ends_with(".apk")).cloned() else { return Ok(None) };
    Ok(Some(AvailableUpdate {
        version: tag_name.to_owned(),
        html_url: data.get("html_url").and_then(Value::as_str).ok_or(UpdateCheckError::BadResponse)?.to_owned(),
        apk_asset,
        checksums_asset: assets.iter().find(|a| a.name == "checksums.txt").cloned(),
    }))
}

/// Parses `sha256sum`-style output ("<hex>  <filename>" per line) and returns the lowercase hash for one specific file
/// name, or `None` if that name isn't listed. Handles the binary-mode `*filename` marker `sha256sum` sometimes writes.
pub fn parse_checksums_file(text: &str, file_name: &str) -> Option<String> {
    // JS splits on /\r?\n/ (a lone CR does not split), and `.` in the pattern does not match line terminators.
    for raw in text.split('\n') {
        let line = js_trim(raw.strip_suffix('\r').unwrap_or(raw));
        if line.len() < 65 || !line.is_char_boundary(64) {
            continue;
        }
        let (hash, rest) = line.split_at(64);
        if !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let name = rest.trim_start_matches(crate::js_compat::js_is_whitespace);
        if name.len() == rest.len() {
            continue; // needs at least one whitespace character after the hash
        }
        let name = match name.strip_prefix('*') {
            Some(after) if !after.is_empty() => after,
            _ => name,
        };
        if name.is_empty() || name.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
            continue;
        }
        if name == file_name {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::block_on;
    use crate::http::test_support::FakeTransport;

    fn release(tag: &str, assets: &[(&str, &str)]) -> String {
        let assets: Vec<String> = assets.iter().map(|(n, u)| format!(r#"{{"name":"{n}","browser_download_url":"{u}"}}"#)).collect();
        format!(r#"{{"tag_name":"{tag}","html_url":"https://github.com/WentTheFox/Beatseam/releases/tag/{tag}","assets":[{}]}}"#, assets.join(","))
    }

    const APK: (&str, &str) = ("app-release.apk", "https://example.com/app-release.apk");
    const SUMS: (&str, &str) = ("checksums.txt", "https://example.com/checksums.txt");

    fn check(body: &str, current: &str) -> Result<Option<AvailableUpdate>, UpdateCheckError> {
        block_on(check_for_update(&FakeTransport::json(200, body), current))
    }

    #[test]
    fn reports_an_update_when_the_latest_release_is_a_newer_semver_than_the_current_version() {
        let got = check(&release("v0.2.0", &[APK, SUMS]), "v0.1.0").unwrap();
        assert_eq!(
            got,
            Some(AvailableUpdate {
                version: "v0.2.0".into(),
                html_url: "https://github.com/WentTheFox/Beatseam/releases/tag/v0.2.0".into(),
                apk_asset: ReleaseAsset { name: "app-release.apk".into(), browser_download_url: "https://example.com/app-release.apk".into() },
                checksums_asset: Some(ReleaseAsset { name: "checksums.txt".into(), browser_download_url: "https://example.com/checksums.txt".into() }),
            })
        );
    }

    #[test]
    fn returns_none_when_already_up_to_date_or_the_release_is_older() {
        assert_eq!(check(&release("v0.1.0", &[APK]), "v0.1.0"), Ok(None));
        assert_eq!(check(&release("v0.1.0", &[APK]), "v0.2.0"), Ok(None));
    }

    #[test]
    fn never_reports_an_update_for_a_dev_build_version_it_cannot_parse() {
        assert_eq!(check(&release("v0.2.0", &[APK]), "v0.1.0-3-gabc1234"), Ok(None));
    }

    #[test]
    fn returns_none_when_the_latest_release_has_no_apk_asset() {
        assert_eq!(check(&release("v0.2.0", &[("source.zip", "https://example.com/source.zip")]), "v0.1.0"), Ok(None));
    }

    #[test]
    fn checksums_asset_is_none_when_the_release_has_no_checksums_file() {
        assert_eq!(check(&release("v0.2.0", &[APK]), "v0.1.0").unwrap().unwrap().checksums_asset, None);
    }

    #[test]
    fn fails_when_the_github_request_fails() {
        let err = block_on(check_for_update(&FakeTransport::json(500, "{}"), "v0.1.0")).unwrap_err();
        assert_eq!(err, UpdateCheckError::Http(500));
        assert!(err.to_string().contains("500"));
        assert_eq!(block_on(check_for_update(&FakeTransport::offline(), "v0.1.0")), Err(UpdateCheckError::Transport(TransportError("offline".into()))));
    }

    #[test]
    fn an_unusable_success_response_is_reported_not_swallowed() {
        for body in ["", "not json", "[]", r#"{"html_url":"x","assets":[]}"#, r#"{"tag_name":"v9.0.0","html_url":"x"}"#] {
            let r = check(body, "v0.1.0");
            assert!(matches!(r, Err(UpdateCheckError::BadResponse)), "{body:?}: {r:?}");
        }
        // A newer release whose asset list is empty is simply "nothing to install", not an error (as in the TS).
        assert_eq!(check(r#"{"tag_name":"v9.0.0","assets":[]}"#, "v0.1.0"), Ok(None));
        // ...and an older/equal release needs no asset list or url at all.
        assert_eq!(check(r#"{"tag_name":"v0.0.1"}"#, "v0.1.0"), Ok(None));
    }

    #[test]
    fn the_request_goes_to_the_latest_release_endpoint_with_a_user_agent() {
        let transport = FakeTransport::json(200, &release("v0.0.1", &[]));
        block_on(check_for_update(&&transport, "v0.0.1")).unwrap();
        let req = &transport.requests.borrow()[0];
        assert_eq!(req.url, "https://api.github.com/repos/WentTheFox/Beatseam/releases/latest");
        assert_eq!(req.headers, vec![("User-Agent".to_string(), "Beatseam".to_string())]);
    }

    #[test]
    fn semver_parsing_is_strict_like_the_ts_regex() {
        assert_eq!(parse_semver("v1.2.3"), Some([1.0, 2.0, 3.0]));
        assert_eq!(parse_semver(" 10.20.30 "), Some([10.0, 20.0, 30.0]));
        for bad in ["v1.2", "v1.2.3.4", "v1.2.3-1-gabc", "1.2.x", "vv1.2.3", "", "v1..3", "+1.2.3", "1.2.3 4", "v1.2.\u{0663}"] {
            assert_eq!(parse_semver(bad), None, "{bad:?}");
        }
        assert!(is_newer("v0.10.0", "v0.9.9") && is_newer("1.0.0", "v0.99.99") && !is_newer("v1.0.0", "v1.0.0"));
    }

    // ---- parseChecksumsFile ----

    const H1: &str = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
    const H2: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

    #[test]
    fn finds_the_hash_for_a_matching_file_name() {
        let text = format!("{H1}  app-release.apk\n{H2}  other-file.zip");
        assert_eq!(parse_checksums_file(&text, "app-release.apk"), Some(H1.to_string()));
        assert_eq!(parse_checksums_file(&text, "other-file.zip"), Some(H2.to_string()));
    }

    #[test]
    fn returns_none_for_a_file_not_listed() {
        assert_eq!(parse_checksums_file(&format!("{H1}  app-release.apk"), "missing.apk"), None);
    }

    #[test]
    fn handles_the_binary_mode_star_prefix_crlf_and_uppercase_hashes() {
        assert_eq!(parse_checksums_file(&format!("{H1} *app-release.apk"), "app-release.apk"), Some(H1.to_string()));
        assert_eq!(parse_checksums_file(&format!("{H2}  x\r\n{H1}  app-release.apk\r\n"), "app-release.apk"), Some(H1.to_string()));
        assert_eq!(parse_checksums_file(&format!("{}  a.apk", H1.to_uppercase()), "a.apk"), Some(H1.to_string()));
    }

    #[test]
    fn rejects_lines_that_are_not_a_64_hex_hash_followed_by_whitespace_and_a_name() {
        let short = &H1[..63];
        let too_long = format!("{H1}0 name.apk"); // 65 hex characters: no whitespace after the 64th
        for line in [format!("{short}  a.apk"), too_long, format!("{H1}a.apk"), format!("{H1}  "), format!("{}  a.apk", "g".repeat(64)), String::new()] {
            assert_eq!(parse_checksums_file(&line, "a.apk"), None, "{line:?}");
        }
        // The star alone is a (strange) file name, not a marker.
        assert_eq!(parse_checksums_file(&format!("{H1}  *"), "*"), Some(H1.to_string()));
    }

    #[test]
    fn file_names_with_spaces_and_unicode_whitespace_separators_work() {
        assert_eq!(parse_checksums_file(&format!("{H1}\u{3000}my app.apk"), "my app.apk"), Some(H1.to_string()));
    }
}
