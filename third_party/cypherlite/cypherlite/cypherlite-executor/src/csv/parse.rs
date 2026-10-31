//! CSV text parsing: RFC 4180 record and field splitting and the `LOAD CSV`
//! row-value construction. Pure functions over text — no filesystem or
//! policy concerns (those live in [`super::policy`]).

use std::collections::BTreeMap;

use cypherlite_core::{Error, Result, Value};

/// The one RFC 4180 quote-state rule (issue #91) shared by
/// [`split_csv_records`] and [`split_csv_line`], so the rule lives once:
/// a `"` opens a quoted field only at a field start; inside quotes `""` is
/// one literal `"` and a lone `"` closes the field; every other character —
/// delimiter and newline included — is field data.
#[derive(Default)]
struct QuoteScan {
    in_quotes: bool,
    field_started: bool,
}

/// What one input character means under the [`QuoteScan`] rule.
enum QuoteEvent {
    /// `c` is field data. Inside quotes that includes delimiters and
    /// newlines; outside quotes it includes a stray `"` that cannot open a
    /// field. Callers distinguish a record-breaking newline from quoted
    /// data via [`QuoteScan::in_quotes`].
    Data(char),
    /// `c` opened a quoted field.
    EnterQuotes,
    /// `c` closed a quoted field.
    ExitQuotes,
    /// `c` starts a `""` escape inside quotes: one literal `"`; the caller
    /// consumes the second `"`.
    EscapedQuote,
    /// `c` is the field delimiter outside quotes: the field ends here.
    FieldEnd,
}

impl QuoteScan {
    /// Classify `c` and advance the quote state. `next_is_quote` peeks the
    /// following character so a `""` escape is recognized without consuming
    /// it; `delim` is the field terminator.
    fn advance(&mut self, c: char, next_is_quote: bool, delim: char) -> QuoteEvent {
        if self.in_quotes {
            if c == '"' {
                if next_is_quote {
                    return QuoteEvent::EscapedQuote;
                }
                self.in_quotes = false;
                return QuoteEvent::ExitQuotes;
            }
            return QuoteEvent::Data(c);
        }
        if c == '"' && !self.field_started {
            self.in_quotes = true;
            self.field_started = true;
            return QuoteEvent::EnterQuotes;
        }
        if c == delim {
            self.field_started = false;
            return QuoteEvent::FieldEnd;
        }
        self.field_started = true;
        QuoteEvent::Data(c)
    }

    /// Whether the scanner is inside a quoted field — a `Data('\n')` event
    /// in this state is quoted field content, not a record break.
    fn in_quotes(&self) -> bool {
        self.in_quotes
    }

    /// Begin the next record's first field. Only meaningful outside quotes,
    /// which is the only state a record break can occur in.
    fn start_record(&mut self) {
        self.field_started = false;
    }
}

/// Split CSV `text` into records, honoring RFC 4180 quoting across line
/// boundaries (issue #91): a newline inside a quoted field is literal, so
/// one record may span several physical lines. Record boundaries otherwise
/// match `str::lines` (split on `\n`, a trailing `\r` is stripped, a final
/// record without a line ending still counts). The quote rule is
/// [`QuoteScan`], shared with [`split_csv_line`], so a `"` that would not
/// open a quoted field within one line never swallows a record break.
pub(crate) fn split_csv_records(text: &str, delim: char) -> Vec<&str> {
    // A `\n` field terminator would make every line break a field end,
    // collapsing the whole text into one record — the terminator must not
    // double as the record break. Split records on `\n` unconditionally;
    // each record's cells then split on the terminator (a `\n` terminator
    // yields the pre-quote one-row-per-line, single-cell behavior).
    if delim == '\n' {
        return text.lines().collect();
    }
    let mut records = Vec::new();
    let mut start = 0;
    let mut scan = QuoteScan::default();
    let mut iter = text.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        match scan.advance(c, iter.peek().is_some_and(|&(_, next)| next == '"'), delim) {
            QuoteEvent::EscapedQuote => {
                iter.next();
            }
            QuoteEvent::Data('\n') if !scan.in_quotes() => {
                let end = if i > start && text.as_bytes()[i - 1] == b'\r' {
                    i - 1
                } else {
                    i
                };
                records.push(&text[start..end]);
                start = i + 1;
                scan.start_record();
            }
            QuoteEvent::Data(_)
            | QuoteEvent::EnterQuotes
            | QuoteEvent::ExitQuotes
            | QuoteEvent::FieldEnd => {}
        }
    }
    if start < text.len() {
        // The final record without a line ending still counts, and its
        // trailing `\r` is stripped exactly like a broken record's —
        // `str::lines` parity (a CRLF file missing its final `\n` must
        // not load a `\r`-suffixed last cell).
        let end = if text.len() > start && text.as_bytes()[text.len() - 1] == b'\r' {
            text.len() - 1
        } else {
            text.len()
        };
        records.push(&text[start..end]);
    }
    records
}

