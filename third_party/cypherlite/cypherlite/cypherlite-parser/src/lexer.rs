use crate::token::{lookup_keyword, Token, TokenType};
use crate::unicode_identifier;

/// Tokenizes a Cypher query string into a list of tokens.
pub fn tokenize(input: &str) -> Vec<Token> {
    let mut lexer = Lexer::new(input);
    lexer.tokenize();
    lexer.tokens
}

struct Lexer<'a> {
    input: &'a [u8],
    pos: usize,
    line: usize,
    col: usize,
    tokens: Vec<Token>,
}

impl<'a> Lexer<'a> {
    fn new(input: &'a str) -> Self {
        Lexer {
            input: input.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
            tokens: Vec::new(),
        }
    }

    fn tokenize(&mut self) {
        while self.pos < self.input.len() {
            self.skip_whitespace();
            if self.pos >= self.input.len() {
                break;
            }

            let ch = self.peek();

            // Skip line comments
            if ch == b'/' && self.peek_at(1) == Some(b'/') {
                self.skip_line_comment();
                continue;
            }
            // Skip block comments
            if ch == b'/' && self.peek_at(1) == Some(b'*') {
                self.skip_block_comment();
                continue;
            }

            let line = self.line;
            let col = self.col;

            match ch {
                b'(' => {
                    self.advance();
                    self.emit(TokenType::LeftParen, "(", line, col);
                }
                b')' => {
                    self.advance();
                    self.emit(TokenType::RightParen, ")", line, col);
                }
                b'[' => {
                    self.advance();
                    self.emit(TokenType::LeftBracket, "[", line, col);
                }
                b']' => {
                    self.advance();
                    self.emit(TokenType::RightBracket, "]", line, col);
                }
                b'{' => {
                    self.advance();
                    self.emit(TokenType::LeftBrace, "{", line, col);
                }
                b'}' => {
                    self.advance();
                    self.emit(TokenType::RightBrace, "}", line, col);
                }
                b':' => {
                    if self.peek_at(1) == Some(b':') {
                        self.advance();
                        self.advance();
                        self.emit(TokenType::DoubleColon, "::", line, col);
                    } else {
                        self.advance();
                        self.emit(TokenType::Colon, ":", line, col);
                    }
                }
                b',' => {
                    self.advance();
                    self.emit(TokenType::Comma, ",", line, col);
                }
                b';' => {
                    self.advance();
                    self.emit(TokenType::Semicolon, ";", line, col);
                }
                b'|' => {
                    if self.peek_at(1) == Some(b'|') {
                        self.advance();
                        self.advance();
                        self.emit(TokenType::DoublePipe, "||", line, col);
                    } else {
                        self.advance();
                        self.emit(TokenType::Pipe, "|", line, col);
                    }
                }
                b'*' => {
                    self.advance();
                    self.emit(TokenType::Star, "*", line, col);
                }
                b'+' => {
                    self.advance();
                    self.emit(TokenType::Plus, "+", line, col);
                }
                b'/' => {
                    self.advance();
                    self.emit(TokenType::Slash, "/", line, col);
                }
                b'%' => {
                    self.advance();
                    self.emit(TokenType::Percent, "%", line, col);
                }
                b'^' => {
                    self.advance();
                    self.emit(TokenType::Caret, "^", line, col);
                }
                b'=' => {
                    if self.peek_at(1) == Some(b'~') {
                        self.advance();
                        self.advance();
                        self.emit(TokenType::RegexMatch, "=~", line, col);
                    } else if self.peek_at(1) == Some(b'>') {
                        self.advance();
                        self.advance();
                        self.emit(TokenType::FatArrow, "=>", line, col);
                    } else {
                        self.advance();
                        self.emit(TokenType::Equals, "=", line, col);
                    }
                }
                b'.' => {
                    if self.peek_at(1) == Some(b'.') {
                        self.advance();
                        self.advance();
                        self.emit(TokenType::DotDot, "..", line, col);
                    } else if matches!(self.peek_at(1), Some(b'0'..=b'9')) {
                        self.read_number(line, col);
                    } else {
                        self.advance();
                        self.emit(TokenType::Dot, ".", line, col);
                    }
                }
                b'-' => {
                    if self.peek_at(1) == Some(b'>') {
                        self.advance();
                        self.advance();
                        self.emit(TokenType::ArrowRight, "->", line, col);
                    } else {
                        self.advance();
                        self.emit(TokenType::Dash, "-", line, col);
                    }
                }
                b'<' => {
                    if self.peek_at(1) == Some(b'-') {
                        self.advance();
                        self.advance();
                        self.emit(TokenType::ArrowLeft, "<-", line, col);
                    } else if self.peek_at(1) == Some(b'>') {
                        self.advance();
                        self.advance();
                        self.emit(TokenType::NotEquals, "<>", line, col);
                    } else if self.peek_at(1) == Some(b'=') {
                        self.advance();
                        self.advance();
                        self.emit(TokenType::LessEqual, "<=", line, col);
                    } else {
                        self.advance();
                        self.emit(TokenType::LessThan, "<", line, col);
                    }
                }
                b'>' => {
                    if self.peek_at(1) == Some(b'=') {
                        self.advance();
                        self.advance();
                        self.emit(TokenType::GreaterEqual, ">=", line, col);
                    } else {
                        self.advance();
                        self.emit(TokenType::GreaterThan, ">", line, col);
                    }
                }
                b'$' => {
                    self.advance();
                    self.read_parameter(line, col);
                }
                b'!' => {
                    self.advance();
                    self.emit(TokenType::Bang, "!", line, col);
                }
                b'&' => {
                    self.advance();
                    self.emit(TokenType::Ampersand, "&", line, col);
                }
                b'\'' | b'"' => {
                    self.read_string(line, col);
                }
                b'`' => {
                    self.read_backtick_identifier(line, col);
                }
                b'0'..=b'9' => {
                    self.read_number(line, col);
                }
                // The remaining bytes open either an identifier or one
                // invalid character; the decoded-character decision lives in
                // `at_ident_start`, so `read_identifier` has exactly this one
                // call site (ADR-0182).
                _ if self.at_ident_start() => {
                    self.read_identifier(line, col);
                }
                _ => {
                    // A decoded non-ASCII character that is not an identifier
                    // start is ONE invalid Unicode character, not one mojibake
                    // error per UTF-8 byte (LABS-188).
                    self.read_invalid_char(line, col);
                }
            }
        }
    }

