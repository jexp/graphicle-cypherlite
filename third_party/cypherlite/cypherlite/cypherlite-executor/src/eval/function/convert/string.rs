//! String → number parsing for `toInteger()`/`toFloat()` and their `OrNull`
//! and list variants (ADR-0175, LABS-178).
//!
//! Both pipelines mirror Neo4j's `CypherRuntimeParser`
//! (`parseAsLongOrElseNoValue` / `parseAsDoubleOrElseNoValue`):
//!
//! - `toInteger()`: trim chars ≤ U+0020 (Java `String.trim`); a
//!   `Long.parseLong` equivalent; the ADR-0168 exact-decimal parse
//!   (`BigDecimal` equivalent), now with the int32-exponent gate that diverts
//!   giant exponents (`'1e3000000000'`) to the literal fallback exactly like
//!   the `BigDecimal` constructor's `NumberFormatException`; then the shared
//!   Cypher25 number-literal fallback, whose value converts with Java
//!   `(long)`-cast semantics.
//! - `toFloat()`: a `Double.parseDouble` emulation ([`parse_double_java`])
//!   ahead of the same literal fallback; ADR-0168's non-finite-spelling
//!   guard applies to the `parseDouble` path only.
//!
//! The fallback ([`cypher25_number_literal`]) implements the Cypher25 literal
//! tokens (`Cypher25Lexer.g4` L59, `LiteralBuilder.exitNumberLiteral` and the
//! `*Literal.value()` classes): it trims Unicode whitespace, takes an
//! optional `-` (not `+`), and accepts lowercase `0x` hex integers, `0o`
//! octal integers, decimal integers with underscores, and decimal doubles
//! with underscores and one trailing `f`/`F`/`d`/`D` suffix. It never yields
//! a non-finite *spelling*: `NaN`/`Infinity` are recognized only on the
//! ≤ U+0020-trimmed `parseDouble` path.

use std::num::IntErrorKind;

use crate::eval::numeric::float_to_i64;

/// The outcome of parsing a string as an INTEGER (ADR-0175).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum IntegerString {
    /// The converted value.
    InRange(i64),
    /// A well-formed decimal whose exact value lies outside the `i64` range:
    /// `toInteger()` raises `NumberOutOfRange`, the `OrNull` and list-element
    /// paths yield null (Neo4j's 22N03, caught by the `OrNull` routes).
    OutOfRange,
    /// Not a number in any accepted form.
    NotANumber,
}

/// `toInteger()`'s string pipeline (ADR-0175): Neo4j's
/// `parseAsLongOrElseNoValue` — ≤ U+0020 trim, `parseLong`, exact decimal
/// with the int32-exponent gate, Cypher25-literal fallback.
pub(super) fn integer_from_string(s: &str) -> IntegerString {
    let trimmed = trim_java(s);
    // Long.parseLong equivalent: one optional sign, ASCII digits, i64 range.
    if let Ok(i) = trimmed.parse::<i64>() {
        return IntegerString::InRange(i);
    }
    match parse_decimal_integer(trimmed) {
        Some(DecimalInteger::InRange(i)) => IntegerString::InRange(i),
        Some(DecimalInteger::OutOfRange) => IntegerString::OutOfRange,
        None => match cypher25_number_literal(trimmed) {
            Some(NumberLiteral::Int(i)) => IntegerString::InRange(i),
            Some(NumberLiteral::Double(d)) => IntegerString::InRange(float_to_i64(d)),
            None => IntegerString::NotANumber,
        },
    }
}

/// `toFloat()`'s string pipeline (ADR-0175): Neo4j's
/// `parseAsDoubleOrElseNoValue` — `Double.parseDouble` first, then the
/// Cypher25-literal fallback whose value widens to DOUBLE.
pub(super) fn float_from_string(s: &str) -> Option<f64> {
    if let Some(d) = parse_double_java(s) {
        return Some(d);
    }
    match cypher25_number_literal(s) {
        Some(NumberLiteral::Int(i)) => Some(i as f64),
        Some(NumberLiteral::Double(d)) => Some(d),
        None => None,
    }
}

/// Java `String.trim()`: strip leading/trailing characters ≤ U+0020.
pub(super) fn trim_java(s: &str) -> &str {
    s.trim_matches(|c: char| c <= '\u{0020}')
}

/// The Cypher25 lexer's SPACE set (Cypher25Lexer.g4): Unicode `White_Space`
/// plus U+001C–U+001F. This is the "Unicode trim" the literal fallback
/// applies, which is why `toInteger('\u{00A0}7\u{00A0}')` = 7 even though
/// `String.trim` leaves U+00A0 in place.
fn trim_cypher25(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\u{001C}'..='\u{001F}').contains(&c))
}

