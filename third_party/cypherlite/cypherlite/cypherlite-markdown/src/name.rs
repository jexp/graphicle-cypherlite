//! Vault file-name sanitization (ADR-0406 §2).
//!
//! One canonical replacement rule shared by labels (directory names)
//! and name paths (file names): the characters illegal in Obsidian
//! wikilinks (`[ ] # ^ |`, the lpgfs set) plus the characters illegal
//! on common filesystems (`/ \ : * ? " < >`) and control bytes become
//! `_`, leading dots are prefixed away (a vault must not grow hidden
//! files), trailing dots and spaces are trimmed, and Windows device
//! names are defused. Sanitization is lossy: a name that needed it no
//! longer equals its property value, and the reverse mapping is not
//! attempted (the lpgfs import tool's documented stance).

use cypherlite_core::Result;

use crate::{is_c0_del, vault_error};

/// The one replacement character (a constant, not a policy knob).
const REPLACEMENT: char = '_';

/// Sanitize a single path segment (a label directory name or one
/// `/`-separated piece of a name path). Fails closed on an input that
/// sanitizes to nothing.
pub fn sanitize_label(raw: &str) -> Result<String> {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if is_illegal(ch) {
            out.push(REPLACEMENT);
        } else {
            out.push(ch);
        }
    }
    let trimmed = out.trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        return Err(vault_error(format!(
            "name '{raw}' has no representable characters"
        )));
    }
    let mut name = trimmed.to_string();
    // One guard over the pre-insert state: a '_'-prefixed name is
    // never a Windows device name, so the two hazards can never both
    // fire — and both are defused by the same single prefix.
    if name.starts_with('.') || is_windows_device_name(&name) {
        name.insert(0, REPLACEMENT);
    }
    Ok(name)
}

/// Sanitize a whole name path (`a/b` stays two segments); every segment
/// goes through [`sanitize_label`].
pub fn sanitize_name_path(raw: &str) -> Result<String> {
    let sanitized = raw
        .split('/')
        .map(sanitize_label)
        .collect::<Result<Vec<_>>>()?;
    Ok(sanitized.join("/"))
}

fn is_illegal(ch: char) -> bool {
    matches!(
        ch,
        '[' | ']' | '#' | '^' | '|' | '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>'
    ) || is_c0_del(ch)
}

fn is_windows_device_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_the_lpgfs_and_filesystem_illegal_sets() {
        assert_eq!(sanitize_label("a[b]c#d^e|f").unwrap(), "a_b_c_d_e_f");
        assert_eq!(
            sanitize_label("a/b\\c:d*e?f\"g<h>i").unwrap(),
            "a_b_c_d_e_f_g_h_i"
        );
    }

    #[test]
    fn del_is_replaced_like_the_c0_controls() {
        // DEL is part of the one control set (`is_c0_del`), even though
        // `char::is_ascii_control` alone stops at U+001F.
        assert_eq!(sanitize_label("a\u{7f}b").unwrap(), "a_b");
        assert_eq!(sanitize_label("a\u{1}b").unwrap(), "a_b");
    }

    #[test]
    fn dot_and_space_hygiene() {
        assert_eq!(sanitize_label(".hidden").unwrap(), "_.hidden");
        assert_eq!(sanitize_label("name. ").unwrap(), "name");
        assert!(sanitize_label("...").is_err());
    }

    #[test]
    fn windows_device_names_are_defused() {
        assert_eq!(sanitize_label("con").unwrap(), "_con");
        assert_eq!(sanitize_label("AUX").unwrap(), "_AUX");
        assert_eq!(sanitize_label("console").unwrap(), "console");
    }

    #[test]
    fn empty_results_fail_closed() {
        assert!(sanitize_label("").is_err());
        assert!(sanitize_label(" . ").is_err());
        // Each illegal character is replaced, so this survives as a name.
        assert_eq!(sanitize_label("///").unwrap(), "___");
    }

    #[test]
    fn name_paths_keep_segment_structure() {
        assert_eq!(sanitize_name_path("a b/c:d").unwrap(), "a b/c_d");
        assert!(sanitize_name_path("a//b").is_err());
        assert!(sanitize_name_path("/lead").is_err());
    }
}