    fn peek(&self) -> u8 {
        if self.pos >= self.input.len() {
            0
        } else {
            self.input[self.pos]
        }
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        let p = self.pos + offset;
        if p >= self.input.len() {
            None
        } else {
            Some(self.input[p])
        }
    }

    fn advance(&mut self) -> u8 {
        if self.pos >= self.input.len() {
            return 0;
        }
        let ch = self.input[self.pos];
        self.pos += 1;
        if ch == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        ch
    }

    /// Decode the character at the cursor as `(char, byte_len)` without
    /// consuming it; `None` only on malformed UTF-8. ASCII is decided on the
    /// byte; a byte >= 0x80 decodes its full sequence (via [`decode_char`]).
    fn peek_char(&self) -> Option<(char, usize)> {
        decode_char(&self.input[self.pos..])
    }

    /// Consume `len` bytes — the byte length [`Lexer::peek_char`] reported for
    /// the character at the cursor — so a multi-byte character is never split.
    fn advance_by(&mut self, len: usize) {
        for _ in 0..len {
            self.advance();
        }
    }

    fn emit(&mut self, ty: TokenType, literal: &str, line: usize, col: usize) {
        self.tokens
            .push(Token::new(ty, literal.to_string(), line, col));
    }

    /// Emit a [`TokenType::Error`] token carrying a symbolic openCypher-TCK
    /// error-*detail* name (see [`Token::detail`]). The parser propagates the
    /// detail onto the raised `ParseError` so a malformed literal reports its
    /// specific TCK detail (`InvalidNumberLiteral`, `InvalidUnicodeLiteral`)
    /// instead of the generic `UnexpectedSyntax`.
    fn emit_error_detail(&mut self, literal: &str, line: usize, col: usize, detail: &'static str) {
        self.tokens.push(Token::error_with_detail(
            literal.to_string(),
            line,
            col,
            detail,
        ));
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.input.len() {
            let ch = self.input[self.pos];
            // ASCII whitespace (CIP-88 adds vertical tab and form feed to
            // the previous \t \n \r + space set; verified on 2026.09.0).
            if matches!(ch, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
                self.advance();
                continue;
            }
            // CIP-88: the GQL/Java Unicode whitespace set (Zs + Zl + Zp
            // categories plus separators, i.e. Rust's char::is_whitespace)
            // is also accepted between tokens — e.g. NBSP U+00A0. Verified
            // against Neo4j 2026.09.0.
            if let Some((c, len)) = self.peek_char() {
                if c.is_whitespace() {
                    self.advance_by(len);
                    continue;
                }
            }
            break;
        }
    }

