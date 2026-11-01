//! Open-time option resolution: environment defaults for the snapshot codec,
//! memory limit, checkpoint interval, and the ADR-0209 bulk-write batch
//! knobs. Checkpoint execution stays in [`crate::checkpoint`].

use std::time::Duration;

use cypherlite_core::{
    parse_memory_limit, parse_positive_memory_limit, require_positive_memory_limit, PreviewFeatures,
};
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

/// The markdown vault's body-property configuration (ADR-0460): the
/// `OpenOptions` value wins, else `CYPHERLITE_MARKDOWN_BODY_PROPERTIES`
/// parses with the flag's grammar — a bad value fails open with the
/// same error the flag's parse gives; unset or blank is `None`, the
/// sidecar manifest's recorded configuration.
pub(crate) fn resolve_markdown_bodies(
    opts: &OpenOptions,
) -> Result<Option<cypherlite_storage::MarkdownBodyProperties>> {
    if let Some(bodies) = &opts.markdown_body_properties {
        return Ok(Some(bodies.clone()));
    }
    markdown_bodies_from_setting(std::env::var("CYPHERLITE_MARKDOWN_BODY_PROPERTIES"))
}

/// The one environment-value triage the variable-backed resolvers
/// share: unset, blank, and whitespace are `None`; a value that is not
/// valid UTF-8 fails open naming the variable — an invalid value is
/// never silently ignored, the preview-feature contract
/// ([`preview_features_from_setting`]). Pure in the raw result, so
/// tests never touch the process environment.
fn present_env_value(
    name: &'static str,
    raw: std::result::Result<String, std::env::VarError>,
) -> Result<Option<String>> {
    match raw {
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(crate::Error::semantic(format!(
            "invalid {name}: value is not valid UTF-8"
        ))),
        Ok(value) if value.trim().is_empty() => Ok(None),
        Ok(value) => Ok(Some(value)),
    }
}

