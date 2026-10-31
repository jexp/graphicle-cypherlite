//! Open-time option resolution: environment defaults for the snapshot codec,
//! memory limit, checkpoint interval, and the ADR-0209 bulk-write batch
//! knobs. Checkpoint execution stays in [`crate::checkpoint`].

use std::time::Duration;

use cypherlite_core::{parse_memory_limit, PreviewFeatures};
use cypherlite_storage::SnapshotCodecKind;

use crate::{OpenOptions, Result, DEFAULT_CHECKPOINT_INTERVAL_SECS};

/// Read a non-blank environment value, folding unset, blank, and
/// `VarError::NotUnicode` into `None`. The preview resolver intentionally
/// does not use this helper: it folds `NotUnicode` into `None`, while
/// [`resolve_preview_features`] must fail open on a present-but-invalid
/// value (ADR-0371), so it reads `std::env::var` directly through
/// [`preview_features_from_setting`].
fn env_setting(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(s) if !s.trim().is_empty() => Some(s),
        _ => None,
    }
}

pub(crate) fn resolve_snapshot_codec(opts: &OpenOptions) -> Result<Option<SnapshotCodecKind>> {
    if let Some(kind) = opts.snapshot_codec {
        return Ok(Some(kind));
    }
    match env_setting("CYPHERLITE_SNAPSHOT_FORMAT") {
        Some(s) => Ok(Some(s.parse()?)),
        None => Ok(None),
    }
}

/// Bytes from an already-read `CYPHERLITE_LOGICAL_GRAPH_SIZE_LIMIT` value.
/// `None` (unset or blank) is unlimited. A present value uses
/// [`parse_memory_limit`]; a bad value is that error, the same rejection
/// as `--logical-graph-size-limit`.
pub(crate) fn memory_limit_from_setting(raw: Option<&str>) -> Result<u64> {
    match raw {
        Some(s) => parse_memory_limit(s),
        None => Ok(0),
    }
}

/// A renamed limit variable must not resolve as unset. Unset is unlimited,
/// so a deployment still using the old name would open uncapped.
fn reject_renamed_env(old: &str, new: &str) -> Result<()> {
    renamed_env_error(old, new, std::env::var(old))
}

fn renamed_env_error(
    old: &str,
    new: &str,
    raw: std::result::Result<String, std::env::VarError>,
) -> Result<()> {
    match raw {
        Err(std::env::VarError::NotPresent) => Ok(()),
        Ok(value) if value.trim().is_empty() => Ok(()),
        _ => Err(crate::Error::semantic(format!(
            "{old} is set but ignored; set {new}"
        ))),
    }
}

pub(crate) fn resolve_memory_limit(opts: &OpenOptions) -> Result<u64> {
    reject_renamed_env(
        "CYPHERLITE_MEMORY_LIMIT",
        "CYPHERLITE_LOGICAL_GRAPH_SIZE_LIMIT",
    )?;
    match opts.logical_graph_size_limit {
        Some(n) => Ok(n),
        None => {
            memory_limit_from_setting(env_setting("CYPHERLITE_LOGICAL_GRAPH_SIZE_LIMIT").as_deref())
        }
    }
}

/// A positive u64 env knob with the memory limit's contract: a set value
/// that does not parse fails open with the same semantic error; zero is
/// rejected (these knobs gate on `>= threshold`, where 0 would batch
/// everything). Pure in the raw value, like [`memory_limit_from_setting`],
/// so tests never touch the process environment.
fn positive_u64_setting(name: &str, raw: Option<&str>, default: u64) -> Result<u64> {
    match raw {
        Some(raw) => raw
            .parse::<u64>()
            .ok()
            .filter(|v| *v > 0)
            .ok_or_else(|| crate::Error::semantic(format!("invalid {name} '{raw}'"))),
        None => Ok(default),
    }
}

/// The ADR-0209 bulk-write batching knobs, resolved once at open from
/// `CYPHERLITE_BULK_BATCH_THRESHOLD` / `CYPHERLITE_BULK_BATCH_BUDGET`
/// (tests set them to exercise batching on small graphs). Set-but-bad
/// values fail open, never silently fall back.
pub(crate) fn resolve_bulk_batch_limits() -> Result<(u64, u64)> {
    let threshold = positive_u64_setting(
        "CYPHERLITE_BULK_BATCH_THRESHOLD",
        env_setting("CYPHERLITE_BULK_BATCH_THRESHOLD").as_deref(),
        cypherlite_executor::DEFAULT_BATCH_THRESHOLD,
    )?;
    let budget = positive_u64_setting(
        "CYPHERLITE_BULK_BATCH_BUDGET",
        env_setting("CYPHERLITE_BULK_BATCH_BUDGET").as_deref(),
        cypherlite_executor::DEFAULT_BATCH_BUDGET,
    )?;
    Ok((threshold, budget))
}