    fn skip_line_comment(&mut self) {
        self.advance(); // /
        self.advance(); // /
        while self.pos < self.input.len() && self.input[self.pos] != b'\n' {
            self.advance();
        }
    }

    fn skip_block_comment(&mut self) {
        let line = self.line;
        let col = self.col;
        self.advance(); // /
        self.advance(); // *
        while self.pos < self.input.len() {
            if self.input[self.pos] == b'*' && self.peek_at(1) == Some(b'/') {
                self.advance();
                self.advance();
                return;
            }
            self.advance();
        }
        // An unterminated block comment must not silently swallow the rest of
        // the input (which could hide trailing statements such as
        // `RETURN 1 /* DETACH DELETE n`); report a lex error instead. Block
        // comments remain non-nesting.
        self.emit(TokenType::Error, "unterminated block comment", line, col);
    }

    fn read_string(&mut self, line: usize, col: usize) {
        let quote = self.advance();
        let mut s = String::new();
        while self.pos < self.input.len() {
            let ch = self.advance();
            if ch == b'\\' && self.pos < self.input.len() {
                let next = self.advance();
                if self.push_escape(&mut s, next, line, col).is_err() {
                    return;
                }
            } else if ch == quote {
                self.emit(TokenType::String, &s, line, col);
                return;
            } else {
                push_utf8(self, &mut s, ch);
            }
        }
        self.emit(TokenType::Error, "unterminated string", line, col);
    }