/// Split one CSV record on `delim`, honoring RFC 4180 double-quoting (issue
/// #91): a field starting with `"` runs to the closing `"` with delimiters
/// and newlines literal inside, and `""` inside a quoted field is one `"`.
/// Records come from [`split_csv_records`], which finds the record breaks
/// with the same [`QuoteScan`] rule. An unterminated quote is preserved
/// as-is (no silent data loss) — the row keeps whatever was read.
/// Leading/trailing spaces outside quotes are kept, matching the unquoted
/// splitter's behavior.
pub(crate) fn split_csv_line(line: &str, delim: char) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut scan = QuoteScan::default();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match scan.advance(c, chars.peek() == Some(&'"'), delim) {
            QuoteEvent::Data(c) => cur.push(c),
            QuoteEvent::EscapedQuote => {
                cur.push('"');
                chars.next();
            }
            QuoteEvent::FieldEnd => cells.push(std::mem::take(&mut cur)),
            QuoteEvent::EnterQuotes | QuoteEvent::ExitQuotes => {}
        }
    }
    cells.push(cur);
    cells
}

/// Resolve the `FIELDTERMINATOR` literal to the split delimiter. The
/// terminator must be exactly one character; a longer or empty string is a
/// type error rather than a silent truncation to its first character. The
/// parser already guarantees a string literal (cypherlite-parser
/// `parse_load_csv`), but not its width — that is a property of the value,
/// so the check stays here at execution time rather than in the front-end's
/// lexical-shape validation.
pub(crate) fn field_terminator_char(field_terminator: Option<&str>) -> Result<char> {
    match field_terminator {
        None => Ok(','),
        Some(s) => {
            let mut chars = s.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Ok(c),
                _ => Err(Error::ty(
                    "LOAD CSV FIELDTERMINATOR must be exactly one character",
                )),
            }
        }
    }
}