fn token_admission_enabled(opts: &OpenOptions) -> Result<bool> {
    token_admission_from_setting(
        opts.token_admission,
        env_setting("CYPHERLITE_TOKEN_ADMISSION").as_deref(),
    )
}

/// `--token-admission` turns admission on. `CYPHERLITE_TOKEN_ADMISSION=0`
/// turns it off, including when the flag is set. `=1` turns it on. Unset
/// follows the flag.
fn token_admission_from_setting(flag: bool, raw: Option<&str>) -> Result<bool> {
    match raw.map(str::trim) {
        Some("0") => Ok(false),
        Some("1") => Ok(true),
        None => Ok(flag),
        Some(other) => Err(crate::Error::semantic(format!(
            "invalid CYPHERLITE_TOKEN_ADMISSION '{other}'"
        ))),
    }
}

fn positive_token_limit(name: &str, option: Option<u64>) -> Result<u64> {
    let value = match option {
        Some(value) => value,
        None => match env_setting(name) {
            Some(raw) => raw
                .parse::<u64>()
                .map_err(|_| crate::Error::semantic(format!("invalid {name} '{raw}'")))?,
            None => {
                return Err(crate::Error::semantic(format!(
                    "{name} is required when token admission is enabled"
                )))
            }
        },
    };
    if value == 0 {
        return Err(crate::Error::semantic(format!(
            "{name} must be positive when token admission is enabled"
        )));
    }
    Ok(value)
}

/// Token admission limits. Off ignores the pool. On, the concurrent peak
/// cap is positive or open fails.
pub(crate) fn resolve_token_limits(opts: &OpenOptions) -> Result<crate::token_budget::TokenLimits> {
    reject_renamed_env(
        "CYPHERLITE_CONCURRENT_PEAK_TOKEN_LIMIT",
        "CYPHERLITE_CONCURRENT_LOGICAL_WORKSPACE_LIMIT",
    )?;
    let enabled = token_admission_enabled(opts)?;
    if !enabled {
        return Ok(crate::token_budget::TokenLimits {
            enabled: false,
            capacity: 0,
        });
    }
    Ok(crate::token_budget::TokenLimits {
        enabled: true,
        capacity: positive_token_limit(
            "CYPHERLITE_CONCURRENT_LOGICAL_WORKSPACE_LIMIT",
            opts.concurrent_logical_workspace_limit,
        )?,
    })
}

/// The preview-feature opt-in from an already-read `CYPHERLITE_PREVIEW`
/// value (ADR-0371). Unset or empty enables nothing; an unknown feature
/// name fails open with a loud usage error naming the known names, the
/// same fail-closed contract as the bulk-batch knobs. A value present but
/// not valid UTF-8 also fails open naming the variable: the documented
/// contract is that an invalid value is never silently ignored, and
/// [`env_setting`] would fold `VarError::NotUnicode` into unset. Pure in
/// the raw value, so tests never touch the process environment.
pub(crate) fn preview_features_from_setting(
    raw: std::result::Result<String, std::env::VarError>,
) -> Result<PreviewFeatures> {
    match raw {
        Ok(value) => PreviewFeatures::parse(&value),
        Err(std::env::VarError::NotPresent) => PreviewFeatures::parse(""),
        Err(std::env::VarError::NotUnicode(_)) => Err(crate::Error::semantic(
            "invalid CYPHERLITE_PREVIEW value: not valid UTF-8",
        )),
    }
}

/// The preview-feature opt-in (ADR-0371): `CYPHERLITE_PREVIEW`, read once
/// at open. See [`preview_features_from_setting`] for the contract.
pub(crate) fn resolve_preview_features() -> Result<PreviewFeatures> {
    preview_features_from_setting(std::env::var("CYPHERLITE_PREVIEW"))
}

pub(crate) fn resolve_checkpoint_interval(opts: &OpenOptions) -> Duration {
    match opts.checkpoint_interval {
        Some(d) => d,
        None => match env_setting("CYPHERLITE_CHECKPOINT_INTERVAL_SECS") {
            Some(s) => Duration::from_secs(s.parse().unwrap_or(DEFAULT_CHECKPOINT_INTERVAL_SECS)),
            None => Duration::from_secs(DEFAULT_CHECKPOINT_INTERVAL_SECS),
        },
    }
}

#[cfg(test)]
mod tests {
    use cypherlite_core::PreviewFeature;

