//! String function helpers (character ranges, trim family, arg extraction,
//! normalization check).

use std::collections::HashSet;
use std::ops::Range;

use cypherlite_core::{Error, Result, Value};
use unicode_normalization::UnicodeNormalization;

pub(crate) fn string_arg(vals: &[Value], i: usize) -> Result<Option<&str>> {
    match vals.get(i) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        _ => Err(Error::ty("expected string argument")),
    }
}

/// The characters `r` of `s` (character indices, not bytes) as a slice of
/// `s`, found in one walk without copying. Both bounds clamp to the end of
/// the string, and an end before the start selects nothing.
pub(crate) fn char_range(s: &str, r: Range<usize>) -> &str {
    let byte = |s: &str, c: usize| s.char_indices().nth(c).map_or(s.len(), |(b, _)| b);
    let rest = &s[byte(s, r.start)..];
    &rest[..byte(rest, r.end.saturating_sub(r.start))]
}

pub(crate) fn str_unary(vals: &[Value], f: impl Fn(&str) -> String) -> Result<Value> {
    Ok(match vals.first() {
        Some(Value::String(s)) => Value::String(f(s).into()),
        Some(Value::Null) | None => Value::Null,
        _ => return Err(Error::ty("expected string")),
    })
}

#[derive(Clone, Copy)]
pub(crate) enum TrimMode {
    Both,
    Leading,
    Trailing,
}

pub(crate) fn java_whitespace(c: char) -> bool {
    match c {
        '\u{001C}'..='\u{001F}' => true,
        '\u{00A0}' | '\u{2007}' | '\u{202F}' | '\u{0085}' => false,
        _ => c.is_whitespace(),
    }
}

pub(crate) fn trim_function(vals: &[Value], mode: TrimMode, strict_single: bool) -> Result<Value> {
    let Some(input) = vals.first() else {
        return Err(Error::semantic("trim function requires an argument"));
    };
    if input.is_null() || vals.get(1).is_some_and(Value::is_null) {
        return Ok(Value::Null);
    }
    let Value::String(input) = input else {
        return Err(Error::invalid_argument_type("trim input must be a string"));
    };
    let out = match vals.get(1) {
        None => match mode {
            TrimMode::Both => input.trim_matches(java_whitespace),
            TrimMode::Leading => input.trim_start_matches(java_whitespace),
            TrimMode::Trailing => input.trim_end_matches(java_whitespace),
        },
        Some(Value::String(chars)) => {
            if strict_single && chars.chars().count() != 1 {
                return Err(Error::ty(
                    "InvalidArgumentValue: trim character string must contain one character",
                ));
            }
            if chars.is_empty() {
                input
            } else {
                let set: HashSet<char> = chars.chars().collect();
                match mode {
                    TrimMode::Both => input.trim_matches(|c| set.contains(&c)),
                    TrimMode::Leading => input.trim_start_matches(|c| set.contains(&c)),
                    TrimMode::Trailing => input.trim_end_matches(|c| set.contains(&c)),
                }
            }
        }
        Some(_) => {
            return Err(Error::invalid_argument_type(
                "trim character string must be a string",
            ))
        }
    };
    Ok(Value::String(out.into()))
}

pub(crate) fn is_normalized(s: &str, form: &str) -> bool {
    let normalized: String = match form.to_ascii_uppercase().as_str() {
        "NFD" => s.nfd().collect(),
        "NFKC" => s.nfkc().collect(),
        "NFKD" => s.nfkd().collect(),
        _ => s.nfc().collect(),
    };
    normalized == s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_range_slices_by_character_and_clamps_at_the_end() {
        // Five characters of 1, 2, 4, 1 and 2 bytes.
        let s = "añ😀bç";
        assert_eq!(char_range(s, 0..5), s);
        assert_eq!(char_range(s, 1..3), "ñ😀");
        assert_eq!(char_range(s, 2..4), "😀b");
        assert_eq!(char_range(s, 4..5), "ç");
        assert_eq!(char_range(s, 3..usize::MAX), "bç");
        assert_eq!(char_range(s, 0..99), s);
        // Empty: zero width, start at or past the end, or end before start.
        assert_eq!(char_range(s, 0..0), "");
        assert_eq!(char_range(s, 5..9), "");
        assert_eq!(char_range(s, usize::MAX..usize::MAX), "");
        assert_eq!(char_range(s, Range { start: 3, end: 1 }), "");
        assert_eq!(char_range("", 0..1), "");
    }
}
