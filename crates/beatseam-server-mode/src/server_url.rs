use url::Url;

/// Normalises what a person typed into an address: adds a scheme if missing, drops trailing slashes and any path.
/// Errors are the messages shown to the person.
pub fn normalize_server_url(input: &str) -> Result<String, String> {
    let trimmed = beatseam_core::js_compat::js_trim(input);
    if trimmed.is_empty() {
        return Err("Enter the server address".into());
    }
    // No scheme typed: https, except for a server on the local network, where plain http is what people run.
    let with_scheme = if has_scheme(trimmed) { trimmed.to_owned() } else { format!("{}://{trimmed}", if host_is_private(trimmed) { "http" } else { "https" }) };
    let url = Url::parse(&with_scheme).map_err(|_| "That does not look like an address".to_owned())?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("The address must start with http:// or https://".into());
    }
    if url.scheme() == "http" && !url.host_str().is_some_and(is_private_host) {
        return Err("Plain http:// is only allowed for servers on your own network (a local address); use https:// for anything on the internet".into());
    }
    Ok(url.origin().ascii_serialization())
}

/// `/^[a-z][a-z0-9+.-]*:\/\//i`
fn has_scheme(s: &str) -> bool {
    let mut chars = s.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    for c in chars {
        if c == ':' {
            return s[s.find(':').unwrap()..].starts_with("://");
        }
        if !(c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')) {
            return false;
        }
    }
    false
}

fn host_is_private(typed: &str) -> bool {
    // The real parse reports what is wrong with an unparseable address.
    Url::parse(&format!("http://{typed}")).ok().and_then(|u| u.host_str().map(is_private_host)).unwrap_or(false)
}

/// Whether a host name is on a private network or the device itself, where unencrypted http is an acceptable trade for not
/// having to set up a certificate: loopback, RFC 1918, link-local, CGNAT (Tailscale), IPv6 local ranges, single-label names
/// and the usual home-network suffixes. Anything else could be crossing the internet, where the device token would be
/// readable by anyone on the path.
pub fn is_private_host(hostname: &str) -> bool {
    let lower = hostname.to_lowercase();
    let host = lower.strip_prefix('[').unwrap_or(&lower);
    let host = host.strip_suffix(']').unwrap_or(host);
    if let Some([a, b, _, _]) = parse_dotted_quad(host) {
        return a == 10 || a == 127 || (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168) || (a == 169 && b == 254) || (a == 100 && (64..=127).contains(&b));
    }
    if host.contains(':') {
        let hex = |s: &str, n: usize| s.len() >= n && s.as_bytes()[..n].iter().all(u8::is_ascii_hexdigit);
        return host == "::1" || (host.len() > 4 && matches!(&host[..2], "fc" | "fd") && hex(&host[2..], 2) && host.as_bytes()[4] == b':') || (host.len() > 4 && &host[..2] == "fe" && matches!(host.as_bytes()[2], b'8' | b'9' | b'a' | b'b') && host.as_bytes()[3].is_ascii_hexdigit() && host.as_bytes()[4] == b':');
    }
    !host.contains('.') || [".local", ".lan", ".home.arpa", ".internal", ".localhost"].iter().any(|suffix| host.ends_with(suffix))
}

/// `/^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/`, each part a number (values above 255 are not rejected, as in the
/// TypeScript).
fn parse_dotted_quad(host: &str) -> Option<[u32; 4]> {
    let mut parts = host.split('.');
    let mut out = [0u32; 4];
    for slot in &mut out {
        let part = parts.next()?;
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_public_name_defaults_to_https_and_a_local_one_to_http() {
        assert_eq!(normalize_server_url("music.example.com").unwrap(), "https://music.example.com");
        assert_eq!(normalize_server_url("  music.example.com/  ").unwrap(), "https://music.example.com");
        assert_eq!(normalize_server_url("192.168.1.20:8080").unwrap(), "http://192.168.1.20:8080");
        assert_eq!(normalize_server_url("homeserver").unwrap(), "http://homeserver");
        assert_eq!(normalize_server_url("nas.local").unwrap(), "http://nas.local");
        assert_eq!(normalize_server_url("100.101.102.103").unwrap(), "http://100.101.102.103");
    }

    #[test]
    fn paths_default_ports_and_case_are_dropped() {
        assert_eq!(normalize_server_url("https://Music.Example.com:443/some/path?x=1#y").unwrap(), "https://music.example.com");
        assert_eq!(normalize_server_url("https://music.example.com:8443/").unwrap(), "https://music.example.com:8443");
        assert_eq!(normalize_server_url("http://localhost:3000/app").unwrap(), "http://localhost:3000");
    }

    #[test]
    fn bad_input_gets_the_messages_the_person_sees() {
        assert_eq!(normalize_server_url("   ").unwrap_err(), "Enter the server address");
        assert_eq!(normalize_server_url("ftp://example.com").unwrap_err(), "The address must start with http:// or https://");
        assert!(normalize_server_url("http://example.com").unwrap_err().starts_with("Plain http:// is only allowed for servers on your own network"));
        assert_eq!(normalize_server_url("https://").unwrap_err(), "That does not look like an address");
        assert_eq!(normalize_server_url("http://[::1]:8080").unwrap(), "http://[::1]:8080");
    }

    #[test]
    fn private_hosts_are_exactly_the_ranges_the_typescript_lists() {
        for host in ["10.0.0.1", "127.0.0.1", "172.16.0.1", "172.31.255.255", "192.168.0.5", "169.254.1.1", "100.64.0.1", "100.127.255.255", "::1", "[::1]", "fc00::1", "fd12:3456::1", "fe80::1", "febf::1", "printer", "x.local", "x.lan", "x.home.arpa", "x.internal", "x.localhost", "LOCALHOST", "NAS.LOCAL"] {
            assert!(is_private_host(host), "{host}");
        }
        for host in ["172.15.0.1", "172.32.0.1", "192.169.0.1", "100.63.0.1", "100.128.0.1", "8.8.8.8", "example.com", "2001:db8::1", "fec0::1", "fe7f::1", "10.0.0", "10.0.0.1.1"] {
            assert!(!is_private_host(host), "{host}");
        }
    }

    #[test]
    fn scheme_detection_matches_the_regex() {
        assert!(has_scheme("https://x") && has_scheme("a+b.c-d://x") && has_scheme("HTTP://x"));
        assert!(!has_scheme("x.com:8080") && !has_scheme("1http://x") && !has_scheme("localhost") && !has_scheme("://x"));
    }
}
