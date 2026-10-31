//! Shared Cypher statement scanner.
//!
//! The one canonical implementation of statement splitting for every
//! CypherLite surface that accepts multi-statement input: the CLI shell
//! (`cypherlite-cli`), the TCK runner (`cypherlite-tck`), and script
//! execution in the engine (`cypherlite::run_script_wire`). A single lexical
//! scan backs [`split_statements`], [`split_executable_statements`], and
//! [`input_is_complete`], so they never diverge.

/// Lexical scan state for statement boundaries.
///
/// This mirrors the comment and quoted-literal rules of the parser's lexer
/// (`cypherlite_parser`'s `tokenize`: `//` line comments, `/* … */` block
/// comments, `'…'`/`"…"` strings with backslash escapes, and `` `…` ``
/// backtick-quoted identifiers with no escape processing) so that a `;` inside
/// any of them is never treated as a statement terminator. The lexer itself
/// discards byte spans and comment/whitespace text, so it cannot split the
/// original source in place; this scanner applies the same rules while keeping
/// the source verbatim.
#[derive(Default)]
struct LexState {
    in_single: bool,
    in_double: bool,
    in_backtick: bool,
    in_line_comment: bool,
    in_block_comment: bool,
}

/// A statement the scan terminated.
struct Piece {
    text: String,
    /// Whether it holds top-level content other than whitespace and
    /// comments, i.e. something to execute.
    content: bool,
}

/// Result of scanning an input for statement boundaries.
struct Scan {
    /// Statements terminated by a top-level `;` (or a dot-command newline).
    statements: Vec<Piece>,
    /// Trailing text after the last terminator (may be a partial statement).
    trailing: String,
    /// Final lexical state (an open string/comment at end-of-input).
    state: LexState,
    /// True when the trailing text holds top-level content that is neither
    /// whitespace nor a comment — i.e. an unterminated statement fragment.
    pending: bool,
    /// Byte offset in [`Scan::trailing`] of a statement-initial top-level `.`,
    /// i.e. a dot-command whose newline terminator had not been scanned when
    /// input ended (comment/whitespace text precedes it in `trailing`).
    /// `None` when the trailing fragment does not begin a dot-command.
    trailing_dot_start: Option<usize>,
}

/// Scan `input` into completed statements plus the trailing fragment and final
/// lexical state, honoring string/comment rules (see [`LexState`]). The single
/// scan backs both [`split_statements`] and [`input_is_complete`] so the two
/// never diverge.
///
/// A statement-initial top-level `.` (one seen with nothing pending in the
/// buffer) marks a dot-command: it is line-terminated rather than
/// `;`-terminated, and any comment/whitespace text before it is discarded from
/// the emitted statement.
fn scan(input: &str) -> Scan {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut st = LexState::default();
    let mut pending = false;
    // Offset into `current` of a statement-initial `.` — a dot-command, which
    // is line-terminated rather than `;`-terminated. Reset on every real
    // terminator so state never leaks across statements.
    let mut dot_start: Option<usize> = None;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        // Inside a line comment: everything up to (and including) the newline is
        // comment text; a `;` here is not a terminator.
        if st.in_line_comment {
            current.push(ch);
            if ch == '\n' {
                st.in_line_comment = false;
            }
            continue;
        }
        // Inside a block comment: consume until the closing `*/`.
        if st.in_block_comment {
            current.push(ch);
            if ch == '*' && chars.peek() == Some(&'/') {
                current.push(chars.next().unwrap());
                st.in_block_comment = false;
            }
            continue;
        }
        // Inside a single-quoted string: a backslash escapes the next char (so
        // `\'` does not close and `\\` before `'` still closes — the bug the
        // old prev_char check got wrong).
        if st.in_single {
            current.push(ch);
            if ch == '\\' {
                if let Some(&next) = chars.peek() {
                    current.push(next);
                    chars.next();
                }
            } else if ch == '\'' {
                st.in_single = false;
            }
            continue;
        }
        // Inside a double-quoted string: same escape handling.
        if st.in_double {
            current.push(ch);
            if ch == '\\' {
                if let Some(&next) = chars.peek() {
                    current.push(next);
                    chars.next();
                }
            } else if ch == '"' {
                st.in_double = false;
            }
            continue;
        }
        // Inside a backtick-quoted identifier: no escape processing (matches the
        // lexer's `read_backtick_identifier`); the next backtick closes it.
        if st.in_backtick {
            current.push(ch);
            if ch == '`' {
                st.in_backtick = false;
            }
            continue;
        }

        // Top-level (not inside any string or comment).
        match ch {
            // `//` line comment: consume both slashes so the second one is not
            // reconsidered.
            '/' if chars.peek() == Some(&'/') => {
                current.push(ch);
                current.push(chars.next().unwrap());
                st.in_line_comment = true;
            }
            // `/*` block comment: consume both chars so the opening `*` cannot
            // be mistaken for a closing `*/`.
            '/' if chars.peek() == Some(&'*') => {
                current.push(ch);
                current.push(chars.next().unwrap());
                st.in_block_comment = true;
            }
            '\'' => {
                st.in_single = true;
                pending = true;
                current.push(ch);
            }
            '"' => {
                st.in_double = true;
                pending = true;
                current.push(ch);
            }
            '`' => {
                st.in_backtick = true;
                pending = true;
                current.push(ch);
            }
            ';' => {
                // A dot-command may also be `;`-terminated (`.begin;`); slice
                // from the recorded `.` so `// c\n.begin;` works too.
                let trimmed = current[dot_start.unwrap_or(0)..].trim();
                if !trimmed.is_empty() {
                    statements.push(Piece {
                        text: trimmed.to_string(),
                        content: pending,
                    });
                }
                current.clear();
                pending = false;
                dot_start = None;
            }
            // Dot-commands are line-based, not semicolon-terminated: a newline
            // at a statement-initial `.` ends the command, dropping any
            // comment/whitespace text that preceded it in the buffer.
            '\n' if dot_start.is_some() => {
                let trimmed = current[dot_start.unwrap()..].trim();
                if !trimmed.is_empty() {
                    statements.push(Piece {
                        text: trimmed.to_string(),
                        content: true,
                    });
                }
                current.clear();
                pending = false;
                dot_start = None;
            }
            other => {
                if !other.is_whitespace() {
                    // A top-level `.` with nothing pending records the start of
                    // a dot-command. `pending` already true at the dot means the
                    // buffer holds statement content (e.g. `RETURN n` newline
                    // `.name`), so the dot is ordinary Cypher, not a command.
                    if !pending && other == '.' && dot_start.is_none() {
                        dot_start = Some(current.len());
                    }
                    pending = true;
                }
                current.push(other);
            }
        }
    }

    Scan {
        statements,
        trailing: current,
        state: st,
        pending,
        trailing_dot_start: dot_start,
    }
}

