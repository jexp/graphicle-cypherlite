//! The raw-response parsers behind the loopback fixtures: one response
//! splitter, one RFC 9112 chunked-frame walker, one gzip inflater, and the
//! one case-insensitive header scan. The fixture module drives them; the
//! server-surface tests reach them through the `loopback` re-exports.

use std::io::Read;

/// The status code out of the response head: the second whitespace-delimited
/// token of the status line.
pub fn status_code(raw: &[u8]) -> u16 {
    let mut parts = raw.split(|byte| byte.is_ascii_whitespace());
    let _version = parts.next();
    std::str::from_utf8(parts.next().expect("the status line has a code"))
        .expect("the status code is ASCII")
        .parse()
        .expect("status code")
}

/// Split one raw response at the head/body separator: the head verbatim
/// (case preserved — [`header_value`] compares names case-insensitively) and
/// the body bytes untouched, so a compressed body stays intact. Both halves
/// of the parse are loud: a response without the separator is a broken
/// server, not an empty body, and a non-UTF-8 head is not a response head.
/// The one response splitter — [`exchange`] and the server-surface tests
/// share it.
pub fn split_response(raw: &[u8]) -> (&str, &[u8]) {
    const SEPARATOR: &[u8] = b"\r\n\r\n";
    let index = raw
        .windows(SEPARATOR.len())
        .position(|window| window == SEPARATOR)
        .expect("head/body separator");
    let head = std::str::from_utf8(&raw[..index]).expect("the head is UTF-8");
    (head, &raw[index + SEPARATOR.len()..])
}

/// Walk an RFC 9112 §7.1 chunked body and return every frame's payload slice
/// in wire order — the one frame parser [`dechunk`] and [`max_frame_size`]
/// share. Every framing rule a `Transfer-Encoding: chunked` writer owes is
/// asserted — lowercase-hex sizes, a payload per size terminated by CRLF, one
/// zero-size last chunk followed by a single CRLF and nothing else (no
/// trailer section, no trailing bytes) — so a malformed frame fails the test
/// instead of being tolerated.
fn frame_payloads(frames: &[u8]) -> Vec<&[u8]> {
    let mut payloads = Vec::new();
    let mut rest = frames;
    loop {
        let end = rest
            .windows(2)
            .position(|window| window == b"\r\n")
            .expect("a chunk-size line");
        let size_line = std::str::from_utf8(&rest[..end]).expect("an ASCII chunk size");
        assert!(
            !size_line.is_empty()
                && size_line
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "chunk sizes are lowercase hex: {size_line:?}"
        );
        let size = usize::from_str_radix(size_line, 16).expect("a hex chunk size");
        rest = &rest[end + 2..];
        if size == 0 {
            assert_eq!(
                rest, b"\r\n",
                "the last chunk ends the body: one CRLF, no trailer section"
            );
            return payloads;
        }
        assert!(rest.len() >= size + 2, "the frame carries its payload");
        payloads.push(&rest[..size]);
        assert_eq!(
            &rest[size..size + 2],
            b"\r\n",
            "the chunk payload is CRLF-terminated"
        );
        rest = &rest[size + 2..];
    }
}

/// De-chunk an RFC 9112 §7.1 response body: walk the chunk-size lines and
/// reassemble the payload bytes. Returns the body and the number of payload
/// frames. Every framing rule a `Transfer-Encoding: chunked` writer owes is
/// asserted — lowercase-hex sizes, a payload per size terminated by CRLF, one
/// zero-size last chunk followed by a single CRLF and nothing else (no
/// trailer section, no trailing bytes) — so a malformed frame fails the test
/// instead of being tolerated. The caller passes the raw bytes after the
/// head/body separator.
pub fn dechunk(frames: &[u8]) -> (Vec<u8>, usize) {
    let payloads = frame_payloads(frames);
    (payloads.concat(), payloads.len())
}

/// The largest payload among a chunked body's frames, across every frame (an
/// empty body has none, so the maximum is `0`). Walks the frames with
/// [`dechunk`]'s one parser, asserting the same framing rules — the bound
/// tests assert this against the writer's frame buffer so a frame larger than
/// the documented bound cannot hide behind the first frame.
pub fn max_frame_size(frames: &[u8]) -> usize {
    frame_payloads(frames)
        .iter()
        .map(|payload| payload.len())
        .max()
        .unwrap_or(0)
}

/// Inflate one gzip member (RFC 1952) — the `content-encoding: gzip`
/// counterpart of [`dechunk`]. The caller passes the de-chunked (or
/// one-shot) body bytes; the whole member must inflate, so a truncated body
/// fails the test here instead of surfacing as a mismatched envelope.
pub fn gunzip(compressed: &[u8]) -> Vec<u8> {
    let mut inflated = Vec::new();
    flate2::read::GzDecoder::new(compressed)
        .read_to_end(&mut inflated)
        .expect("the body is one complete gzip member");
    inflated
}

