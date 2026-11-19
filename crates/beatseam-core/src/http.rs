//! A minimal HTTP seam. `beatseam-core` never talks to the network itself: callers hand it an [`HttpTransport`] (reqwest
//! on desktop and the server, `fetch` through `web-sys` on web, a Kotlin/OkHttp bridge on Android, a fake in tests).
//!
//! The trait uses `async fn` directly and has no `Send` bound, so it runs on any executor - including the
//! single-threaded wasm one - without this crate depending on a runtime. A transport should cancel the request when the
//! future it returned is dropped.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    /// Give up (fail with a [`TransportError`]) if no answer arrives within this long; `None` leaves it to the
    /// transport's own default.
    pub timeout_ms: Option<u64>,
}

impl HttpRequest {
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Self { method, url: url.into(), headers: Vec::new(), body: None, timeout_ms: None }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    /// The reason phrase (`fetch`'s `statusText`), possibly empty (HTTP/2 has none).
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self { status, status_text: String::new(), headers: Vec::new(), body: body.into() }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// `response.ok` in `fetch`: a 2xx status.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// A header value by case-insensitive name (the first one, if repeated).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
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

#[cfg(test)]
pub(crate) mod test_support {
    use std::cell::RefCell;

    use super::{HttpRequest, HttpResponse, HttpTransport, TransportError};

    /// Records every request and answers with a canned response (or a transport failure).
    pub struct FakeTransport {
        pub requests: RefCell<Vec<HttpRequest>>,
        pub response: Result<HttpResponse, TransportError>,
    }

    impl FakeTransport {
        pub fn json(status: u16, body: &str) -> Self {
            Self { requests: RefCell::new(Vec::new()), response: Ok(HttpResponse::new(status, body)) }
        }

        pub fn offline() -> Self {
            Self { requests: RefCell::new(Vec::new()), response: Err(TransportError("offline".into())) }
        }
    }

    impl HttpTransport for FakeTransport {
        async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            self.requests.borrow_mut().push(request);
            self.response.clone()
        }
    }

    impl HttpTransport for &FakeTransport {
        async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            (**self).send(request).await
        }
    }
}
