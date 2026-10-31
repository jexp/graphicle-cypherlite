//! Canonical v3 shard object names (ADR-0086, ADR-0089).

use crate::snapshot::Kind;
use crate::vfs::{hex_16, is_lower_hex_32, random_bytes_16};
use cypherlite_core::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ShardName {
    pub(super) kind: Kind,
    // The covered IDs are [lo, hi).
    pub(super) lo: i64,
    pub(super) hi: i64,
    // None is the legacy fixed spelling; Some is an immutable generation.
    pub(super) generation: Option<[u8; 16]>,
}

impl ShardName {
    pub(super) fn fixed(kind: Kind, lo: i64, hi: i64) -> Self {
        assert!(lo >= 0 && hi > lo, "invalid shard window");
        Self {
            kind,
            lo,
            hi,
            generation: None,
        }
    }

    pub(super) fn random(kind: Kind, lo: i64, hi: i64) -> Result<Self> {
        Ok(Self {
            generation: Some(random_bytes_16()?),
            ..Self::fixed(kind, lo, hi)
        })
    }

    pub(crate) fn parse(path: &str) -> Option<Self> {
        let (dir, file) = path.split_once('/')?;
        let kind = match dir {
            "nodes" => Kind::Node,
            "relationships" => Kind::Rel,
            _ => return None,
        };
        let stem = file.strip_suffix(".parquet")?;
        let mut parts = stem.split('-');
        let lo = canonical_id(parts.next()?)?;
        let hi = canonical_id(parts.next()?)?;
        if hi <= lo {
            return None;
        }
        let generation = match parts.next() {
            Some(hex) => {
                if !is_lower_hex_32(hex) {
                    return None;
                }
                let mut bytes = [0u8; 16];
                for (byte, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
                    *byte = (lower_hex(pair[0])? << 4) | lower_hex(pair[1])?;
                }
                Some(bytes)
            }
            None => None,
        };
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            kind,
            lo,
            hi,
            generation,
        })
    }

    pub(super) fn path(self) -> String {
        let mut path = format!("{}/{}-{}", self.kind.dir(), self.lo, self.hi);
        if let Some(generation) = self.generation {
            path.push('-');
            path.push_str(&hex_16(generation));
        }
        path.push_str(".parquet");
        path
    }
}

fn canonical_id(text: &str) -> Option<i64> {
    if text.is_empty()
        || (text.len() > 1 && text.starts_with('0'))
        || !text.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    text.parse().ok()
}

fn lower_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_and_generation_names_roundtrip_for_both_kinds() {
        for kind in [Kind::Node, Kind::Rel] {
            for (lo, hi) in [(0, 1), (12, 4096), (i64::MAX - 1, i64::MAX)] {
                let fixed = ShardName::fixed(kind, lo, hi);
                assert_eq!(ShardName::parse(&fixed.path()), Some(fixed));
                let generated = ShardName {
                    generation: Some([0xab; 16]),
                    ..fixed
                };
                assert_eq!(ShardName::parse(&generated.path()), Some(generated));
                assert!(generated
                    .path()
                    .ends_with(&format!("-{}.parquet", "ab".repeat(16))));
            }
            let random = ShardName::random(kind, 0, 4096).unwrap();
            assert!(random.generation.is_some());
            assert_eq!(ShardName::parse(&random.path()), Some(random));
            assert_ne!(
                random.path(),
                ShardName::random(kind, 0, 4096).unwrap().path()
            );
        }
    }

    #[test]
    fn malformed_names_are_never_cleanup_candidates() {
        let generation = "0123456789abcdef0123456789abcdef";
        for path in [
            "nodes/-1-2.parquet",
            "nodes/+1-2.parquet",
            "nodes/0-0.parquet",
            "nodes/2-1.parquet",
            "nodes/00-1.parquet",
            "nodes/0-01.parquet",
            "nodes/9223372036854775808-9223372036854775809.parquet",
            "nodes/0-1.parquet ",
            " nodes/0-1.parquet",
            "nodes/0 -1.parquet",
            "nodes/0-1.parquet/extra",
            "nodes/../0-1.parquet",
            "nodes//0-1.parquet",
            "/nodes/0-1.parquet",
            "C:/nodes/0-1.parquet",
            "nodes\\0-1.parquet",
            "nodes/0-1\\.parquet",
            "wal/.cypherlite.lock",
            "relationships/0-1.parquet.tmp",
            "nodes/0-1-ABCDEF0123456789abcdef0123456789.parquet",
            "nodes/0-1-0123456789abcdef0123456789abcde.parquet",
            "nodes/0-1-0123456789abcdef0123456789abcdef.parquet.bak",
            "nodes/0-1-0123456789abcdef0123456789abcdef-extra.parquet",
            "nodes/0-1-0123456789abcdef0123456789abcdef.parquet/other",
        ] {
            assert_eq!(ShardName::parse(path), None, "{path}");
        }
        assert_eq!(
            ShardName::parse(&format!("nodes/0-1-{generation}.parquet"))
                .unwrap()
                .generation,
            Some([
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
                0xcd, 0xef
            ])
        );
    }
}