/// The one header scan over a captured request head or a response head:
/// every value of the `name` header in wire order, with surrounding OWS
/// trimmed. Header names compare case-insensitively (HTTP); values do not
/// (RFC 9110 §5.1).
fn header_values_in<'a>(head: &'a str, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    head.lines().filter_map(move |line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// The first value of the `name` header in a captured request head or a
/// response head, borrowed — the one header lookup every `Option` assertion
/// uses (the compression, framing and CORS tests). [`header_values`] is its
/// all-values form.
pub fn header_value<'a>(head: &'a str, name: &'a str) -> Option<&'a str> {
    header_values_in(head, name).next()
}

/// Every value of the `name` header in a captured request head or a response
/// head, in wire order. Header names compare case-insensitively (HTTP);
/// values do not (RFC 9110 §5.1).
pub fn header_values(head: &str, name: &str) -> Vec<String> {
    header_values_in(head, name).map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_values_matches_names_case_insensitively_in_wire_order() {
        let head = "POST / HTTP/1.1\r\nAccept: one\r\naccept: two\r\nX-Other: no\r\n\r\n";
        assert_eq!(header_values(head, "ACCEPT"), vec!["one", "two"]);
        assert_eq!(header_values(head, "x-other"), vec!["no"]);
        assert!(header_values(head, "missing").is_empty());
    }

    #[test]
    fn header_value_is_the_first_value_of_the_one_header_scan() {
        let head = "HTTP/1.1 200 OK\r\nVary: accept-encoding\r\nvary: origin\r\n\r\n";
        assert_eq!(header_value(head, "VARY"), Some("accept-encoding"));
        assert_eq!(header_value(head, "Vary"), Some("accept-encoding"));
        assert_eq!(header_value(head, "missing"), None);
        assert_eq!(
            header_values(head, "vary"),
            vec!["accept-encoding", "origin"]
        );
    }

    #[test]
    fn split_response_keeps_the_head_and_the_body_bytes() {
        let raw = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}tail";
        let (head, body) = split_response(raw);
        assert_eq!(head, "HTTP/1.1 200 OK\r\ncontent-length: 2");
        assert_eq!(
            body, b"{}tail",
            "the body is every byte after the separator"
        );
        // A compressed body is not UTF-8: the split must not touch it.
        let compressed = [0x1f, 0x8b, 0xff];
        let mut raw = b"HTTP/1.1 200 OK\r\ncontent-encoding: gzip\r\n\r\n".to_vec();
        raw.extend_from_slice(&compressed);
        let (_, body) = split_response(&raw);
        assert_eq!(body, compressed);
    }

    #[test]
    #[should_panic(expected = "head/body separator")]
    fn split_response_refuses_a_response_without_a_body() {
        split_response(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n");
    }

    #[test]
    fn gunzip_inflates_one_gzip_member() {
        use std::io::Write as _;

        let payload = b"{\"data\":{\"values\":[[1],[2]]}}";
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(payload).unwrap();
        let compressed = encoder.finish().unwrap();
        assert_eq!(gunzip(&compressed), payload);
    }

    #[test]
    fn dechunk_reassembles_frames_and_counts_them() {
        assert_eq!(dechunk(b"5\r\nhello\r\n0\r\n\r\n"), (b"hello".to_vec(), 1));
        assert_eq!(
            dechunk(b"a\r\n0123456789\r\n1\r\nx\r\n0\r\n\r\n"),
            (b"0123456789x".to_vec(), 2)
        );
        assert_eq!(dechunk(b"0\r\n\r\n"), (Vec::new(), 0));
    }

    #[test]
    fn max_frame_size_walks_every_frame() {
        assert_eq!(
            max_frame_size(b"5\r\nhello\r\n1\r\nx\r\n0\r\n\r\n"),
            5,
            "the largest frame is later than the first"
        );
        assert_eq!(
            max_frame_size(b"0\r\n\r\n"),
            0,
            "an empty body has no frame"
        );
    }

    #[test]
    #[should_panic(expected = "lowercase hex")]
    fn dechunk_rejects_an_uppercase_size() {
        dechunk(b"A\r\n0123456789\r\n0\r\n\r\n");
    }

    #[test]
    #[should_panic(expected = "no trailer section")]
    fn dechunk_rejects_a_trailer_section() {
        dechunk(b"1\r\nx\r\n0\r\nx-trailer: 1\r\n\r\n");
    }
}
