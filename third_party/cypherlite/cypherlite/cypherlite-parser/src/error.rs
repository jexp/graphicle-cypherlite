use std::fmt;

/// A parser error with source position information.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
    pub line: usize,
    pub col: usize,
    /// Optional source line text for caret display.
    pub source_line: Option<String>,
    /// Optional symbolic openCypher-TCK error-*detail* name for this parse
    /// failure (e.g. `"IntegerOverflow"`, `"FloatingPointOverflow"`).
    ///
    /// A parse failure always renders as a `SyntaxError` *class*, but the TCK
    /// error grammar also carries a finer detail (`a SyntaxError should be
    /// raised at compile time: IntegerOverflow`). This tag lets
    /// `EngineError::kind()` surface that detail for the TCK harness
    /// matcher without depending on `cypherlite-error` (the dependency runs the
    /// other way): the consumer maps the string through `ErrorKind::from_name`.
    /// It is **never** rendered by `Display`, so tagging a `ParseError`
    /// preserves the byte-for-byte error string (the prime invariant).
    pub detail: Option<&'static str>,
}

impl ParseError {
    pub fn new(message: impl Into<String>, line: usize, col: usize) -> Self {
        ParseError {
            message: message.into(),
            line,
            col,
            source_line: None,
            detail: None,
        }
    }

    pub fn with_source(mut self, source: &str) -> Self {
        let lines: Vec<&str> = source.split('\n').collect();
        if self.line >= 1 && self.line <= lines.len() {
            self.source_line = Some(lines[self.line - 1].to_string());
        }
        self
    }

    /// Tags this error with a symbolic TCK error-detail name (see
    /// [`ParseError::detail`]). Does not affect `Display`.
    pub fn with_detail(mut self, detail: &'static str) -> Self {
        self.detail = Some(detail);
        self
    }
}

/// Maximum source chars echoed on each side of the error column.
///
/// Bounds the `Display` rendering (and its cost) for pathological
/// single-line inputs: without a window, a 100 KB one-line query would echo
/// ~200 KB into the error string and into logs / wire failure messages.
const ECHO_SIDE_CHARS: usize = 120;
/// Marker prepended/appended when the echoed source line is truncated.
const ELLIPSIS: char = '…';

/// Converts a 1-based byte column (as tracked by the lexer) into a 0-based
/// char index into `line`. Boundary-safe: a column landing mid-character
/// resolves to the char containing that byte, and a column past the end of
/// the line resolves to `line.chars().count()`.
fn byte_col_to_char_idx(line: &str, col: usize) -> usize {
    let byte_idx = col.saturating_sub(1);
    line.char_indices()
        .take_while(|(b, _)| *b < byte_idx)
        .count()
}

/// Windows `line` around the 1-based byte `col`, returning the excerpt plus
/// the 1-based caret column within that excerpt. Lexer columns are
/// byte-based, so `col` is first converted to a char index — otherwise
/// multi-byte chars before the error shift the caret right and can push the
/// true error character outside the excerpt entirely. Lines that fit the
/// window are returned unchanged so short-line rendering is byte-for-byte
/// identical to the unwindowed behavior.
fn window_source_excerpt(line: &str, col: usize) -> (String, usize) {
    let chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    let char_idx = byte_col_to_char_idx(line, col);
    let caret_idx = char_idx.min(len.saturating_sub(1));
    let start = caret_idx.saturating_sub(ECHO_SIDE_CHARS);
    let end = (caret_idx + ECHO_SIDE_CHARS + 1).min(len);
    if start == 0 && end == len {
        // Untruncated lines keep the legacy byte-for-byte rendering: the
        // raw column for out-of-range columns (historical fallback), the
        // char column otherwise (identical to the raw column on ASCII).
        let caret_col = if col.saturating_sub(1) >= line.len() {
            col
        } else {
            char_idx + 1
        };
        return (line.to_string(), caret_col);
    }
    let mut excerpt = String::with_capacity(end - start + 2);
    let mut caret_col = caret_idx - start + 1;
    if start > 0 {
        excerpt.push(ELLIPSIS);
        caret_col += 1;
    }
    excerpt.extend(chars[start..end].iter());
    if end < len {
        excerpt.push(ELLIPSIS);
    }
    (excerpt, caret_col)
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(ref src_line) = self.source_line {
            if self.col > 0 {
                let (excerpt, caret_col) = window_source_excerpt(src_line, self.col);
                let caret = " ".repeat(caret_col - 1) + "^";
                return write!(
                    f,
                    "parse error: {}\n  {}\n  {}",
                    self.message, excerpt, caret
                );
            }
        }
        write!(
            f,
            "parse error: {} at line {}, col {}",
            self.message, self.line, self.col
        )
    }
}

impl std::error::Error for ParseError {}