/// Zip header names with row cells under the ragged-row width rule
/// (docs/api/cli/spec.md): a row with fewer cells than the header maps the
/// missing names to `null` — Neo4j's `LOAD CSV WITH HEADERS` semantics,
/// adopted deliberately so an absent field is distinguishable from an
/// empty one (this supersedes the earlier empty-string padding) — and
/// extra cells past the header width are dropped, so the row map is keyed
/// by exactly the header names.
fn zip_with_header_nulls<'a>(
    headers: &'a [String],
    cells: &'a [String],
) -> impl Iterator<Item = (&'a String, Value)> + 'a {
    headers.iter().enumerate().map(|(i, key)| {
        let value = match cells.get(i) {
            Some(cell) => Value::String(cell.as_str().into()),
            None => Value::Null,
        };
        (key, value)
    })
}

/// Build the `LOAD CSV` row value for one parsed record: a `Map` keyed by
/// the header row under `WITH HEADERS` (ragged rows follow
/// [`zip_with_header_nulls`]), a `List` of every cell otherwise — list
/// rows keep their natural width, never padded or truncated.
pub(crate) fn record_value(headers: Option<&[String]>, cells: Vec<String>) -> Value {
    match headers {
        Some(headers) => Value::Map(
            zip_with_header_nulls(headers, &cells)
                .map(|(key, value)| (key.clone(), value))
                .collect::<BTreeMap<_, _>>()
                .into(),
        ),
        None => Value::List(
            cells
                .into_iter()
                .map(|c| Value::String(c.into()))
                .collect::<Vec<_>>()
                .into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{field_terminator_char, split_csv_line, split_csv_records};

    #[test]
    fn final_record_without_line_ending_strips_trailing_cr() {
        let rs = split_csv_records("a,b\r\nc,d\r", ',');
        assert_eq!(rs, ["a,b", "c,d"]);
    }

    #[test]
    fn newline_field_terminator_keeps_one_row_per_line() {
        let rs = split_csv_records("1\n2\n3", '\n');
        assert_eq!(rs, ["1", "2", "3"]);
        assert_eq!(split_csv_line("1", '\n'), vec!["1".to_string()]);
    }

    #[test]
    fn quoted_field_with_delimiter_stays_one_cell() {
        let cells = split_csv_line("1,\"Neo4j, Cypher and graphs\",x", ',');
        assert_eq!(cells, vec!["1", "Neo4j, Cypher and graphs", "x"]);
    }

    #[test]
    fn doubled_quote_inside_quoted_field_is_one_quote() {
        let cells = split_csv_line("\"a \"\"quoted\"\" b\",2", ',');
        assert_eq!(cells, vec!["a \"quoted\" b", "2"]);
    }

    #[test]
    fn header_row_is_quote_aware_too() {
        let cells = split_csv_line("id,\"the, header\",n", ',');
        assert_eq!(cells, vec!["id", "the, header", "n"]);
    }

    #[test]
    fn unquoted_lines_are_unchanged() {
        assert_eq!(split_csv_line("a,b,c", ','), vec!["a", "b", "c"]);
        assert_eq!(split_csv_line("a", ','), vec!["a"]);
    }

    #[test]
    fn unterminated_quote_is_preserved_not_dropped() {
        // RFC violation, but the row must not silently lose data.
        let cells = split_csv_line("\"open,ended", ',');
        assert_eq!(cells, vec!["open,ended"]);
    }

    #[test]
    fn quoted_field_with_newline_stays_one_record() {
        let records = split_csv_records("1,\"line one\nline two\",3\n4,four,5\n", ',');
        assert_eq!(records, vec!["1,\"line one\nline two\",3", "4,four,5"]);
        assert_eq!(
            split_csv_line(records[0], ','),
            vec!["1", "line one\nline two", "3"]
        );
    }

    #[test]
    fn quoted_record_break_with_crlf_and_blank_lines() {
        let records = split_csv_records("a,\"b\r\nc\"\r\n\r\n\"d\",e", ',');
        assert_eq!(records, vec!["a,\"b\r\nc\"", "", "\"d\",e"]);
    }

    #[test]
    fn stray_quote_never_swallows_a_record_break() {
        // A `"` not at a field start is literal, so it cannot start a
        // quoted field that eats the following lines.
        let records = split_csv_records("say \"hi\",1\nnext,2\n", ',');
        assert_eq!(records, vec!["say \"hi\",1", "next,2"]);
    }

    #[test]
    fn custom_field_terminator_respects_quotes() {
        let cells = split_csv_line("a\t\"b\tc\"\td", '\t');
        assert_eq!(cells, vec!["a", "b\tc", "d"]);
    }

    #[test]
    fn empty_fields_and_only_quotes() {
        assert_eq!(split_csv_line(",\"\",x", ','), vec!["", "", "x"]);
        assert_eq!(split_csv_line("a,,", ','), vec!["a", "", ""]);
    }

    #[test]
    fn field_terminator_must_be_exactly_one_char() {
        assert_eq!(field_terminator_char(None).unwrap(), ',');
        assert_eq!(field_terminator_char(Some("\t")).unwrap(), '\t');
        for invalid in ["||", ",;", ""] {
            assert!(
                field_terminator_char(Some(invalid)).is_err(),
                "{invalid:?} must be rejected"
            );
        }
    }
}
