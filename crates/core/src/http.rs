//! The one place this crate talks to the network.
//!
//! Everything that reaches a remote server goes through [`Http`]. The real
//! implementation is [`UreqHttp`]; tests substitute a fake and never open a
//! socket. Only `GET` is modelled, because fetching a skill, checking a tree
//! sha and searching a registry are all reads.
//!
//! Every call blocks. Run them on a background task, the way
//! [`Usage::load`](crate::Usage::load) is run.

use std::io::Read as _;
use std::time::Duration;

use thiserror::Error;

/// How long a request may take to connect, and to finish.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(120);

/// Largest response body this crate will hold in memory.
///
/// A skill repository tarball is a few hundred kilobytes; the largest known
/// public one is a few megabytes. The cap stops a wrong URL from filling
/// memory.
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// The `User-Agent` sent with every request.
///
/// GitHub's API rejects a request without one. skills.sh does not require it.
pub const USER_AGENT: &str = concat!("skillbase/", env!("CARGO_PKG_VERSION"));

/// A request that never reached a server, or a body that could not be read.
///
/// An HTTP status is not an error here: a 404 or a 403 comes back as an
/// [`HttpResponse`], because only the caller knows what a status means.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HttpError {
    /// The request could not be made, or the response could not be read.
    #[error("{url}: {message}")]
    Transport {
        /// The URL that was requested.
        url: String,
        /// What the transport reported.
        message: String,
    },

    /// The response body is larger than [`MAX_BODY_BYTES`].
    #[error("{url}: response body is larger than {limit} bytes")]
    TooLarge {
        /// The URL that was requested.
        url: String,
        /// The cap, [`MAX_BODY_BYTES`].
        limit: usize,
    },
}

impl HttpError {
    /// Builds a [`HttpError::Transport`] from anything that can describe
    /// itself.
    pub fn transport(url: impl Into<String>, message: impl std::fmt::Display) -> Self {
        Self::Transport {
            url: url.into(),
            message: message.to_string(),
        }
    }
}

/// A response, read into memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// The HTTP status code.
    pub status: u16,
    /// Response headers, with lowercase names, in the order they arrived.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// A response with a status and a body and no headers. For tests and for
    /// callers building a canned reply.
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    /// Adds a header. Names are lowercased, so lookups are case-insensitive.
    pub fn with_header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_lowercase(), value.into()));
        self
    }

    /// The first value of a header, matched without regard to case.
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    /// A header parsed as an integer, when it is present and parses.
    pub fn header_int(&self, name: &str) -> Option<i64> {
        self.header(name)?.trim().parse().ok()
    }

    /// True for a 2xx status.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as text, with invalid UTF-8 replaced rather than rejected.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// A blocking HTTP `GET`.
///
/// Implemented by [`UreqHttp`] against the network, and by a fake in this
/// crate's tests. Every remote call in this crate takes an `&impl Http`, so no
/// test needs a socket.
pub trait Http: Send + Sync {
    /// Requests `url`, sending `headers` as given, and reads the whole body.
    ///
    /// Redirects are followed. A non-2xx status is returned as a response, not
    /// an error.
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, HttpError>;
}

impl<T: Http + ?Sized> Http for &T {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, HttpError> {
        (**self).get(url, headers)
    }
}

impl<T: Http + ?Sized> Http for Box<T> {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, HttpError> {
        (**self).get(url, headers)
    }
}

impl<T: Http + ?Sized> Http for std::sync::Arc<T> {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, HttpError> {
        (**self).get(url, headers)
    }
}

/// [`Http`] over `ureq`, with rustls.
///
/// One agent is reused across calls so connections are pooled: checking
/// thirteen repositories reuses one TLS session to `api.github.com` rather than
/// opening thirteen.
#[derive(Debug, Clone)]
pub struct UreqHttp {
    agent: ureq::Agent,
}

