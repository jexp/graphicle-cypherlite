//! Open presets and closed-gate preconditions shared by unit tests and
//! `tests/`.
use std::env::VarError;
use std::time::{Duration, Instant};

use super::{Database, OpenMode, OpenOptions, Value};

/// Assert the process does not opt into any preview feature: the closed-gate
/// fixture precondition (ADR-0371). `CYPHERLITE_PREVIEW` must be unset or
/// blank — `Database::open` resolves it from the live process environment, so
/// a dev shell exporting it silently changes what a fail-closed fixture tests;
/// this fails loudly instead, naming the variable and the leaked setting.
#[doc(hidden)]
pub fn assert_preview_unset() {
    if let Err(leaked) = closed_preview_setting(std::env::var("CYPHERLITE_PREVIEW")) {
        panic!(
            "the test environment sets {leaked}; unset it for the closed-gate fixture \
             (Database::open reads CYPHERLITE_PREVIEW from the process environment)"
        );
    }
}

/// Classify one `CYPHERLITE_PREVIEW` read: `Ok(())` when the setting is absent
/// or blank, `Err(leaked)` naming it otherwise. Split from the environment read
/// so both branches are unit-tested without mutating process-global state.
fn closed_preview_setting(setting: Result<String, VarError>) -> Result<(), String> {
    match setting {
        Ok(value) if value.trim().is_empty() => Ok(()),
        Ok(value) => Err(format!("CYPHERLITE_PREVIEW={value:?}")),
        Err(VarError::NotPresent) => Ok(()),
        Err(VarError::NotUnicode(raw)) => {
            Err(format!("CYPHERLITE_PREVIEW={raw:?} (not valid Unicode)"))
        }
    }
}

#[doc(hidden)]
pub fn writable() -> OpenOptions {
    OpenOptions {
        checkpoint_interval: Some(Duration::ZERO),
        ..OpenOptions::default()
    }
}

#[doc(hidden)]
pub fn read_only() -> OpenOptions {
    OpenOptions {
        mode: OpenMode::ReadOnly,
        checkpoint_interval: Some(Duration::ZERO),
        ..OpenOptions::default()
    }
}

#[doc(hidden)]
pub fn following() -> OpenOptions {
    OpenOptions {
        mode: OpenMode::Follow,
        checkpoint_interval: Some(Duration::ZERO),
        ..OpenOptions::default()
    }
}

#[doc(hidden)]
pub use crate::publish::set_fail_after_dest;

#[doc(hidden)]
pub use crate::publish::{set_plant_destination, PlantShape, PLANTED_SENTINEL};

#[doc(hidden)]
pub fn count(db: &Database, label: &str) -> i64 {
    let rows = db
        .connect()
        .unwrap()
        .query(&format!("MATCH (n:{label}) RETURN count(n)"))
        .unwrap()
        .rows;
    match &rows[0][0] {
        Value::Int(n) => *n,
        other => panic!("count was {other:?}"),
    }
}

/// Poll `db` until `:label` holds `expect` nodes — the one follower poller
/// and timeout policy shared by the in-crate follow suite and `tests/`.
/// `count` panics on a follow error, so a stuck follower names the reason.
#[doc(hidden)]
pub fn wait_count(db: &Database, label: &str, expect: i64) {
    let start = Instant::now();
    loop {
        if count(db, label) == expect {
            return;
        }
        if start.elapsed() > Duration::from_secs(3) {
            panic!(
                "timed out waiting for {expect} :{label}, have {}",
                count(db, label)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use std::env::VarError;
    use std::ffi::OsString;

    use super::closed_preview_setting;

    /// Absent and blank settings both leave every preview feature closed.
    #[test]
    fn closed_preview_setting_accepts_absent_and_blank_values() {
        for setting in [
            Err(VarError::NotPresent),
            Ok(String::new()),
            Ok(" \t\n".to_string()),
        ] {
            assert_eq!(
                closed_preview_setting(setting),
                Ok(()),
                "a blank CYPHERLITE_PREVIEW disables every preview feature"
            );
        }
    }

    /// A leaked opt-in is reported with the variable name and the value, so a
    /// developer can find the shell that exported it.
    #[test]
    fn closed_preview_setting_names_a_leaked_opt_in() {
        let leaked = closed_preview_setting(Ok("ontology".to_string())).unwrap_err();
        assert!(leaked.contains("CYPHERLITE_PREVIEW"), "{leaked}");
        assert!(leaked.contains("ontology"), "{leaked}");

        let leaked =
            closed_preview_setting(Err(VarError::NotUnicode(OsString::from("x")))).unwrap_err();
        assert!(leaked.contains("CYPHERLITE_PREVIEW"), "{leaked}");
        assert!(leaked.contains("not valid Unicode"), "{leaked}");
    }
}