/// A well-formed decimal string's INTEGER value (ADR-0168).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum DecimalInteger {
    /// The exact value lies in the `i64` range; truncated toward zero.
    InRange(i64),
    /// The exact value lies outside the `i64` range.
    OutOfRange,
}

/// Parse `s` as an exact decimal — an optional sign, digits with an optional
/// fraction (`7`, `7.`, `.5`, `7.5`) and an optional exponent (`e`/`E`, an
/// optional sign, digits) — and judge its exact value against the `i64`
/// range before truncating toward zero, so no `f64` rounding is involved
/// (Neo4j's `BigDecimal` path). `None` means `s` is not a decimal number,
/// which includes every NaN and Infinity spelling.
///
/// ADR-0175: a well-formed decimal whose exponent exceeds the `i32` range
/// (`'1e3000000000'`) also yields `None`, mirroring the `BigDecimal`
/// constructor's `NumberFormatException`, which diverts the string to the
/// Cypher25-literal fallback.
pub(super) fn parse_decimal_integer(s: &str) -> Option<DecimalInteger> {
    let (negative, unsigned) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (mantissa, exponent) = match unsigned.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, parse_decimal_exponent(exponent)?),
        None => (unsigned, 0),
    };
    // ADR-0175: `new BigDecimal(s)` parses the exponent as a Java `int`; an
    // exponent outside the i32 range throws NumberFormatException and the
    // string falls through to the Cypher25-literal fallback.
    if i32::try_from(exponent).is_err() {
        return None;
    }
    let (int_digits, frac_digits) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let all_digits = |d: &str| d.bytes().all(|b| b.is_ascii_digit());
    if (int_digits.is_empty() && frac_digits.is_empty())
        || !all_digits(int_digits)
        || !all_digits(frac_digits)
    {
        return None;
    }
    let digits: Vec<u8> = int_digits
        .bytes()
        .chain(frac_digits.bytes())
        .map(|b| b - b'0')
        .collect();
    let leading_zeros = digits.iter().take_while(|&&d| d == 0).count();
    let significant = &digits[leading_zeros..];
    if significant.is_empty() {
        return Some(DecimalInteger::InRange(0));
    }
    // Number of integer digits once the exponent is applied, counted from
    // the first non-zero digit.
    let len = |n: usize| n as i128;
    let int_len = len(int_digits.len()) + i128::from(exponent) - len(leading_zeros);
    if int_len <= 0 {
        // 0 < |value| < 1 truncates to zero.
        return Some(DecimalInteger::InRange(0));
    }
    // A leading non-zero digit followed by 19 or more digits is >= 10^19,
    // beyond both i64 limits.
    let Some(int_len) = usize::try_from(int_len).ok().filter(|&n| n <= 19) else {
        return Some(DecimalInteger::OutOfRange);
    };
    let magnitude = significant
        .iter()
        .chain(std::iter::repeat(&0))
        .take(int_len)
        .fold(0u64, |acc, &d| acc * 10 + u64::from(d));
    let has_fraction = significant
        .get(int_len..)
        .is_some_and(|rest| rest.iter().any(|&d| d != 0));
    let truncated = if negative {
        0i64.checked_sub_unsigned(magnitude)
    } else {
        i64::try_from(magnitude).ok()
    };
    // The range check precedes truncation: a limit plus a fraction is out.
    Some(match truncated {
        Some(i) if !(has_fraction && (i == i64::MAX || i == i64::MIN)) => {
            DecimalInteger::InRange(i)
        }
        _ => DecimalInteger::OutOfRange,
    })
}

/// The exponent of a decimal string: an optional sign and at least one
/// digit. Saturates at the `i64` limits, far beyond where the value leaves
/// the `i64` range or truncates to zero.
fn parse_decimal_exponent(s: &str) -> Option<i64> {
    // `str::parse` reports an overflow without reading the rest of the
    // string, so the digits are checked first: `1e99999999999999999999x` is
    // not a number.
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    match s.parse::<i64>() {
        Ok(e) => Some(e),
        Err(e) => match e.kind() {
            IntErrorKind::PosOverflow => Some(i64::MAX),
            IntErrorKind::NegOverflow => Some(i64::MIN),
            _ => None,
        },
    }
}

