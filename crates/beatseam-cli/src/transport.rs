//! The CLI's [`HttpTransport`]: `ureq` (blocking, rustls). The CLI's executor is single-threaded and has nothing else
//! to do while a request is in flight, so blocking inside the future is fine here; a GUI host would use a real async client.

use std::time::Duration;

use beatseam_core::http::{HttpRequest, HttpResponse, HttpTransport, Method, TransportError};

#[derive(Clone)]
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    pub fn new() -> Self {
        // A 4xx/5xx is an answer the API client interprets, not a transport failure.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        Self { agent }
    }
}

fn failure(e: impl std::fmt::Display) -> TransportError {
    TransportError(e.to_string())
}

impl HttpTransport for UreqTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let timeout = request.timeout_ms.map(Duration::from_millis);
        let mut response = match request.method {
            Method::Get | Method::Delete => {
                let mut builder = if request.method == Method::Get {
                    self.agent.get(&request.url)
                } else {
                    self.agent.delete(&request.url)
                };
                for (k, v) in &request.headers {
                    builder = builder.header(k, v);
                }
                builder = builder.config().timeout_global(timeout).build();
                builder.call().map_err(failure)?
            }
            Method::Post | Method::Put => {
                let mut builder = if request.method == Method::Post {
                    self.agent.post(&request.url)
                } else {
                    self.agent.put(&request.url)
                };
                for (k, v) in &request.headers {
                    builder = builder.header(k, v);
                }
                builder = builder.config().timeout_global(timeout).build();
                builder
                    .send(request.body.unwrap_or_default())
                    .map_err(failure)?
            }
        };
        let status = response.status();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
            .collect();
        let body = response.body_mut().read_to_vec().map_err(failure)?;
        Ok(HttpResponse {
            status: status.as_u16(),
            status_text: status.canonical_reason().unwrap_or_default().to_owned(),
            headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use beatseam_core::http::block_on;

    use super::*;

    /// Serves one canned response and hands back the raw request it received.
    fn serve_once(response: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Headers and body may arrive in separate segments: read until the request is complete.
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            while !raw.windows(4).any(|w| w == b"\r\n\r\n")
                || (raw.starts_with(b"POST") && !raw.ends_with(b"}"))
            {
                let n = stream.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
            }
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8_lossy(&raw).into_owned()
        });
        (url, handle)
    }

    #[test]
    fn returns_error_statuses_as_responses() {
        let (url, server) = serve_once(
            "HTTP/1.1 401 Unauthorized\r\ncontent-length: 2\r\nx-a: b\r\nconnection: close\r\n\r\nno",
        );
        let response =
            block_on(UreqTransport::new().send(HttpRequest::new(Method::Get, format!("{url}/x"))))
                .unwrap();
        assert_eq!(
            (response.status, response.body.as_slice()),
            (401, &b"no"[..])
        );
        assert_eq!(response.header("X-A"), Some("b"));
        assert!(server.join().unwrap().starts_with("GET /x "));
    }

    #[test]
    fn sends_headers_and_body() {
        let (url, server) =
            serve_once("HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        let mut request = HttpRequest::new(Method::Post, format!("{url}/p"));
        request
            .headers
            .push(("Authorization".into(), "Bearer t".into()));
        request.body = Some("{\"a\":1}".into());
        assert!(block_on(UreqTransport::new().send(request)).unwrap().ok());
        let raw = server.join().unwrap();
        assert!(raw.starts_with("POST /p "));
        assert!(raw.to_lowercase().contains("authorization: bearer t"));
        assert!(raw.ends_with("{\"a\":1}"));
    }

    #[test]
    fn a_refused_connection_is_a_transport_error() {
        let url = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        assert!(block_on(UreqTransport::new().send(HttpRequest::new(Method::Get, url))).is_err());
    }
}