    /// Resolve the byte following a `\` in a plain string body and push its
    /// decoded form onto `s`. Used by [`read_string`]; an unknown escape
    /// (including `\{`/`\}`) stays as the literal two-character form. Returns
    /// `Err(())` after emitting an `InvalidUnicodeLiteral` error, which the
    /// caller must treat as a stop.
    ///
    /// [`read_interpolation`] deliberately does *not* use this: it keeps every
    /// escape unresolved so the interpolation parser can distinguish `\\` + `{`
    /// (escaped backslash, then an embed) from `\{` (escaped literal brace).
    fn push_escape(
        &mut self,
        s: &mut String,
        escaped: u8,
        line: usize,
        col: usize,
    ) -> Result<(), ()> {
        match escaped {
            b'n' => s.push('\n'),
            b't' => s.push('\t'),
            b'\\' => s.push('\\'),
            b'\'' => s.push('\''),
            b'"' => s.push('"'),
            b'u' => {
                if self.pos + 4 > self.input.len() {
                    self.emit_error_detail(
                        "invalid unicode escape sequence",
                        line,
                        col,
                        "InvalidUnicodeLiteral",
                    );
                    return Err(());
                }
                let hex = &self.input[self.pos..self.pos + 4];
                let hex_str = std::str::from_utf8(hex).unwrap_or("");
                match u32::from_str_radix(hex_str, 16) {
                    Ok(cp) => {
                        // Surrogates (U+D800..=U+DFFF) and codepoints above
                        // U+10FFFF are not valid `char`s; dropping them would
                        // silently corrupt the string (main's #72 hardening).
                        match char::from_u32(cp) {
                            Some(ch) => {
                                s.push(ch);
                                for _ in 0..4 {
                                    self.advance();
                                }
                            }
                            None => {
                                self.emit_error_detail(
                                    &format!("invalid unicode escape sequence: \\u{hex_str}"),
                                    line,
                                    col,
                                    "InvalidUnicodeLiteral",
                                );
                                return Err(());
                            }
                        }
                    }
                    Err(_) => {
                        self.emit_error_detail(
                            &format!("invalid unicode escape sequence: \\u{hex_str}"),
                            line,
                            col,
                            "InvalidUnicodeLiteral",
                        );
                        return Err(());
                    }
                }
            }
            other => {
                s.push('\\');
                // An escaped non-ASCII byte must survive byte-identically,
                // not be byte-widened into mojibake (decode the UTF-8
                // sequence like read_string's fallback did).
                push_utf8(self, s, other);
            }
        }
        Ok(())
    }

    fn read_backtick_identifier(&mut self, line: usize, col: usize) {
        self.advance(); // skip opening backtick
        let mut s = String::new();
        while self.pos < self.input.len() {
            let ch = self.advance();
            if ch == b'`' {
                // A doubled backtick inside a backtick-quoted identifier is an
                // escaped literal backtick, mirroring the formatter's
                // `cypher_identifier` (which doubles every embedded backtick).
                // Peek the next byte: another backtick means "one literal
                // backtick, keep reading"; anything else terminates the
                // identifier. Without this the `.dump` / `.mode cypher` output
                // for a label/type/key containing a backtick would not re-parse.
                if self.pos < self.input.len() && self.input[self.pos] == b'`' {
                    self.advance(); // consume the second backtick of the pair
                    s.push('`');
                    continue;
                }
                self.emit(TokenType::Identifier, &s, line, col);
                return;
            }
            push_utf8(self, &mut s, ch);
        }
        self.emit(
            TokenType::Error,
            "unterminated backtick identifier",
            line,
            col,
        );
    }

    fn read_number(&mut self, line: usize, col: usize) {
        let start = self.pos;
        let mut is_float = false;

        // Check for hex or octal prefix
        if self.pos < self.input.len() && self.input[self.pos] == b'0' {
            if let Some(next) = self.peek_at(1) {
                if next == b'x' || next == b'X' {
                    self.advance(); // 0
                    self.advance(); // x
                    let body_start = self.pos;
                    while self.pos < self.input.len() && self.input[self.pos].is_ascii_hexdigit() {
                        self.advance();
                    }
                    // An empty body (`0x`, Literals3 [12]) or trailing
                    // identifier characters (`0x1A2b3j...`, a non-hex letter
                    // continuing the run — Literals3 [13]/[14]) makes this an
                    // invalid number literal, not a valid hex integer.
                    if self.pos == body_start || self.number_has_trailing_ident() {
                        self.emit_malformed_number(start, line, col);
                        return;
                    }
                    let literal = std::str::from_utf8(&self.input[start..self.pos]).unwrap_or("");
                    self.emit(TokenType::Integer, literal, line, col);
                    return;
                }
                if next == b'o' || next == b'O' {
                    self.advance(); // 0
                    self.advance(); // o
                    let body_start = self.pos;
                    while self.pos < self.input.len()
                        && self.input[self.pos] >= b'0'
                        && self.input[self.pos] <= b'7'
                    {
                        self.advance();
                    }
                    if self.pos == body_start || self.number_has_trailing_ident() {
                        self.emit_malformed_number(start, line, col);
                        return;
                    }
                    let literal = std::str::from_utf8(&self.input[start..self.pos]).unwrap_or("");
                    self.emit(TokenType::Integer, literal, line, col);
                    return;
                }
            }
        }

        while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
            self.advance();
        }