/// Split input into statements on semicolons, respecting quoted strings and
/// comments. A `;` inside a `'…'`/`"…"` string, a `` `…` `` backtick
/// identifier, a `// …` line comment, or a `/* … */` block comment is not a
/// terminator. A dot-command — a statement-initial, top-level `.` line — is
/// split on the newline instead, since it is line-based. The trailing fragment
/// is sliced at a dot-command start, so a script whose last line is a
/// dot-command with no trailing newline still terminates that command.
pub fn split_statements(input: &str) -> Vec<String> {
    pieces(input).into_iter().map(|p| p.text).collect()
}

/// [`split_statements`], without the pieces that hold nothing to execute.
///
/// The scanner keeps comment text verbatim, so a comment between two `;`,
/// or after the last one, is a piece of its own. Hosts that execute each
/// statement (script execution in the engine) take only the pieces with
/// content; blank or comment-only input yields no statements.
pub fn split_executable_statements(input: &str) -> Vec<String> {
    pieces(input)
        .into_iter()
        .filter(|p| p.content)
        .map(|p| p.text)
        .collect()
}

fn pieces(input: &str) -> Vec<Piece> {
    let result = scan(input);
    let mut statements = result.statements;
    // Slice at a recorded statement-initial `.` (dropping any preceding
    // comment/whitespace text); otherwise trim the fragment as before.
    let trailing = match result.trailing_dot_start {
        Some(start) => result.trailing[start..].trim(),
        None => result.trailing.trim(),
    };
    if !trailing.is_empty() {
        statements.push(Piece {
            text: trailing.to_string(),
            content: result.pending || result.trailing_dot_start.is_some(),
        });
    }
    statements
}