/// The markdown vault's body-property configuration from an already-read
/// `CYPHERLITE_MARKDOWN_BODY_PROPERTIES` value. Unset, blank, and
/// whitespace are `None` (the sidecar manifest's recorded configuration
/// governs); a present value parses with the flag's grammar. Pure in the
/// raw result, so tests never touch the process environment.
fn markdown_bodies_from_setting(
    raw: std::result::Result<String, std::env::VarError>,
) -> Result<Option<cypherlite_storage::MarkdownBodyProperties>> {
    let Some(spec) = present_env_value("CYPHERLITE_MARKDOWN_BODY_PROPERTIES", raw)? else {
        return Ok(None);
    };
    Ok(Some(cypherlite_storage::MarkdownBodyProperties::parse(
        &spec,
    )?))
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

/// A concurrent logical-workspace pool from an already-read
/// `CYPHERLITE_CONCURRENT_LOGICAL_WORKSPACE_LIMIT` value (ADR-0452).
///
/// Unset, blank, and whitespace leave admission off (`Ok(None)`). A
/// present value uses [`cypherlite_core::parse_positive_memory_limit`],
/// the same `SIZE` grammar as the live-graph cap. `0`, a bad SIZE, and a
/// value that is not valid UTF-8 fail open naming the variable. This
/// takes the [`std::env::var`] result so a present-but-unusable value is
/// never treated as unset. Pure in that result, so tests never touch the
/// process environment.
pub fn workspace_limit_from_setting(
    raw: std::result::Result<String, std::env::VarError>,
) -> Result<Option<u64>> {
    match present_env_value("CYPHERLITE_CONCURRENT_LOGICAL_WORKSPACE_LIMIT", raw)? {
        Some(raw) => {
            parse_positive_memory_limit("CYPHERLITE_CONCURRENT_LOGICAL_WORKSPACE_LIMIT", &raw)
                .map(Some)
        }
        None => Ok(None),
    }
}

/// Token admission limits (ADR-0452). A positive concurrent
/// logical-workspace limit turns the pool on. Unset leaves it off. The
/// engine does not invent a default capacity.
pub(crate) fn resolve_token_limits(opts: &OpenOptions) -> Result<crate::token_budget::TokenLimits> {
    reject_renamed_env(
        "CYPHERLITE_CONCURRENT_PEAK_TOKEN_LIMIT",
        "CYPHERLITE_CONCURRENT_LOGICAL_WORKSPACE_LIMIT",
    )?;
    reject_renamed_env(
        "CYPHERLITE_TOKEN_ADMISSION",
        "CYPHERLITE_CONCURRENT_LOGICAL_WORKSPACE_LIMIT",
    )?;
    let capacity = match opts.concurrent_logical_workspace_limit {
        Some(n) => Some(require_positive_memory_limit(
            "concurrent_logical_workspace_limit",
            n,
        )?),
        None => workspace_limit_from_setting(std::env::var(
            "CYPHERLITE_CONCURRENT_LOGICAL_WORKSPACE_LIMIT",
        ))?,
    };
    Ok(crate::token_budget::TokenLimits { capacity })
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
    match present_env_value("CYPHERLITE_PREVIEW", raw)? {
        Some(value) => PreviewFeatures::parse(&value),
        // Unset and blank both enable nothing — `parse("")` is
        // `PreviewFeatures::none()`.
        None => Ok(PreviewFeatures::none()),
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
    fn markdown_bodies_setting_parses_unset_and_fails_open() {
        // The env contract of the sibling resolvers: unset/blank is None
        // (the sidecar governs), a good spec parses with the flag
        // grammar, a bad spec is its parse error, and non-UTF-8 fails
        // open naming the variable.
        assert_eq!(
            markdown_bodies_from_setting(Err(std::env::VarError::NotPresent)).unwrap(),
            None
        );
        assert_eq!(
            markdown_bodies_from_setting(Ok(String::new())).unwrap(),
            None
        );
        assert_eq!(
            markdown_bodies_from_setting(Ok("   ".to_string())).unwrap(),
            None
        );
        let config = markdown_bodies_from_setting(Ok("summary,notes".to_string()))
            .unwrap()
            .expect("a good spec parses");
        assert_eq!(config.default, vec!["summary", "notes"]);
        let bad = markdown_bodies_from_setting(Ok("summary,,notes".to_string())).unwrap_err();
        assert!(
            bad.to_string().contains("an entry names no property"),
            "{bad}"
        );
        let utf8 = markdown_bodies_from_setting(Err(std::env::VarError::NotUnicode(
            std::ffi::OsString::from("not utf-8"),
        )))
        .unwrap_err();
        assert!(
            utf8.to_string()
                .contains("CYPHERLITE_MARKDOWN_BODY_PROPERTIES: value is not valid UTF-8"),
            "{utf8}"
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
    fn workspace_limit_setting_enables_only_when_positive() {
        assert_eq!(
            workspace_limit_from_setting(Err(std::env::VarError::NotPresent)).unwrap(),
            None
        );
        assert_eq!(
            workspace_limit_from_setting(Ok(String::new())).unwrap(),
            None
        );
        assert_eq!(
            workspace_limit_from_setting(Ok("   ".into())).unwrap(),
            None
        );
        assert_eq!(
            workspace_limit_from_setting(Ok("4096".into())).unwrap(),
            Some(4096)
        );
        assert_eq!(
            workspace_limit_from_setting(Ok("2mib".into())).unwrap(),
            Some(2 * 1024 * 1024)
        );
        let zero = workspace_limit_from_setting(Ok("0".into())).unwrap_err();
        assert!(
            zero.to_string().contains(
                "invalid CYPHERLITE_CONCURRENT_LOGICAL_WORKSPACE_LIMIT: must be positive"
            ),
            "{zero}"
        );
        let bad = workspace_limit_from_setting(Ok("1tb".into())).unwrap_err();
        assert!(
            bad.to_string()
                .contains("invalid CYPHERLITE_CONCURRENT_LOGICAL_WORKSPACE_LIMIT:"),
            "{bad}"
        );
        let utf8 = workspace_limit_from_setting(Err(std::env::VarError::NotUnicode(
            std::ffi::OsString::from("not utf-8"),
        )))
        .unwrap_err();
        assert!(
            utf8.to_string().contains(
                "invalid CYPHERLITE_CONCURRENT_LOGICAL_WORKSPACE_LIMIT: value is not valid UTF-8"
            ),
            "{utf8}"
        );
        let ignored = renamed_env_error(
            "CYPHERLITE_TOKEN_ADMISSION",
            "CYPHERLITE_CONCURRENT_LOGICAL_WORKSPACE_LIMIT",
            Ok("1".into()),
        )
        .unwrap_err();
        assert!(
            ignored
                .to_string()
                .contains("CYPHERLITE_TOKEN_ADMISSION is set but ignored"),
            "{ignored}"
        );
        let on = resolve_token_limits(&OpenOptions {
            concurrent_logical_workspace_limit: Some(8192),
            ..OpenOptions::default()
        })
        .unwrap();
        assert_eq!(on.capacity, Some(8192));
        let zero_option = resolve_token_limits(&OpenOptions {
            concurrent_logical_workspace_limit: Some(0),
            ..OpenOptions::default()
        })
        .unwrap_err();
        assert!(
            zero_option
                .to_string()
                .contains("invalid concurrent_logical_workspace_limit: must be positive"),
            "{zero_option}"
        );
    }
}
