//! Loopback HTTP fixtures in two directions: a scripted raw-HTTP **server**
//! on `127.0.0.1` plus the [`LoadOpts`] that point a pull at it (the
//! `cypherlite-load` tests, the CLI e2e pull tests, the `pull_rca` loader
//! bench, ADR-0080), and the raw **client** helpers ([`exchange`],
//! [`exchange_bytes`], [`request`], [`post`]) the server-surface tests (Query
//! API in `cypherlite-http`, the MCP streamable-HTTP transport, the CLI's
//! `e2e_serve`) drive against their own listeners. Every request head a test
//! sends is assembled here: [`post`] and its siblings ([`post_with_headers`],
//! [`post_with_content_type`], [`post_head_only`], [`post_with_version`]) and
//! [`request`] are thin cases over one assembler, so no test module re-spells
//! `Host`, `content-type` or `content-length`.
//!
//! The response side is parsed here too, so a wire test never carries its own
//! scanner: [`split_response`] cuts one raw response at the head/body
//! separator (keeping a compressed body's bytes intact), [`header_value`] and
//! [`header_values`] are the one case-insensitive header scan, [`dechunk`]
//! reassembles RFC 9112 chunk frames (and [`max_frame_size`] reports the
//! largest frame's payload for the framing-bound tests), and [`gunzip`]
//! inflates a gzip body.
//!
//! The [`spawn_*_capturing`](spawn_scripted_responses_capturing) variants
//! record the request head of every connection they serve — the server
//! thread's join value — so tests can pin wire-level request details
//! (ADR-0181: the pull path's typed `Accept` header); [`header_values`]
//! pulls one header's values out of a captured head.
//!
//! [`spawn_capture_server`] is the run-until-stopped variant: it answers
//! every request with one fixed response and records each request body
//! until the returned [`CaptureSink`] is dropped, so tests whose request
//! count is not known up front (a background worker flushing events) can
//! poll the recordings instead of joining a scripted server.
//!
//! std::net only — the fixture serves fixed bodies, so a real HTTP stack
//! would be dead weight.

use std::borrow::Cow;
use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(feature = "loopback")]
use cypherlite_load::{Credentials, LoadOpts};

mod response;

pub use response::{dechunk, gunzip, header_value, header_values, max_frame_size, split_response};

/// The raw loopback exchange primitive: connect, send `request`, read the
/// whole response to EOF (the server surfaces answer `Connection: close`),
/// and return the status code plus the untouched raw response bytes (head
/// included). Byte-level so a compressed body — which is not UTF-8 — stays
/// intact; [`exchange_raw`] is its UTF-8 form.
pub fn exchange_bytes(port: u16, request: &[u8]) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.write_all(request).expect("write request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    let status = response::status_code(&raw);
    (status, raw)
}

/// The raw loopback exchange for text responses: [`exchange_bytes`] with the
/// whole response (head included) decoded as UTF-8 — the form tests that
/// assert on response *headers* use. A non-UTF-8 (compressed) body must go
/// through [`exchange_bytes`] instead.
pub fn exchange_raw(port: u16, request: &[u8]) -> (u16, String) {
    let (status, raw) = exchange_bytes(port, request);
    let raw = String::from_utf8(raw).expect("the raw response is UTF-8");
    (status, raw)
}

/// A raw loopback HTTP exchange for server-surface tests (the Query API
/// and MCP surfaces), split into status code and body (the head is
/// dropped). Built on [`exchange_raw`] and [`split_response`].
pub fn exchange(port: u16, request: &[u8]) -> (u16, String) {
    let (status, raw) = exchange_bytes(port, request);
    let (_, body) = split_response(&raw);
    (
        status,
        String::from_utf8(body.to_vec()).expect("the response body is UTF-8"),
    )
}

/// The `Host` every loopback request names unless a test varies it — the
/// discovery document echoes the request's Host back (`query_api`), so one
/// test does vary it.
pub const LOOPBACK_HOST: &str = "localhost";

/// The request media type every loopback POST defaults to.
pub const JSON_CONTENT_TYPE: &str = "application/json";