impl UreqHttp {
    /// An agent with this crate's timeouts.
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(TOTAL_TIMEOUT))
            .user_agent(USER_AGENT)
            // A non-2xx is a response, not a transport failure, and its headers
            // and body are the most interesting part of it: a 403 carries
            // `x-ratelimit-remaining` and GitHub's own explanation, which is
            // what tells a spent rate limit apart from a real refusal. Left at
            // the default, ureq turns those into a bare status code and the
            // distinction is unrecoverable.
            .http_status_as_error(false)
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl Default for UreqHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl Http for UreqHttp {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, HttpError> {
        let mut request = self.agent.get(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(e) => return Err(HttpError::transport(url, e)),
        };

        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_lowercase(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();

        let mut body = Vec::new();
        let mut reader = response.into_body().into_reader().take(
            // One byte past the cap, so a body exactly at the limit is kept and
            // one byte over is caught.
            MAX_BODY_BYTES as u64 + 1,
        );
        reader
            .read_to_end(&mut body)
            .map_err(|e| HttpError::transport(url, e))?;
        if body.len() > MAX_BODY_BYTES {
            return Err(HttpError::TooLarge {
                url: url.to_string(),
                limit: MAX_BODY_BYTES,
            });
        }

        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A scripted [`Http`] for tests. Answers from a table of canned replies
    //! and records every URL it was asked for, so a test can assert on the
    //! number and order of requests as well as the result.

    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    use super::{Http, HttpError, HttpResponse, MAX_BODY_BYTES};

    /// One recorded request.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Recorded {
        pub url: String,
        pub headers: Vec<(String, String)>,
    }

    /// An [`Http`] that never opens a socket.
    #[derive(Debug, Default)]
    pub(crate) struct FakeHttp {
        replies: Mutex<HashMap<String, HttpResponse>>,
        /// Answers for a URL that has no exact reply, matched by substring.
        patterns: Mutex<Vec<(String, HttpResponse)>>,
        /// URLs answered with [`HttpError::TooLarge`].
        too_large: Mutex<HashSet<String>>,
        requests: Mutex<Vec<Recorded>>,
    }

    impl FakeHttp {
        /// A fake with nothing scripted. Any request returns 404.
        pub(crate) fn new() -> Self {
            Self::default()
        }

        /// Answers `url` with `response`.
        pub(crate) fn reply(&self, url: &str, response: HttpResponse) -> &Self {
            self.replies
                .lock()
                .unwrap()
                .insert(url.to_string(), response);
            self
        }

        /// Answers `url` with a 200 and this JSON text.
        pub(crate) fn json(&self, url: &str, body: &str) -> &Self {
            self.reply(url, HttpResponse::new(200, body.as_bytes().to_vec()))
        }

        /// Refuses `url` with [`HttpError::TooLarge`], the way the real
        /// transport refuses a body past [`MAX_BODY_BYTES`].
        ///
        /// Scripted rather than served: a fake that really produced 64 MB
        /// would cost every test that touches it a 64 MB allocation to prove
        /// nothing the cap check does not already prove in `http`'s own tests.
        pub(crate) fn too_large(&self, url: &str) -> &Self {
            self.too_large.lock().unwrap().insert(url.to_string());
            self
        }

        /// Answers any URL containing `needle` with `response`, when no exact
        /// reply matches.
        pub(crate) fn pattern(&self, needle: &str, response: HttpResponse) -> &Self {
            self.patterns
                .lock()
                .unwrap()
                .push((needle.to_string(), response));
            self
        }

        /// Every URL requested, in order.
        pub(crate) fn urls(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.url.clone())
                .collect()
        }

        /// How many requests were made.
        pub(crate) fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }

        /// Every request, with its headers.
        pub(crate) fn requests(&self) -> Vec<Recorded> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Http for FakeHttp {
        fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, HttpError> {
            self.requests.lock().unwrap().push(Recorded {
                url: url.to_string(),
                headers: headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            });
            if self.too_large.lock().unwrap().contains(url) {
                return Err(HttpError::TooLarge {
                    url: url.to_string(),
                    limit: MAX_BODY_BYTES,
                });
            }
            if let Some(response) = self.replies.lock().unwrap().get(url) {
                return Ok(response.clone());
            }
            for (needle, response) in self.patterns.lock().unwrap().iter() {
                if url.contains(needle.as_str()) {
                    return Ok(response.clone());
                }
            }
            Ok(HttpResponse::new(
                404,
                b"{\"message\":\"Not Found\"}".to_vec(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeHttp;
    use super::*;

    #[test]
    fn headers_are_matched_without_regard_to_case() {
        let response = HttpResponse::new(200, "hi").with_header("X-RateLimit-Remaining", "59");
        assert_eq!(response.header("x-ratelimit-remaining"), Some("59"));
        assert_eq!(response.header_int("X-RATELIMIT-REMAINING"), Some(59));
        assert_eq!(response.header("absent"), None);
    }

    #[test]
    fn the_fake_records_requests_and_falls_back_to_404() {
        let http = FakeHttp::new();
        http.json("https://example.test/a", "{\"ok\":true}");

        let hit = http.get("https://example.test/a", &[]).unwrap();
        assert_eq!(hit.status, 200);
        assert_eq!(hit.text(), "{\"ok\":true}");

        let miss = http.get("https://example.test/b", &[]).unwrap();
        assert_eq!(miss.status, 404);

        assert_eq!(
            http.urls(),
            ["https://example.test/a", "https://example.test/b"]
        );
    }

    /// Guards the bug that made a spent GitHub rate limit unrecognisable:
    /// `ureq` reports a non-2xx as an error, and unwrapping that back into a
    /// bare status code threw away the very headers and body that say *why*
    /// the request was refused. Served over loopback, so it needs no internet.
    #[test]
    fn a_non_2xx_keeps_its_headers_and_body() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let body = br#"{"message":"API rate limit exceeded for 1.2.3.4"}"#;
            let response = format!(
                "HTTP/1.1 403 Forbidden\r\n\
                 Content-Type: application/json\r\n\
                 X-RateLimit-Remaining: 0\r\n\
                 X-RateLimit-Limit: 60\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(body);
            let _ = stream.flush();
        });

        let http = UreqHttp::new();
        let response = http
            .get(&format!("http://127.0.0.1:{port}/"), &[])
            .expect("a 403 is a response, not a transport failure");

        assert_eq!(response.status, 403);
        assert_eq!(response.header_int("x-ratelimit-remaining"), Some(0));
        assert_eq!(response.header_int("x-ratelimit-limit"), Some(60));
        assert!(
            response.text().contains("rate limit"),
            "body was lost: {:?}",
            response.text()
        );

        server.join().expect("server thread");
    }

    /// The cap that made every skill in a large monorepo uninstallable:
    /// `github/awesome-copilot` serves an 86 MB archive, and the transport
    /// stops reading past [`MAX_BODY_BYTES`]. Callers rely on getting
    /// [`HttpError::TooLarge`] rather than a truncated body, because a
    /// truncated `tar.gz` unpacks as a corrupt archive and the error would
    /// then blame the archive rather than its size.
    ///
    /// Served over loopback, so it needs no internet. The body is written in
    /// chunks rather than allocated whole, and the writes stop being read as
    /// soon as the client gives up, so the server's own errors are ignored.
    #[test]
    fn a_body_past_the_cap_is_refused_rather_than_truncated() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            // No Content-Length: the body runs to the close, which is how
            // codeload serves an archive.
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\n\
                  Content-Type: application/x-gzip\r\n\
                  Connection: close\r\n\r\n",
            );
            let chunk = vec![0u8; 64 * 1024];
            let mut sent = 0usize;
            while sent <= MAX_BODY_BYTES {
                if stream.write_all(&chunk).is_err() {
                    break;
                }
                sent += chunk.len();
            }
            let _ = stream.flush();
        });

        let http = UreqHttp::new();
        let error = http
            .get(&format!("http://127.0.0.1:{port}/"), &[])
            .expect_err("a body past the cap is an error");

        match error {
            HttpError::TooLarge { limit, .. } => assert_eq!(limit, MAX_BODY_BYTES),
            other => panic!("expected TooLarge, got {other:?}"),
        }

        server.join().expect("server thread");
    }
}