        if self.pos < self.input.len()
            && self.input[self.pos] == b'.'
            && self.peek_at(1) != Some(b'.')
            && self.pos + 1 < self.input.len()
            && self.input[self.pos + 1].is_ascii_digit()
        {
            is_float = true;
            self.advance(); // skip dot
            while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
                self.advance();
            }
        }

        // Scientific notation
        if self.pos < self.input.len()
            && (self.input[self.pos] == b'e' || self.input[self.pos] == b'E')
        {
            is_float = true;
            self.advance();
            if self.pos < self.input.len()
                && (self.input[self.pos] == b'+' || self.input[self.pos] == b'-')
            {
                self.advance();
            }
            while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
                self.advance();
            }
        }

        // A numeric literal immediately followed by identifier characters
        // (`9223372h54775808`, Literals2 [11]) is a malformed number literal —
        // Cypher requires whitespace before any following identifier/keyword.
        if self.number_has_trailing_ident() {
            self.emit_malformed_number(start, line, col);
            return;
        }

        let literal = std::str::from_utf8(&self.input[start..self.pos]).unwrap_or("");
        if is_float {
            self.emit(TokenType::Float, literal, line, col);
        } else {
            self.emit(TokenType::Integer, literal, line, col);
        }
    }

    /// True when the character immediately after a just-scanned number is an
    /// identifier-start character (letter or `_`) — i.e. the numeric run is
    /// glued to an identifier with no separating whitespace, which is never a
    /// valid Cypher number literal. The character is decoded so a non-ASCII
    /// identifier start (e.g. `é` in `RETURN 1é`) counts too (ADR-0182).
    fn number_has_trailing_ident(&self) -> bool {
        self.at_ident_start()
    }

    /// Consume any trailing identifier characters that are glued to a malformed
    /// number so the whole run becomes a single error token, then emit it tagged
    /// with the `InvalidNumberLiteral` TCK detail.
    fn emit_malformed_number(&mut self, start: usize, line: usize, col: usize) {
        let literal = self.take_ident_run(start);
        self.emit_error_detail(
            &format!("invalid number literal: {literal}"),
            line,
            col,
            "InvalidNumberLiteral",
        );
    }

    /// True when the character at the cursor is an identifier start: an ASCII
    /// `[A-Za-z_]` byte or a non-ASCII `char` accepted by the hand-written
    /// Unicode predicate (ADR-0182). ASCII is decided on the byte; only a
    /// byte >= 0x80 is decoded.
    fn at_ident_start(&self) -> bool {
        self.peek_char()
            .is_some_and(|(c, _)| unicode_identifier::is_identifier_start(c))
    }

    /// Consume the remaining identifier-part run from `start` and return it
    /// as the run's literal — the one canonical consume-and-derive for
    /// identifier runs, consulted by `emit_malformed_number`,
    /// `read_parameter`'s pass-through, and its part-only-start error block
    /// (which differ only in message and detail tag).
    fn take_ident_run(&mut self, start: usize) -> String {
        while self.take_ident_part() {}
        std::str::from_utf8(&self.input[start..self.pos])
            .expect("take_ident_part consumes only decoded characters")
            .to_string()
    }

    /// If the character at the current position is an identifier part,
    /// consume it — all of its UTF-8 bytes — and return true. The character
    /// is decoded through [`Lexer::peek_char`] (ASCII on the byte; a byte at
    /// or above 0x80 as its full sequence) and classified with the
    /// hand-written Unicode predicate, so a multi-byte character is never
    /// split (ADR-0182).
    fn take_ident_part(&mut self) -> bool {
        let Some((c, len)) = self.peek_char() else {
            return false;
        };
        if !unicode_identifier::is_identifier_part(c) {
            return false;
        }
        self.advance_by(len);
        true
    }

    /// Consume and report one character that opens neither a token nor an
    /// identifier: a decoded non-ASCII character is ONE
    /// `InvalidUnicodeCharacter` error token covering all of its UTF-8 bytes
    /// (LABS-188); an ASCII byte keeps the historical untagged
    /// `unexpected character:` error.
    fn read_invalid_char(&mut self, line: usize, col: usize) {
        if let Some((c, len)) = self.peek_char() {
            if !c.is_ascii() {
                self.advance_by(len);
                self.emit_error_detail(
                    &format!("invalid unicode character: {c}"),
                    line,
                    col,
                    "InvalidUnicodeCharacter",
                );
                return;
            }
        }
        // An ASCII byte and a malformed byte take the same path: one byte
        // consumed, one untagged `unexpected character:` token. For ASCII the
        // decoded `char` and `char::from(byte)` are the same thing; malformed
        // UTF-8 cannot occur in `&str` input — every cursor sits on a complete
        // sequence — but keep the historical byte-wise report for the
        // defensive branch.
        let byte = self.advance();
        self.emit(
            TokenType::Error,
            &format!("unexpected character: {}", char::from(byte)),
            line,
            col,
        );
    }

    fn read_identifier(&mut self, line: usize, col: usize) {
        let start = self.pos;
        let literal = self.take_ident_run(start);
        // CIP-261: a lone `s`/`S` immediately followed by a quote starts an
        // interpolated string literal (`s'...'` / `s"..."`). Anything else
        // stays a normal identifier/keyword.
        if literal.len() == 1
            && literal.as_bytes()[0].eq_ignore_ascii_case(&b's')
            && matches!(self.peek(), b'\'' | b'"')
        {
            self.read_interpolation(line, col);
            return;
        }
        let tok_type = lookup_keyword(&literal);
        self.emit(tok_type, &literal, line, col);
    }

    /// Read a CIP-261 interpolated string literal: the opening `s` and quote
    /// are already consumed positioning-wise (`read_identifier` detected the
    /// shape and the quote is the next byte). Emits one
    /// [`TokenType::InterpString`] token whose payload is the raw body:
    /// unescaped `{`/`}` preserved as expression delimiters and *every* escape
    /// (`\n`, `\\`, `\{`, …) passed through as its original two characters --
    /// resolved later by the interpolation parser. Keeping escapes unresolved
    /// is what lets the parser tell `\\` + `{` (escaped backslash, then an
    /// embed) from `\{` (escaped literal brace); a lexer-resolved `\\` would
    /// collapse to the same byte a resolved `\{` starts with. Inside a
    /// `{ ... }` embed the raw form also makes the re-lexed expression see the
    /// original text (an embed like `{f('a\'b')}` would otherwise be
    /// corrupted).
    fn read_interpolation(&mut self, line: usize, col: usize) {
        let quote = self.advance();
        let mut s = String::new();
        // Brace depth and inner-string state: inside `{ ... }` an embedded
        // string -- or a backtick-quoted identifier -- may itself contain
        // braces or the outer quote character.
        let mut brace_depth = 0usize;
        let mut inner_quote: Option<u8> = None;
        let mut in_backtick = false;
        while self.pos < self.input.len() {
            let ch = self.advance();
            if ch == b'\\' && self.pos < self.input.len() {
                // Pass the escape through raw, in both text and embed regions.
                let next = self.advance();
                s.push('\\');
                push_utf8(self, &mut s, next);
            } else if let Some(q) = inner_quote {
                if ch == q {
                    inner_quote = None;
                }
                push_utf8(self, &mut s, ch);
            } else if in_backtick {
                if ch == b'`' {
                    in_backtick = false;
                }
                push_utf8(self, &mut s, ch);
            } else if brace_depth > 0 {
                match ch {
                    b'{' => brace_depth += 1,
                    b'}' => brace_depth -= 1,
                    b'\'' | b'"' => inner_quote = Some(ch),
                    b'`' => in_backtick = true,
                    _ => {}
                }
                push_utf8(self, &mut s, ch);
            } else if ch == b'{' {
                brace_depth += 1;
                s.push('{');
            } else if ch == quote {
                self.emit(TokenType::InterpString, &s, line, col);
                return;
            } else {
                push_utf8(self, &mut s, ch);
            }
        }
        self.emit(TokenType::Error, "unterminated string", line, col);
    }

    fn read_parameter(&mut self, line: usize, col: usize) {
        // A parameter's first character is an identifier start or an ASCII
        // digit — ADR-0182's Consequences prose, confirmed against the live
        // Neo4j reference (LABS-209): a part-only character such as `€` is
        // rejected with the reference's error shape; an arbitrary name (spaces,
        // part-only characters) is spelled through the backtick form
        // `$`name``, which this lexer does not offer yet (LABS-209's
        // acceptance criteria).
        // `(` after `$` is the dynamic-expression opener (`$(expr)`,
        // e.g. `n IS LABELED $('A')`): it lexes as an empty parameter token
        // whose expansion the parser owns, so it stays on the pass-through
        // path below.
        let start_ok = self.at_ident_start()
            || self
                .peek_char()
                .is_some_and(|(c, _)| c.is_ascii_digit() || c == '(');
        // One consume-and-derive for both outcomes: the accepted name and
        // the rejected run are the same identifier-part run from the same
        // position; the guard above only classifies its first character.
        let start = self.pos;
        let run = self.take_ident_run(start);
        if !start_ok {
            // The offending run becomes one error token, not a cascade (the
            // `emit_malformed_number` shape).
            let literal = if run.is_empty() {
                "Invalid input after '$': expected an identifier or an integer value".to_string()
            } else {
                format!("Invalid input '{run}': expected an identifier or an integer value")
            };
            self.emit_error_detail(&literal, line, col, "InvalidParameterName");
            return;
        }
        self.emit(TokenType::Parameter, &run, line, col);
    }
}