pub type ParseResult<T> = Result<T, ParseError>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `ParseError` at (line 1, `col`) whose source echo is
    /// `line_text`.
    fn error_at(col: usize, line_text: &str) -> ParseError {
        ParseError::new("boom", 1, col).with_source(line_text)
    }

    /// Splits a rendered error into (source excerpt, caret line), both
    /// without the two-space indent.
    fn rendered_excerpt_and_caret(rendered: &str) -> (&str, &str) {
        let mut lines = rendered.lines();
        assert!(lines.next().is_some_and(|l| l.starts_with("parse error:")));
        let excerpt = lines.next().expect("excerpt line");
        let caret = lines.next().expect("caret line");
        (
            excerpt.strip_prefix("  ").expect("excerpt indent"),
            caret.strip_prefix("  ").expect("caret indent"),
        )
    }

    #[test]
    fn long_single_line_error_renders_bounded_excerpt() {
        let line = "a".repeat(100 * 1024);
        let rendered = error_at(50 * 1024, &line).to_string();
        let (excerpt, caret) = rendered_excerpt_and_caret(&rendered);
        // 2 * 120 window chars + at most 2 ellipsis markers.
        assert!(
            excerpt.chars().count() < 400,
            "excerpt must be bounded, got {} chars",
            excerpt.chars().count()
        );
        assert!(caret.chars().count() < 400);
        assert!(excerpt.starts_with(ELLIPSIS) && excerpt.ends_with(ELLIPSIS));
    }

    #[test]
    fn caret_points_at_true_column_within_window() {
        // Distinct marker char mid-line in a very long line.
        let mut line = "a".repeat(50_000);
        line.push('X');
        line.push_str(&"a".repeat(50_000));
        let col = 50_001; // 1-based column of 'X'
        let rendered = error_at(col, &line).to_string();
        let (excerpt, caret) = rendered_excerpt_and_caret(&rendered);

        let x_pos = excerpt
            .chars()
            .position(|c| c == 'X')
            .expect("marker char must survive windowing");
        let caret_pos = caret
            .chars()
            .position(|c| c == '^')
            .expect("caret must be present");
        assert_eq!(
            x_pos, caret_pos,
            "caret must point at the true error column within the excerpt"
        );
    }

    #[test]
    fn short_line_renders_exactly_as_before() {
        let rendered = error_at(7, "CREATE (n)").to_string();
        assert_eq!(rendered, "parse error: boom\n  CREATE (n)\n        ^");
    }

    #[test]
    fn col_beyond_line_length_keeps_legacy_caret_position() {
        // Untruncated lines must keep the old byte-for-byte rendering even
        // when the column overshoots the line length.
        let rendered = error_at(30, "MATCH (n)").to_string();
        let expected = format!("parse error: boom\n  MATCH (n)\n  {}^", " ".repeat(29));
        assert_eq!(rendered, expected);
    }

    #[test]
    fn caret_aligns_after_cjk_chars_on_long_line() {
        // 500 CJK chars (3 bytes each), a marker, then 500 more: the byte
        // column of the marker (1501) is far outside the char window around
        // its char index (500), so without byte→char conversion the marker
        // falls outside the excerpt entirely.
        let mut line = "漢".repeat(500);
        line.push('X');
        line.push_str(&"漢".repeat(500));
        let byte_col = 500 * 3 + 1; // 1-based byte column of 'X'
        let rendered = error_at(byte_col, &line).to_string();
        let (excerpt, caret) = rendered_excerpt_and_caret(&rendered);

        let x_pos = excerpt
            .chars()
            .position(|c| c == 'X')
            .expect("marker char must survive windowing");
        let caret_pos = caret
            .chars()
            .position(|c| c == '^')
            .expect("caret must be present");
        assert_eq!(
            x_pos, caret_pos,
            "caret must point at the true error character within the excerpt"
        );
    }

    #[test]
    fn caret_aligns_after_emoji_on_long_line() {
        // A 4-byte emoji before the error must not shift the caret.
        let mut line = "a".repeat(300);
        line.push('🦀');
        line.push_str(&"a".repeat(300));
        line.push('X');
        line.push_str(&"a".repeat(300));
        let byte_col = 300 + 4 + 300 + 1; // 1-based byte column of 'X'
        let rendered = error_at(byte_col, &line).to_string();
        let (excerpt, caret) = rendered_excerpt_and_caret(&rendered);

        let x_pos = excerpt
            .chars()
            .position(|c| c == 'X')
            .expect("marker char must survive windowing");
        let caret_pos = caret
            .chars()
            .position(|c| c == '^')
            .expect("caret must be present");
        assert_eq!(x_pos, caret_pos);
    }

    #[test]
    fn caret_aligns_at_window_boundary_with_mixed_width_chars() {
        // Alternating ASCII and 2-byte chars; the marker sits at the first
        // char index where the window starts sliding (start becomes 1).
        let mut line = String::new();
        for i in 0..=ECHO_SIDE_CHARS {
            line.push(if i % 2 == 0 { 'a' } else { 'é' });
        }
        let byte_col = line.len() + 1; // 1-based byte column of the marker
        line.push('X');
        line.push_str(&"ab".repeat(500));
        let rendered = error_at(byte_col, &line).to_string();
        let (excerpt, caret) = rendered_excerpt_and_caret(&rendered);

        assert!(excerpt.starts_with(ELLIPSIS));
        let x_pos = excerpt
            .chars()
            .position(|c| c == 'X')
            .expect("marker char must survive windowing");
        let caret_pos = caret
            .chars()
            .position(|c| c == '^')
            .expect("caret must be present");
        assert_eq!(x_pos, caret_pos);
    }

    #[test]
    fn short_multibyte_line_caret_uses_char_column() {
        // 'é' is 2 bytes: the lexer column of 'b' is 4, its char column 3.
        let rendered = error_at(4, "éab").to_string();
        assert_eq!(rendered, "parse error: boom\n  éab\n    ^");
    }

    #[test]
    fn error_near_line_start_truncates_only_at_end() {
        let line = "b".repeat(1_000);
        let rendered = error_at(3, &line).to_string();
        let (excerpt, _) = rendered_excerpt_and_caret(&rendered);
        assert!(!excerpt.starts_with(ELLIPSIS));
        assert!(excerpt.ends_with(ELLIPSIS));
        // 2 chars before the caret char + caret char + 120 window chars
        // after + 1 trailing ellipsis.
        assert_eq!(excerpt.chars().count(), 2 + 1 + ECHO_SIDE_CHARS + 1);
    }
}
