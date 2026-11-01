//! Human-readable memory-limit sizes (ADR-0042).

use crate::{Error, Result};

/// Parse `--logical-graph-size-limit` / `CYPHERLITE_LOGICAL_GRAPH_SIZE_LIMIT` sizes.
///
/// Integer bytes, or an integer plus `b`/`bytes`/`mb`/`mib`/`gb`/`gib`
/// (case-insensitive, optional space). `mb`/`gb` are SI; `mib`/`gib` are IEC.
/// `0` is unlimited at the call site.
///
/// ```
/// use cypherlite_core::parse_memory_limit;
/// assert_eq!(parse_memory_limit("1gib").unwrap(), 1 << 30);
/// assert_eq!(parse_memory_limit("2 mb").unwrap(), 2_000_000);
/// ```
pub fn parse_memory_limit(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        return Err(Error::semantic("empty memory limit"));
    }
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, rest) = s.split_at(split);
    if num.is_empty() {
        return Err(Error::semantic(format!("invalid memory limit '{s}'")));
    }
    let n: u64 = num
        .parse()
        .map_err(|_| Error::semantic(format!("invalid memory limit '{s}'")))?;
    let unit = rest.trim().to_ascii_lowercase();
    let mul: u64 = match unit.as_str() {
        "" | "b" | "byte" | "bytes" => 1,
        "mb" => 1_000_000,
        "mib" => 1 << 20,
        "gb" => 1_000_000_000,
        "gib" => 1 << 30,
        _ => {
            return Err(Error::semantic(format!(
                "invalid memory limit unit '{rest}'; use bytes, mb, mib, gb, or gib"
            )))
        }
    };
    n.checked_mul(mul)
        .ok_or_else(|| Error::semantic(format!("memory limit '{s}' overflows u64")))
}

/// Reject `0` for a size that turns a feature on.
///
/// `0` is `invalid {name}: must be positive`. A positive `n` is returned
/// unchanged. [`parse_memory_limit`] still accepts `0` (unlimited) for the
/// live-graph cap.
pub fn require_positive_memory_limit(name: &str, n: u64) -> Result<u64> {
    if n == 0 {
        Err(Error::semantic(format!("invalid {name}: must be positive")))
    } else {
        Ok(n)
    }
}

/// [`parse_memory_limit`] for a size that must be greater than zero.
///
/// `name` is the flag or variable in the error. A bad SIZE is
/// `invalid {name}: …` around that parse error. `0` is
/// [`require_positive_memory_limit`]. A blank string is a bad SIZE, not
/// an unset value; callers that treat blank as off fold it first.
pub fn parse_positive_memory_limit(name: &str, s: &str) -> Result<u64> {
    let n = match parse_memory_limit(s) {
        Ok(n) => n,
        Err(Error::Semantic(msg)) => {
            return Err(Error::semantic(format!("invalid {name}: {msg}")));
        }
        Err(other) => return Err(other),
    };
    require_positive_memory_limit(name, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        assert_eq!(parse_memory_limit("0").unwrap(), 0);
        assert_eq!(parse_memory_limit("512").unwrap(), 512);
        assert_eq!(parse_memory_limit("512b").unwrap(), 512);
        assert_eq!(parse_memory_limit("1 bytes").unwrap(), 1);
        assert_eq!(parse_memory_limit("2mb").unwrap(), 2_000_000);
        assert_eq!(parse_memory_limit("2 MiB").unwrap(), 2 << 20);
        assert_eq!(parse_memory_limit("1GB").unwrap(), 1_000_000_000);
        assert_eq!(parse_memory_limit("1gib").unwrap(), 1 << 30);
    }

    #[test]
    fn rejects_unknown_unit() {
        assert!(parse_memory_limit("1tb").is_err());
        assert!(parse_memory_limit("gib").is_err());
        assert!(parse_memory_limit("").is_err());
    }

    #[test]
    fn rejects_overflow() {
        assert!(parse_memory_limit("18446744073709551615gib").is_err());
    }

    #[test]
    fn positive_limit_rejects_zero_and_names_the_knob() {
        let zero =
            require_positive_memory_limit("concurrent_logical_workspace_limit", 0).unwrap_err();
        assert_eq!(
            zero.to_string(),
            "semantic error: invalid concurrent_logical_workspace_limit: must be positive"
        );
        assert_eq!(
            require_positive_memory_limit("--concurrent-logical-workspace-limit", 8).unwrap(),
            8
        );
        let bad =
            parse_positive_memory_limit("CYPHERLITE_CONCURRENT_LOGICAL_WORKSPACE_LIMIT", "1tb")
                .unwrap_err();
        assert!(
            bad.to_string()
                .contains("invalid CYPHERLITE_CONCURRENT_LOGICAL_WORKSPACE_LIMIT:"),
            "{bad}"
        );
        assert_eq!(
            parse_positive_memory_limit("--concurrent-logical-workspace-limit", "2mib").unwrap(),
            2 * 1024 * 1024
        );
    }
}
