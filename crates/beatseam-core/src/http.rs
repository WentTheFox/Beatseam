//! A minimal HTTP seam. `beatseam-core` never talks to the network itself: callers hand it an [`HttpTransport`] (reqwest
//! on desktop and the server, `fetch` through `web-sys` on web, a Kotlin/OkHttp bridge on Android, a fake in tests).
//!
//! The trait uses `async fn` directly and has no `Send` bound, so it runs on any executor - including the
//! single-threaded wasm one - without this crate depending on a runtime.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// `response.ok` in `fetch`: a 2xx status.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The request never produced a response (offline, DNS failure, TLS error, ...). A retry later could work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportError(pub String);

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TransportError {}

pub trait HttpTransport {
    fn send(&self, request: HttpRequest) -> impl Future<Output = Result<HttpResponse, TransportError>>;
}

/// `application/x-www-form-urlencoded` serialization, as `URLSearchParams#toString` does it: everything except ASCII
/// letters/digits and `*-._` is percent-encoded (uppercase hex), and a space becomes `+`. Pairs keep their order.
pub fn form_urlencode<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)]) -> String {
    fn push_encoded(out: &mut String, s: &str) {
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => out.push(b as char),
                b' ' => out.push('+'),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
    }
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        push_encoded(&mut out, k.as_ref());
        out.push('=');
        push_encoded(&mut out, v.as_ref());
    }
    out
}

/// JavaScript's `encodeURIComponent`: everything except ASCII letters/digits and `-_.!~*'()` is percent-encoded
/// (uppercase hex, UTF-8 bytes).
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Drives a future to completion on the current thread with a no-op waker. For tests and for code that knows its
/// future never really waits (a fake transport); real callers use their executor.
pub fn block_on<F: Future>(future: F) -> F::Output {
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        std::thread::yield_now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_urlencode_follows_the_whatwg_rules() {
        assert_eq!(form_urlencode(&[("a b", "c&d=e"), ("k", "~!'()*")]), "a+b=c%26d%3De&k=%7E%21%27%28%29*");
        assert_eq!(form_urlencode(&[("artist[0]", "Ünï ☃")]), "artist%5B0%5D=%C3%9Cn%C3%AF+%E2%98%83");
        assert_eq!(form_urlencode::<&str, &str>(&[]), "");
    }

    #[test]
    fn encode_uri_component_follows_ecmascript() {
        assert_eq!(encode_uri_component("a b&c=d/é~!'()*-_."), "a%20b%26c%3Dd%2F%C3%A9~!'()*-_.");
    }

    #[test]
    fn block_on_runs_an_immediately_ready_future() {
        assert_eq!(block_on(async { 1 + 1 }), 2);
    }
}