/// Decode the first UTF-8 character of `bytes` as `(char, byte_len)`, or
/// `None` on malformed/truncated input.
///
/// Validation is bounded to the single 2–4 byte sequence being decoded,
/// never the whole remaining slice: `skip_whitespace` calls this at every
/// token boundary, so validating the tail here would make lexing quadratic on
/// input heavy in multi-byte whitespace.
fn decode_char(bytes: &[u8]) -> Option<(char, usize)> {
    let first = *bytes.first()?;
    if first < 0x80 {
        return Some((first as char, 1));
    }
    let seq_len = if first >= 0xF0 {
        4
    } else if first >= 0xE0 {
        3
    } else {
        2
    };
    let end = seq_len.min(bytes.len());
    let s = std::str::from_utf8(&bytes[..end]).ok()?;
    let c = s.chars().next()?;
    Some((c, c.len_utf8()))
}

/// Push one raw character of a string or interpolation body, decoding
/// multi-byte UTF-8 via [`decode_char`] and advancing past the continuation
/// bytes. Shared by [`Lexer::read_string`] and [`Lexer::read_interpolation`].
fn push_utf8(lexer: &mut Lexer<'_>, s: &mut String, ch: u8) {
    if ch < 0x80 {
        s.push(ch as char);
        return;
    }
    let start = lexer.pos - 1;
    if let Some((c, len)) = decode_char(&lexer.input[start..]) {
        s.push(c);
        for _ in 1..len {
            lexer.advance();
        }
    } else {
        s.push(ch as char);
    }
}

#[cfg(test)]
#[path = "lexer_tests.rs"]
mod tests;
