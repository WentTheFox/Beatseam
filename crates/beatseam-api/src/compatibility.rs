//! API version negotiation (`packages/api/src/compatibility.ts`). See `docs/api-compatibility.md`.

use beatseam_core::js_compat::js_trim;

use crate::schemas::{API_VERSION, API_VERSIONS_HEADER};

/// What a server that doesn't advertise its versions speaks: version 1 was all there ever was before the header existed.
const IMPLICIT_SERVER_VERSIONS: [u32; 1] = [1];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompatibilityProblem {
    /// The server has moved past this app, so the app needs updating.
    AppTooOld,
    /// The app is newer than the server, so the server needs updating.
    ServerTooOld,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiCompatibility {
    Compatible,
    Incompatible { problem: CompatibilityProblem, message: String },
}

/// Reads the versions a server advertised in its response headers; none advertised (or nothing usable in it) means an
/// older server that speaks only version 1.
pub fn parse_server_api_versions(header: Option<&str>) -> Vec<u32> {
    let versions: Vec<u32> = header
        .filter(|h| !js_trim(h).is_empty())
        .into_iter()
        .flat_map(|h| h.split(','))
        // `Number(part.trim())` accepts a decimal integer; anything else is dropped.
        .filter_map(|part| js_trim(part).parse::<u32>().ok())
        .filter(|n| *n > 0)
        .collect();
    if versions.is_empty() { IMPLICIT_SERVER_VERSIONS.to_vec() } else { versions }
}

/// Whether a client speaking `client_version` can talk to a server that speaks `server_versions`, and if not, which side
/// has to update.
pub fn check_api_compatibility(client_version: u32, server_versions: &[u32]) -> ApiCompatibility {
    if server_versions.contains(&client_version) {
        return ApiCompatibility::Compatible;
    }
    let newest = server_versions.iter().copied().max().unwrap_or(0);
    let described = describe_versions(server_versions);
    if client_version < newest {
        ApiCompatibility::Incompatible {
            problem: CompatibilityProblem::AppTooOld,
            message: format!("This server needs a newer version of the app (the app speaks API version {client_version}; the server speaks {described}). Update the app to keep using it."),
        }
    } else {
        ApiCompatibility::Incompatible {
            problem: CompatibilityProblem::ServerTooOld,
            message: format!("This server is older than the app (the app speaks API version {client_version}; the server speaks {described}). Update the server, or use an older version of the app."),
        }
    }
}

/// Checks the versions a response advertises against this build's own.
pub fn check_response_compatibility(headers: &[(String, String)]) -> ApiCompatibility {
    let advertised = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(API_VERSIONS_HEADER)).map(|(_, v)| v.as_str());
    check_api_compatibility(API_VERSION, &parse_server_api_versions(advertised))
}

fn describe_versions(versions: &[u32]) -> String {
    if let [only] = versions {
        return only.to_string();
    }
    let mut sorted = versions.to_vec();
    sorted.sort_unstable();
    sorted.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_comma_separated_list() {
        assert_eq!(parse_server_api_versions(Some("1")), [1]);
        assert_eq!(parse_server_api_versions(Some("1, 2")), [1, 2]);
    }

    #[test]
    fn treats_a_missing_or_unreadable_header_as_the_original_version_1() {
        for h in [None, Some(""), Some("  "), Some("banana"), Some("0"), Some("-1,0"), Some("1.5")] {
            assert_eq!(parse_server_api_versions(h), [1], "{h:?}");
        }
        // The usable parts of a partly garbled list are kept.
        assert_eq!(parse_server_api_versions(Some("x, 2, 1.5, 3")), [2, 3]);
    }

    #[test]
    fn accepts_a_server_that_speaks_the_apps_version_even_alongside_others() {
        assert_eq!(check_api_compatibility(1, &[1]), ApiCompatibility::Compatible);
        assert_eq!(check_api_compatibility(1, &[1, 2]), ApiCompatibility::Compatible);
        assert_eq!(check_api_compatibility(2, &[1, 2]), ApiCompatibility::Compatible);
    }

    #[test]
    fn says_the_app_must_update_when_the_server_has_moved_past_it() {
        let ApiCompatibility::Incompatible { problem, message } = check_api_compatibility(1, &[2]) else { panic!("compatible") };
        assert_eq!(problem, CompatibilityProblem::AppTooOld);
        assert!(message.contains("newer version of the app"), "{message}");
    }

    #[test]
    fn says_the_server_must_update_when_the_app_is_newer_than_it() {
        let ApiCompatibility::Incompatible { problem, message } = check_api_compatibility(3, &[2, 1]) else { panic!("compatible") };
        assert_eq!(problem, CompatibilityProblem::ServerTooOld);
        assert!(message.contains("Update the server") && message.contains("the server speaks 1, 2)"), "{message}");
    }

    #[test]
    fn the_response_check_reads_the_header_case_insensitively() {
        let h = |name: &str, v: &str| vec![(name.to_string(), v.to_string())];
        assert_eq!(check_response_compatibility(&[]), ApiCompatibility::Compatible); // an old server: version 1
        assert_eq!(check_response_compatibility(&h("x-beatseam-api-versions", "1")), ApiCompatibility::Compatible);
        assert!(matches!(check_response_compatibility(&h("X-BEATSEAM-API-VERSIONS", "2")), ApiCompatibility::Incompatible { problem: CompatibilityProblem::AppTooOld, .. }));
    }
}