/// Whether `input` is a complete statement ready to execute.
///
/// Used by line-based hosts (the CLI's interactive prompt loop) to decide
/// whether pressing Enter submits the buffer or continues a multi-line
/// statement. Input is complete when it is empty, a
/// single-line dot-command, or a Cypher statement (or statements) terminated by
/// a top-level `;` with no open string/comment and nothing pending afterward.
/// A buffer ending in a statement-initial dot-command (e.g. a comment line then
/// `.begin`) is also complete — the command's newline terminator is implied at
/// end-of-input. It is incomplete while a string or block comment is still
/// open, or while there is unterminated statement content (no trailing top-level
/// `;`).
pub fn input_is_complete(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return true;
    }
    // Dot-commands are single-line; Enter submits them immediately.
    if trimmed.starts_with('.') {
        return true;
    }
    let result = scan(input);
    // An open string or block comment means the user is mid-token: keep
    // prompting. (A line comment implicitly ends at end-of-input, so an open
    // `in_line_comment` does not block completion.) This must be checked before
    // the trailing-dot-command rule below, so `RETURN 1 /* open\n.begin` stays
    // incomplete.
    if result.state.in_single
        || result.state.in_double
        || result.state.in_backtick
        || result.state.in_block_comment
    {
        return false;
    }
    // Complete only once the final statement has been terminated (nothing but
    // whitespace/comments trails the last top-level `;`) — or the trailing
    // fragment is a statement-initial dot-command, which is line-based and
    // complete on its own once its first line is entered.
    !result.pending || result.trailing_dot_start.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_statements_drop_comment_only_pieces() {
        let exec = |s: &str| split_executable_statements(s);
        assert!(exec("").is_empty());
        assert!(exec("  \n ").is_empty());
        assert!(exec("// just a note").is_empty());
        assert!(exec("/* a */ // b\n  /* c */").is_empty());
        assert!(exec("// a;\n/* b; */ ;").is_empty());
        assert_eq!(exec("// load\nRETURN 1"), ["// load\nRETURN 1"]);
        assert_eq!(
            exec("/* x */ MATCH (n) RETURN n"),
            ["/* x */ MATCH (n) RETURN n"]
        );
        assert_eq!(exec("RETURN 1; // trailing note"), ["RETURN 1"]);
        assert_eq!(
            exec("RETURN 1; /* between */; RETURN ';'"),
            ["RETURN 1", "RETURN ';'"]
        );
        assert_eq!(exec("RETURN '' ;"), ["RETURN ''"]);
        // split_statements keeps the comment-only pieces.
        assert_eq!(
            split_statements("RETURN 1; // trailing note"),
            ["RETURN 1", "// trailing note"]
        );
    }

    #[test]
    fn test_split_statements() {
        let stmts = split_statements("CREATE (:Person); MATCH (n) RETURN n;");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "CREATE (:Person)");
        assert_eq!(stmts[1], "MATCH (n) RETURN n");
    }

    #[test]
    fn test_split_statements_with_quotes() {
        let stmts = split_statements("CREATE (:Person {name: 'hello;world'}); RETURN 1;");
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("hello;world"));
    }

    #[test]
    fn test_split_statements_trailing() {
        let stmts = split_statements("RETURN 1");
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "RETURN 1");
    }

    #[test]
    fn test_split_statements_semicolon_in_line_comment_not_split() {
        // The ';' inside the `//` comment must not split the statement.
        let stmts = split_statements("RETURN 1 // a ; b\n;\nRETURN 2;");
        assert_eq!(stmts.len(), 2, "got {stmts:?}");
        assert!(stmts[0].contains("RETURN 1"));
        assert!(stmts[1].contains("RETURN 2"));
    }

    #[test]
    fn test_split_statements_semicolon_in_block_comment_not_split() {
        // Every ';' inside the `/* … */` block comment must be ignored.
        let stmts = split_statements("RETURN 1 /* a ; b ; c */ ; RETURN 2;");
        assert_eq!(stmts.len(), 2, "got {stmts:?}");
        assert!(stmts[0].contains("RETURN 1"));
        assert!(stmts[1].contains("RETURN 2"));
    }

    #[test]
    fn test_split_statements_semicolon_in_double_quote_not_split() {
        let stmts = split_statements("RETURN \"a;b\" AS s; RETURN 2;");
        assert_eq!(stmts.len(), 2, "got {stmts:?}");
        assert!(stmts[0].contains("a;b"));
    }

    #[test]
    fn test_split_statements_semicolon_in_backtick_not_split() {
        // Backtick-quoted identifiers can hold a ';' without splitting.
        let stmts = split_statements("MATCH (n) RETURN n.`odd;key` AS k; RETURN 2;");
        assert_eq!(stmts.len(), 2, "got {stmts:?}");
        assert!(stmts[0].contains("odd;key"));
    }

    #[test]
    fn test_split_statements_escaped_backslash_before_quote() {
        // The literal string is 'a\\' — one backslash then the closing quote.
        // The old prev_char == '\\' check mis-read the closing quote as escaped,
        // swallowing the following ';'. It must split into two statements.
        let stmts = split_statements(r"CREATE (:X {p: 'a\\'}); RETURN 1;");
        assert_eq!(stmts.len(), 2, "got {stmts:?}");
        assert!(stmts[1].contains("RETURN 1"));
    }

    #[test]
    fn test_split_statements_unterminated_block_comment_kept_whole() {
        // `/*/` is NOT a closed block comment (the opening `*` is not a closer);
        // the whole thing stays one (unterminated) fragment.
        let stmts = split_statements("RETURN 1 /*/ RETURN 2");
        assert_eq!(stmts.len(), 1, "got {stmts:?}");
    }

    #[test]
    fn test_input_is_complete_basic() {
        assert!(input_is_complete("RETURN 1;"));
        assert!(!input_is_complete("MATCH (n)"));
        assert!(input_is_complete(".help"));
        assert!(input_is_complete(""));
        assert!(input_is_complete("   "));
    }

    #[test]
    fn test_input_is_complete_unterminated_string_multiline() {
        // A multi-line unterminated string is detected incomplete.
        assert!(!input_is_complete("RETURN 'abc"));
        assert!(!input_is_complete("RETURN 'abc\nstill open"));
        assert!(input_is_complete("RETURN 'closed';"));
    }

    #[test]
    fn test_input_is_complete_semicolon_in_string_or_comment() {
        // A ';' inside a string does not complete the statement.
        assert!(!input_is_complete("RETURN 'a;b'"));
        // A ';' inside a line comment does not complete either.
        assert!(!input_is_complete("RETURN 1 // done;"));
        // An open block comment is incomplete; a closed one with a trailing ';'
        // is complete.
        assert!(!input_is_complete("RETURN 1 /* open"));
        assert!(input_is_complete("RETURN 1 /* closed */;"));
    }

    #[test]
    fn test_input_is_complete_multi_statement_last_pending() {
        // First statement terminated, second still open -> incomplete.
        assert!(!input_is_complete("CREATE (:A); CREATE (:B"));
        assert!(input_is_complete("CREATE (:A); CREATE (:B);"));
    }

    #[test]
    fn test_split_statements_comment_before_dot_command() {
        // Issue #7 regression: a `//` comment before a dot-command used to get
        // glued to it, so `.begin` was swallowed into the next Cypher statement
        // and surfaced as "parse error: unexpected token \".\"". The comment
        // line must be discarded and the dot-command split out on its own.
        let stmts = split_statements("// header\n.begin\nCREATE (:X);\n.commit\n");
        assert_eq!(
            stmts,
            vec![".begin", "CREATE (:X)", ".commit"],
            "got {stmts:?}"
        );

        // A blank line between the comment and the command must not change that
        // (whitespace never counts as pending, so the dot still starts fresh).
        let stmts = split_statements("// c\n\n.begin\n");
        assert_eq!(stmts, vec![".begin"], "got {stmts:?}");
    }

    #[test]
    fn test_split_statements_block_comment_same_line_dot() {
        // A block comment on the same line before `.begin` is comment text (not
        // statement content), so the dot-command is still detected and the
        // comment dropped from the emitted statement.
        let stmts = split_statements("/* c */ .begin\n");
        assert_eq!(stmts, vec![".begin"], "got {stmts:?}");
    }

    #[test]
    fn test_split_statements_dot_command_no_trailing_newline() {
        // A script whose final line is a dot-command with no trailing newline
        // still terminates: `trailing_dot_start` slices the trailing fragment
        // at the statement-initial dot. Both with preceding comment text and
        // bare.
        let stmts = split_statements("// c\n.begin");
        assert_eq!(stmts, vec![".begin"], "got {stmts:?}");
        let stmts = split_statements(".begin");
        assert_eq!(stmts, vec![".begin"], "got {stmts:?}");
    }

    #[test]
    fn test_split_statements_dot_continuation_after_pending_cypher() {
        // REGRESSION GUARD: a property access wrapped onto its own line
        // (`RETURN n\n.name`) must NOT be mis-split into a bogus `.name`
        // dot-command. The dot starts a dot-command only when nothing is
        // pending — a real statement boundary — so checking just that the
        // last line starts with '.' (the naive fix) would break this.
        let stmts = split_statements("RETURN n\n.name;");
        assert_eq!(stmts, vec!["RETURN n\n.name"], "got {stmts:?}");
        assert!(!input_is_complete("RETURN n\n.name"));
    }

    #[test]
    fn test_split_statements_dot_command_with_semicolon_argument() {
        // PRE-EXISTING LIMITATION, pinned so a future change can't silently
        // alter it: a `;` inside a dot-command argument is treated as a
        // statement terminator (dot-commands may be `;`-terminated, e.g.
        // `.begin;`), so `.read a;b.cypher` splits into two. Deliberately
        // NOT fixed here.
        let stmts = split_statements(".read a;b.cypher\n");
        assert_eq!(stmts, vec![".read a", "b.cypher"], "got {stmts:?}");
    }

    #[test]
    fn test_input_is_complete_comment_then_dot_command() {
        // A comment line then a dot-command is complete even with no trailing
        // newline: the command's newline terminator is implied at end-of-input.
        assert!(input_is_complete("// header\n.begin"));
        // ...but the trailing-dot-command rule must be checked AFTER the open
        // block comment: `RETURN 1 /* open\n.begin` has an open comment, so it
        // must stay incomplete rather than completing on the trailing `.begin`.
        assert!(!input_is_complete("RETURN 1 /* open\n.begin"));
    }
}