/// The request-line version every builder sends; the HTTP/1.0 refusal test
/// is the one variation ([`post_with_version`]).
const HTTP_1_1: &str = "HTTP/1.1";

/// The one request-head assembler: request line, `Host`, `headers` in wire
/// order, and — when `content_length` is `Some` — the declared
/// `content-length` followed by `body`. [`request`] and [`post_wire`] are its
/// only callers, so a wire-level change to a test request is one edit.
fn assemble<'a>(
    version: &str,
    method: &str,
    path: &str,
    host: &str,
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
    body: PostBody<'_>,
) -> String {
    let mut request = format!("{method} {path} {version}\r\nhost: {host}\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    let (declared, bytes) = match body {
        PostBody::Absent => (None, ""),
        PostBody::Sent(bytes) => (Some(bytes.len()), bytes),
        PostBody::Withheld(length) => (Some(length), ""),
    };
    if let Some(length) = declared {
        request.push_str(&format!("content-length: {length}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(bytes);
    request
}

/// Build a bodyless request (GET, OPTIONS, …) for `path` at `host`: the
/// request line, `Host`, and `headers` in wire order, with no body and no
/// `content-length`.
pub fn request(method: &str, path: &str, host: &str, headers: &[(&str, &str)]) -> String {
    assemble(
        HTTP_1_1,
        method,
        path,
        host,
        headers.iter().copied(),
        PostBody::Absent,
    )
}

/// One test request's head-to-body relation: the bytes written after the
/// head and the `content-length` the head declares for them. The normal case
/// declares exactly the bytes it sends ([`PostBody::Sent`]); the body-cap
/// fixtures declare a length they never send ([`PostBody::Withheld`]); a
/// bodyless request ([`PostBody::Absent`]) declares nothing.
enum PostBody<'a> {
    /// No body and no `content-length` (GET, OPTIONS, …).
    Absent,
    /// The normal case: the body is the given bytes and the head declares
    /// their length.
    Sent(&'a str),
    /// A head-only request: the head declares the length and no body bytes
    /// follow it.
    Withheld(usize),
}

/// The one POST assembler: request line at `version`, `Host`, the request
/// `content_type`, `extra_headers` in wire order, the declared
/// `content-length`, and the body. Every POST builder below delegates here,
/// so a wire-level change to a test POST is one edit.
fn post_wire(
    version: &str,
    path: &str,
    content_type: &str,
    extra_headers: &[(&str, &str)],
    body: PostBody<'_>,
) -> String {
    assemble(
        version,
        "POST",
        path,
        LOOPBACK_HOST,
        std::iter::once(("content-type", content_type)).chain(extra_headers.iter().copied()),
        body,
    )
}

/// Build a POST request for `path` against a JSON surface: JSON content
/// type, an optional pre-encoded `authorization` header value, and the
/// body.
pub fn post(path: &str, body: &str, auth: Option<&str>) -> String {
    let auth_header: &[(&str, &str)] = match auth {
        Some(auth) => &[("authorization", auth)],
        None => &[],
    };
    post_with_headers(path, body, auth_header)
}

/// [`post`] with `extra_headers` appended after the JSON content type, in
/// wire order — the caller-chosen request headers (`origin`,
/// `accept-encoding`, …) the compression and browser-guard tests vary.
pub fn post_with_headers(path: &str, body: &str, extra_headers: &[(&str, &str)]) -> String {
    post_with_content_type(path, JSON_CONTENT_TYPE, body, extra_headers)
}

/// A POST whose request content type is `content_type` rather than JSON,
/// with `extra_headers` in wire order: the media-type negotiation fixtures,
/// where the request media type is the behavior under test — and the one
/// content-type-carrying POST builder [`post`] and [`post_with_headers`]
/// delegate to.
pub fn post_with_content_type(
    path: &str,
    content_type: &str,
    body: &str,
    extra_headers: &[(&str, &str)],
) -> String {
    post_wire(
        HTTP_1_1,
        path,
        content_type,
        extra_headers,
        PostBody::Sent(body),
    )
}

/// A POST that sends only its head: it declares `content_length` and writes
/// no body bytes — the fixture for the reader's body cap and the
/// auth-before-body ordering, where the declared length deliberately exceeds
/// both the bytes on the wire and the configured cap.
pub fn post_head_only(path: &str, content_length: usize) -> String {
    post_wire(
        HTTP_1_1,
        path,
        JSON_CONTENT_TYPE,
        &[],
        PostBody::Withheld(content_length),
    )
}

/// [`post`] whose request line names `version` rather than HTTP/1.1 — the
/// HTTP/1.0 refusal fixture (RFC 9112 §2.3: the request version is part of
/// what the surface answers).
pub fn post_with_version(version: &str, path: &str, body: &str) -> String {
    post_wire(version, path, JSON_CONTENT_TYPE, &[], PostBody::Sent(body))
}

/// [`LoadOpts`] for `query` against a loopback fixture on `port`: plain
/// `http://127.0.0.1` (loopback needs no plaintext opt-in), fixed test
/// credentials, no params. The loader bridge, behind the `loopback` feature.
#[cfg(feature = "loopback")]
pub fn loopback_opts(port: u16, query: &str) -> LoadOpts {
    LoadOpts {
        credentials: Credentials {
            uri: format!("http://127.0.0.1:{port}"),
            username: "neo4j".into(),
            password: "pw".into(),
            database: "neo4j".into(),
        },
        query: query.into(),
        params: serde_json::Map::new(),
        allow_plaintext: false,
    }
}

/// Serve scripted raw HTTP responses on loopback, one body per connection.
/// The server thread's join value is the captured request head of every
/// connection it served, in arrival order.
pub fn spawn_scripted_responses(
    status_line: &'static str,
    bodies: Vec<impl Into<Cow<'static, str>>>,
) -> (std::thread::JoinHandle<Vec<String>>, u16) {
    let bodies: Vec<Cow<'static, str>> = bodies.into_iter().map(Into::into).collect();
    spawn_scripted(status_line, bodies)
}

/// Serve one raw HTTP response on loopback. The server thread's join value
/// is the captured request head of the connection it served.
pub fn spawn_single_response(
    status_line: &'static str,
    body: impl Into<Cow<'static, str>>,
) -> (std::thread::JoinHandle<Vec<String>>, u16) {
    spawn_scripted_responses(status_line, vec![body])
}

/// Like [`spawn_scripted_responses`], but the returned [`CapturingFixture`]
/// exposes the request head of every connection served, so tests can assert
/// wire-level request details (ADR-0181).
pub fn spawn_scripted_responses_capturing(
    status_line: &'static str,
    bodies: Vec<impl Into<Cow<'static, str>>>,
) -> (CapturingFixture, u16) {
    let bodies: Vec<Cow<'static, str>> = bodies.into_iter().map(Into::into).collect();
    let (thread, port) = spawn_scripted(status_line, bodies);
    (CapturingFixture { thread }, port)
}

/// Like [`spawn_scripted_responses_capturing`], but the returned
/// [`CapturedRequests`] fixture exposes the **whole request** — head and
/// body — of every connection served, for suites that assert on request
/// payloads (the push write path's statements and parameters, ADR-0401).
pub fn spawn_scripted_responses_capturing_requests(
    status_line: &'static str,
    bodies: Vec<impl Into<Cow<'static, str>>>,
) -> (CapturedRequests, u16) {
    let bodies: Vec<Cow<'static, str>> = bodies.into_iter().map(Into::into).collect();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    listener.set_nonblocking(true).expect("nonblocking");
    let thread = std::thread::spawn(move || {
        bodies
            .iter()
            .map_while(|body| serve_one_full(&listener, status_line, body))
            .collect()
    });
    (
        CapturedRequests {
            thread: Some(thread),
        },
        port,
    )
}

/// A scripted fixture that records the whole request — head and body — of
/// every connection it served (see
/// [`spawn_scripted_responses_capturing_requests`]). Panics on join when
/// the server thread panicked, like [`CapturingFixture`].
pub struct CapturedRequests {
    thread: Option<std::thread::JoinHandle<Vec<(String, String)>>>,
}

impl CapturedRequests {
    /// Wait for the fixture to serve every scripted body and return every
    /// captured request as `(head, body)` pairs, in arrival order.
    pub fn join(mut self) -> Vec<(String, String)> {
        self.thread
            .take()
            .expect("join once")
            .join()
            .expect("scripted server thread")
    }
}

/// Like [`spawn_single_response`], but captures the request head.
pub fn spawn_single_response_capturing(
    status_line: &'static str,
    body: impl Into<Cow<'static, str>>,
) -> (CapturingFixture, u16) {
    spawn_scripted_responses_capturing(status_line, vec![body])
}

/// Serve one fixed raw HTTP response per request on loopback, running until
/// the returned [`CaptureSink`] is stopped or dropped. Unlike the scripted
/// spawns, the server has no fixed request count: it records every request
/// body it serves, in arrival order, for the sink to expose.
pub fn spawn_capture_server(
    status_line: &'static str,
    body: impl Into<Cow<'static, str>>,
) -> (CaptureSink, u16) {
    let body = body.into();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    listener.set_nonblocking(true).expect("nonblocking");
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let bodies = Arc::clone(&bodies);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Some(served) = serve_connection(&stream, status_line, &body) {
                            bodies.lock().expect("bodies").push(served.body);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        })
    };
    (
        CaptureSink {
            bodies,
            stop,
            thread: Some(thread),
        },
        port,
    )
}

/// A run-until-stopped capture server (see [`spawn_capture_server`]):
/// records the body of every request it serves. Dropping the sink stops the
/// server and joins its thread.
pub struct CaptureSink {
    bodies: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CaptureSink {
    /// The request bodies recorded so far, in arrival order.
    pub fn bodies(&self) -> Vec<String> {
        self.bodies.lock().expect("bodies").clone()
    }

    /// How many requests the server has recorded so far.
    pub fn request_count(&self) -> usize {
        self.bodies.lock().expect("bodies").len()
    }

    /// Stop the server; the thread exits on its next accept poll (≤ 5 ms)
    /// and is joined on drop. Idempotent.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for CaptureSink {
    fn drop(&mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A scripted-fixture server that records the request head of every
/// connection it served (request line + headers, CRLF-terminated), one entry
/// per connection in arrival order.
pub struct CapturingFixture {
    thread: std::thread::JoinHandle<Vec<String>>,
}

impl CapturingFixture {
    /// Wait for the fixture to serve every scripted body and return the
    /// captured request heads. Panics when the server thread panicked, so a
    /// broken fixture fails the test here instead of surfacing as a
    /// misleading captured-heads count.
    pub fn join(self) -> Vec<String> {
        self.thread.join().expect("scripted server thread")
    }
}

/// Bind a scripted server and spawn its thread; the thread's join value is
/// the request head of every connection it reads, in arrival order.
fn spawn_scripted(
    status_line: &'static str,
    bodies: Vec<Cow<'static, str>>,
) -> (std::thread::JoinHandle<Vec<String>>, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    listener.set_nonblocking(true).expect("nonblocking");
    let handle = std::thread::spawn(move || {
        bodies
            .iter()
            .map_while(|body| serve_one(&listener, status_line, body))
            .collect()
    });
    (handle, port)
}

/// One served request: the captured head (request line + headers,
/// CRLF-terminated) and the request body.
struct ServedRequest {
    head: String,
    body: String,
}

/// Serve one connection of a scripted fixture: accept it, read its request,
/// write the response, and return the captured request head. Returns `None`
/// when no connection arrived before the deadline or the peer hung up
/// mid-request, so a test that never connects cannot hang the server thread.
fn serve_one(listener: &TcpListener, status_line: &str, body: &str) -> Option<String> {
    let stream = accept_one(listener)?;
    serve_connection(&stream, status_line, body).map(|served| served.head)
}

/// [`serve_one`]'s whole-request twin: the captured `(head, body)` pair, for
/// the fixtures that assert on request payloads (ADR-0401).
fn serve_one_full(
    listener: &TcpListener,
    status_line: &str,
    body: &str,
) -> Option<(String, String)> {
    let stream = accept_one(listener)?;
    serve_connection(&stream, status_line, body).map(|served| (served.head, served.body))
}

/// Serve one already-accepted connection: force the socket back to blocking
/// mode (on macOS/BSD an accepted socket inherits the listener's O_NONBLOCK
/// flag, and `set_read_timeout` does not make a nonblocking socket block),
/// read the request, write the response, and return the captured head and
/// body. `None` when the peer hung up mid-request.
fn serve_connection(stream: &TcpStream, status_line: &str, body: &str) -> Option<ServedRequest> {
    stream
        .set_nonblocking(false)
        .expect("blocking accepted socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .expect("set write timeout");
    let served = read_request(stream)?;
    let resp = format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    let mut write_half = stream;
    let _ = write_half.write_all(resp.as_bytes());
    Some(served)
}

/// Accept one connection from the nonblocking `listener`, polling on a 30 s
/// deadline; `None` when the deadline expires.
fn accept_one(listener: &TcpListener) -> Option<TcpStream> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return Some(stream),
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
}

/// Read one request head and its `Content-Length` body; `None` when the peer
/// hangs up first. Returns the head text (request line + headers + the
/// terminating CRLF) and the body for capture. `read_line` and `read_exact`
/// retry `Interrupted` internally, so an error here is a broken connection,
/// not a signal.
fn read_request(stream: &TcpStream) -> Option<ServedRequest> {
    let mut reader = std::io::BufReader::new(stream);
    let mut content_length = 0usize;
    let mut head = String::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return None,
            Ok(_) => {
                if let Some(rest) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = rest.trim().parse().unwrap_or(0);
                }
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            Err(_) => return None,
        }
    }
    let mut request_body = vec![0u8; content_length];
    reader.read_exact(&mut request_body).ok()?;
    Some(ServedRequest {
        head,
        body: String::from_utf8_lossy(&request_body).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_builds_a_json_post_with_optional_auth() {
        let request = post("/db/neo4j/query/v2", "{}", Some("Basic QQ=="));
        assert_eq!(
            request,
            "POST /db/neo4j/query/v2 HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\nauthorization: Basic QQ==\r\ncontent-length: 2\r\n\r\n{}"
        );
        assert!(!post("/x", "{}", None).contains("authorization"));
    }

    #[test]
    fn post_with_headers_appends_caller_headers_before_the_length() {
        let request = post_with_headers(
            "/db/neo4j/query/v2",
            "{}",
            &[
                ("origin", "https://browser.neo4j.io"),
                ("accept-encoding", "gzip"),
            ],
        );
        assert_eq!(
            request,
            "POST /db/neo4j/query/v2 HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\norigin: https://browser.neo4j.io\r\naccept-encoding: gzip\r\ncontent-length: 2\r\n\r\n{}"
        );
    }

    #[test]
    fn post_with_content_type_replaces_the_json_media_type() {
        let request = post_with_content_type(
            "/db/neo4j/query/v2",
            "application/vnd.neo4j.query",
            "{}",
            &[("accept", "application/json")],
        );
        assert_eq!(
            request,
            "POST /db/neo4j/query/v2 HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/vnd.neo4j.query\r\naccept: application/json\r\ncontent-length: 2\r\n\r\n{}"
        );
        assert_eq!(request.matches("content-type:").count(), 1);
    }

    #[test]
    fn post_head_only_declares_a_length_it_does_not_send() {
        let request = post_head_only("/db/neo4j/query/v2", 17);
        assert_eq!(
            request,
            "POST /db/neo4j/query/v2 HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\ncontent-length: 17\r\n\r\n"
        );
    }

    #[test]
    fn request_builds_a_bodyless_request_at_the_given_host() {
        let get = request(
            "GET",
            "/",
            "127.0.0.1:7474",
            &[("origin", "https://browser.example")],
        );
        assert_eq!(
            get,
            "GET / HTTP/1.1\r\nhost: 127.0.0.1:7474\r\norigin: https://browser.example\r\n\r\n"
        );
        assert_eq!(
            request("OPTIONS", "/db/neo4j/query/v2", LOOPBACK_HOST, &[]),
            "OPTIONS /db/neo4j/query/v2 HTTP/1.1\r\nhost: localhost\r\n\r\n"
        );
    }

    #[test]
    fn capturing_fixture_propagates_a_server_panic() {
        let fixture = CapturingFixture {
            thread: std::thread::spawn(|| panic!("server boom")),
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fixture.join()));
        let payload = result.unwrap_err();
        let msg = payload.downcast_ref::<String>();
        assert!(
            msg.is_some_and(|m| m.contains("scripted server thread")),
            "join must re-raise the server panic, got: {msg:?}"
        );
    }

    #[cfg(feature = "loopback")]
    #[test]
    fn loopback_opts_point_at_the_fixture_port() {
        let opts = loopback_opts(7474, "MATCH (n) RETURN n");
        assert_eq!(opts.credentials.uri, "http://127.0.0.1:7474");
        assert_eq!(opts.query, "MATCH (n) RETURN n");
        assert!(!opts.allow_plaintext);
    }

    #[test]
    fn scripted_server_answers_one_request_per_body() {
        const REQUEST: &[u8] = b"POST /db/neo4j/query/v2 HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}";
        let (h, port) = spawn_scripted_responses("200 OK", vec!["{\"a\":1}", "{\"b\":2}"]);
        for expected in ["{\"a\":1}", "{\"b\":2}"] {
            let (status, resp) = exchange(port, REQUEST);
            assert_eq!(status, 200, "{resp}");
            assert!(resp.ends_with(expected), "{resp}");
        }
        h.join().unwrap();
    }

    #[test]
    fn capture_server_records_bodies_until_stopped() {
        const REQUEST: &[u8] = b"POST /track HTTP/1.1\r\nContent-Length: 7\r\n\r\n[{\"a\":1";
        let (sink, port) = spawn_capture_server("200 OK", "1");
        for _ in 0..2 {
            let (status, resp) = exchange(port, REQUEST);
            assert!(
                status == 200 && resp == "1",
                "the fixed response is served: {resp}"
            );
        }
        assert_eq!(sink.request_count(), 2);
        assert_eq!(sink.bodies(), vec!["[{\"a\":1", "[{\"a\":1"]);
        sink.stop();
        // Stopping is idempotent and the sink still reads back; drop joins
        // the server thread.
        sink.stop();
        assert_eq!(sink.request_count(), 2);
    }

    #[test]
    fn capture_server_drop_stops_and_joins_the_thread() {
        let (sink, port) = spawn_capture_server("200 OK", "1");
        let (status, _) = exchange(port, b"POST / HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}");
        assert_eq!(status, 200);
        assert_eq!(sink.bodies(), vec!["{}"]);
        drop(sink); // must not hang: the accept loop polls the stop flag
    }

    #[test]
    fn capturing_fixture_records_every_request_head() {
        const REQUEST: &[u8] = b"POST /db/neo4j/query/v2 HTTP/1.1\r\n\
                                 Accept: application/json\r\n\
                                 Content-Length: 2\r\n\r\n{}";
        let (fixture, port) = spawn_scripted_responses_capturing("200 OK", vec!["{}", "{}"]);
        for _ in 0..2 {
            let (status, _) = exchange(port, REQUEST);
            assert_eq!(status, 200);
        }
        let heads = fixture.join();
        assert_eq!(heads.len(), 2, "one head per connection: {heads:?}");
        for head in &heads {
            assert!(
                head.starts_with("POST /db/neo4j/query/v2 HTTP/1.1\r\n"),
                "{head:?}"
            );
            assert_eq!(
                header_values(head, "accept"),
                vec!["application/json"],
                "the captured head parses back to its single Accept value: {head:?}"
            );
            assert!(head.ends_with("\r\n\r\n"), "{head:?}");
        }
    }
}