/// `toFloat()`'s `Double.parseDouble` emulation (ADR-0168's spelling guard on
/// the ADR-0175 pipeline): leading/trailing characters ≤ U+0020 are ignored;
/// the only non-finite spellings are exactly `NaN` or `Infinity`, optionally
/// after one `+` or `-`; Java's hex-float forms (`0x1p3`, `0x1.8p1`,
/// case-tolerant `0X1P3`) parse; one trailing `f`/`F`/`d`/`D` suffix is
/// accepted (`1.5f`, `2f`); numeric strings that overflow `f64` parse to
/// ±Infinity. The other spellings Rust accepts (`nan`, `inf`, `infinity` in
/// any case) are not numbers.
fn parse_double_java(s: &str) -> Option<f64> {
    let s = trim_java(s);
    if s.is_empty() {
        return None;
    }
    let (negative, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    // Java's non-finite spellings, exact case (ADR-0168); the sign is
    // meaningless for NaN.
    if body == "NaN" {
        return Some(f64::NAN);
    }
    if body == "Infinity" {
        return Some(if negative {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    // In Rust's grammar only the non-finite spellings start with a letter;
    // reject every other such spelling before Rust's `f64` parse sees them.
    if body.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let value = if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        parse_hex_float(hex)?
    } else {
        // One optional trailing float/double suffix (`1.5f`, `2f`); `2L` is
        // not a number.
        let core = body.strip_suffix(['f', 'F', 'd', 'D']).unwrap_or(body);
        core.parse::<f64>().ok()?
    };
    Some(if negative { -value } else { value })
}

/// Java's hex-float significand and binary exponent (the text after
/// `0x`/`0X`): hex digits with an optional `.` (at least one digit overall),
/// a mandatory `p`/`P` exponent (optional sign, decimal digits) and an
/// optional `f`/`F`/`d`/`D` suffix. Without a `p`/`P` exponent the form is
/// not a hex float (`0x1F` is the literal fallback's hex *integer*).
fn parse_hex_float(s: &str) -> Option<f64> {
    let (significand, rest) = s.split_once(['p', 'P'])?;
    let rest = rest.strip_suffix(['f', 'F', 'd', 'D']).unwrap_or(rest);
    let exponent = parse_decimal_exponent(rest)?;
    let (int_part, frac_part) = significand.split_once('.').unwrap_or((significand, ""));
    let is_hex = |b: u8| b.is_ascii_hexdigit();
    if (int_part.is_empty() && frac_part.is_empty())
        || !int_part.bytes().all(is_hex)
        || !frac_part.bytes().all(is_hex)
    {
        return None;
    }
    let mut value = 0.0f64;
    // Power-of-two correction applied to keep the accumulate step exact:
    // below 2^48, `value * 16 + digit` is exact in f64.
    let mut adjust = 0i64;
    for b in int_part.bytes().chain(frac_part.bytes()) {
        let digit = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            _ => b - b'A' + 10,
        };
        value = value * 16.0 + f64::from(digit);
        if value >= 281_474_976_710_656.0 {
            value *= 0.0625;
            adjust += 4;
        }
    }
    if value == 0.0 {
        return Some(0.0);
    }
    let exp = exponent
        .saturating_add(adjust)
        .saturating_sub(4 * frac_part.len() as i64)
        .clamp(-2200, 2200);
    Some(value * 2f64.powi(exp as i32))
}

/// A Cypher25 number literal's value (ADR-0175's literal fallback).
#[derive(Debug, PartialEq)]
enum NumberLiteral {
    Int(i64),
    Double(f64),
}

/// The shared Cypher25 number-literal fallback (ADR-0175): Neo4j parses the
/// string with the Cypher25 parser's `numberLiteral` rule and converts the
/// literal's value. Unicode whitespace trims; one optional `-` is accepted
/// (`+` is not: `'+0x1F'` is null); lowercase `0x`/`0o` prefix hex/octal
/// integers (uppercase `0X`/`0O` is not: `'0X1F'` is null); decimal integers
/// take underscores (`1_000`); decimal doubles take underscores and one
/// trailing `f`/`F`/`d`/`D` suffix via `Double.parseDouble`
/// (`DecimalDoubleLiteral.stringToDouble`), while an integer token with a
/// trailing letter is not a number (`'2f'`, `'2L'`). Integer literals outside
/// the `i64` range are no value (Neo4j's `Long.decode`/`parseLong` throws).
fn cypher25_number_literal(s: &str) -> Option<NumberLiteral> {
    let s = trim_cypher25(s);
    let (negative, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    if body.is_empty() {
        return None;
    }
    // UNSIGNED_HEX_INTEGER / UNSIGNED_OCTAL_INTEGER (lowercase prefix only).
    if let Some(hex) = body.strip_prefix("0x") {
        return parse_radix_literal(negative, hex, 16).map(NumberLiteral::Int);
    }
    if let Some(oct) = body.strip_prefix("0o") {
        return parse_radix_literal(negative, oct, 8).map(NumberLiteral::Int);
    }
    cypher25_decimal(negative, body)
}

/// The fallback's decimal forms: a DECIMAL_DOUBLE token (digits `.` digits,
/// `.` digits, or digits exponent — Cypher25Lexer.g4 L59, tolerating a
/// trailing identifier whose value survives only as one `f`/`F`/`d`/`D`
/// suffix) or an UNSIGNED_DECIMAL_INTEGER token (digits with underscores,
/// trailing letters failing the value parse).
fn cypher25_decimal(negative: bool, body: &str) -> Option<NumberLiteral> {
    let bytes = body.as_bytes();
    let len = bytes.len();
    // A run of INTEGER_PARTs: [0-9] ('_'? [0-9])*.
    let run = |mut i: usize| -> usize {
        while i < len {
            if bytes[i].is_ascii_digit() {
                i += 1;
            } else if bytes[i] == b'_' && i + 1 < len && bytes[i + 1].is_ascii_digit() {
                i += 2;
            } else {
                break;
            }
        }
        i
    };
    let int_end = run(0);
    let has_int = int_end > 0;
    let mut i = int_end;
    let mut has_dot = false;
    let mut has_frac = false;
    if i < len && bytes[i] == b'.' {
        has_dot = true;
        i += 1;
        let frac_end = run(i);
        has_frac = frac_end > i;
        i = frac_end;
    }
    let mut has_exp = false;
    if i < len && matches!(bytes[i], b'e' | b'E') {
        let mut j = i + 1;
        if j < len && matches!(bytes[j], b'+' | b'-') {
            j += 1;
        }
        let exp_end = run(j);
        if exp_end > j {
            has_exp = true;
            i = exp_end;
        }
    }
    let suffix = &body[i..];
    let is_double = (has_dot && has_frac) || (has_int && !has_dot && has_exp);
    if is_double {
        // DecimalDoubleLiteral.stringToDouble: with an underscore the text
        // must match the double matcher — which allows no suffix — before the
        // underscores strip; without, Double.parseDouble takes one suffix.
        let d = if body.contains('_') {
            if !suffix.is_empty() {
                return None;
            }
            body.replace('_', "").parse::<f64>().ok()?
        } else {
            let core = suffix
                .strip_prefix(['f', 'F', 'd', 'D'])
                .filter(|rest| rest.is_empty())
                .map_or(body, |_| &body[..i]);
            core.parse::<f64>().ok()?
        };
        return Some(NumberLiteral::Double(if negative { -d } else { d }));
    }
    // UNSIGNED_DECIMAL_INTEGER: StringDecimalInteger.stringToLong — with an
    // underscore the text must be digits with single underscores between
    // digit runs; without, plain parseLong. Either way, i64 range.
    let joined = join_underscored(body, 10)?;
    let signed = if negative {
        format!("-{joined}")
    } else {
        joined
    };
    signed.parse::<i64>().ok().map(NumberLiteral::Int)
}

/// Join a digit string that may carry Cypher25 underscore separators into
/// the plain digit string: every `_`-separated part must be non-empty and
/// all `radix` digits (a text without an underscore is a single part).
/// `None` rejects the literal.
fn join_underscored(digits: &str, radix: u32) -> Option<String> {
    let ok = |t: &str| !t.is_empty() && t.bytes().all(|b| (b as char).is_digit(radix));
    let parts: Vec<&str> = digits.split('_').collect();
    if parts.iter().all(|p| ok(p)) {
        Some(parts.concat())
    } else {
        None
    }
}

/// A hex/octal literal's value: digits with the Cypher25 underscore rule
/// (`(_?[0-9a-fA-F]+)+` after the prefix — one optional underscore per digit
/// group, a leading one included), judged against the `i64` range like
/// Java's `Long.decode` (overflow is no value).
fn parse_radix_literal(negative: bool, digits: &str, radix: u32) -> Option<i64> {
    let digits = digits.strip_prefix('_').unwrap_or(digits);
    let joined = join_underscored(digits, radix)?;
    let magnitude = u128::from_str_radix(&joined, radix).ok()?;
    let limit = if negative {
        (i64::MAX as u128) + 1
    } else {
        i64::MAX as u128
    };
    if magnitude > limit {
        return None;
    }
    let value = magnitude as i128;
    Some(if negative { -value } else { value } as i64)
}
