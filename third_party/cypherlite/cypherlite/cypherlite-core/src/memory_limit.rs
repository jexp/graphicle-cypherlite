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
}