    use super::*;
    use crate::{Error, OpenOptions};

    #[test]
    fn non_utf8_preview_setting_fails_open_naming_the_variable() {
        // The payload is never inspected: the resolver's contract keys on
        // the `VarError` variant, and `OsString::from_vec` (the realistic
        // invalid-bytes construction) does not exist on Windows. This
        // variant is exactly what `std::env::var` returns for a variable
        // whose bytes are not UTF-8.
        let err = preview_features_from_setting(Err(std::env::VarError::NotUnicode(
            std::ffi::OsString::from("not utf-8"),
        )))
        .unwrap_err();
        assert!(matches!(err, Error::Semantic(_)), "{err}");
        let display = err.to_string();
        assert!(display.contains("CYPHERLITE_PREVIEW"), "{display}");
        assert!(display.contains("not valid UTF-8"), "{display}");
    }

    #[test]
    fn preview_setting_keeps_unset_empty_and_unknown_contract() {
        assert_eq!(
            preview_features_from_setting(Err(std::env::VarError::NotPresent)).unwrap(),
            PreviewFeatures::none(),
        );
        assert_eq!(
            preview_features_from_setting(Ok(String::new())).unwrap(),
            PreviewFeatures::none(),
        );
        assert_eq!(
            preview_features_from_setting(Ok("   ".to_string())).unwrap(),
            PreviewFeatures::none(),
        );
        let enabled = preview_features_from_setting(Ok("ontology".to_string())).unwrap();
        assert!(enabled.enabled(PreviewFeature::Ontology));
        let err = preview_features_from_setting(Ok("bogus".to_string())).unwrap_err();
        assert!(
            err.to_string().contains("unknown preview feature 'bogus'"),
            "{err}"
        );
    }

    #[test]
    fn bad_bulk_batch_setting_fails_open() {
        let err =
            positive_u64_setting("CYPHERLITE_BULK_BATCH_THRESHOLD", Some("1tb"), 7).unwrap_err();
        assert!(
            err.to_string().contains("CYPHERLITE_BULK_BATCH_THRESHOLD"),
            "{err}"
        );
        let err = positive_u64_setting("CYPHERLITE_BULK_BATCH_BUDGET", Some("0"), 7).unwrap_err();
        assert!(
            err.to_string().contains("CYPHERLITE_BULK_BATCH_BUDGET"),
            "{err}"
        );
        assert_eq!(positive_u64_setting("X", None, 7).unwrap(), 7);
        assert_eq!(positive_u64_setting("X", Some("3"), 7).unwrap(), 3);
    }

    #[test]
    fn bad_memory_limit_setting_is_the_parse_error() {
        let err = memory_limit_from_setting(Some("1tb")).unwrap_err();
        assert_eq!(
            err,
            Error::semantic("invalid memory limit unit 'tb'; use bytes, mb, mib, gb, or gib")
        );
        assert_eq!(memory_limit_from_setting(None).unwrap(), 0);
        let ignored = renamed_env_error(
            "CYPHERLITE_MEMORY_LIMIT",
            "CYPHERLITE_LOGICAL_GRAPH_SIZE_LIMIT",
            Ok("1mib".into()),
        )
        .unwrap_err();
        assert!(
            ignored
                .to_string()
                .contains("CYPHERLITE_MEMORY_LIMIT is set but ignored"),
            "{ignored}"
        );
        assert!(renamed_env_error(
            "CYPHERLITE_MEMORY_LIMIT",
            "CYPHERLITE_LOGICAL_GRAPH_SIZE_LIMIT",
            Err(std::env::VarError::NotPresent),
        )
        .is_ok());
        assert_eq!(memory_limit_from_setting(Some("2mb")).unwrap(), 2_000_000);
        // An explicit OpenOptions value wins; the env string is not consulted.
        let opts = OpenOptions {
            logical_graph_size_limit: Some(1),
            ..Default::default()
        };
        assert_eq!(resolve_memory_limit(&opts).unwrap(), 1);
    }

    #[test]
    fn token_admission_zero_turns_it_off() {
        assert!(!token_admission_from_setting(true, Some("0")).unwrap());
        assert!(token_admission_from_setting(false, Some("1")).unwrap());
        assert!(!token_admission_from_setting(false, None).unwrap());
        assert!(token_admission_from_setting(true, None).unwrap());
        let err = token_admission_from_setting(false, Some("off")).unwrap_err();
        assert!(
            err.to_string()
                .contains("invalid CYPHERLITE_TOKEN_ADMISSION 'off'"),
            "{err}"
        );
    }
}
