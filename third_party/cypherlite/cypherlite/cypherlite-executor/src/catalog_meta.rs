//! Neutral catalog metadata shared by the built-in function and procedure
//! registries (CIP-84/CIP-113/CIP-157): one formal parameter/return-column
//! shape and one typed deprecation record.
//!
//! Both [`crate::function_registry`] and [`crate::procedure_registry`]
//! import these types, so neither registry owns the other's vocabulary.
//! [`DeprecatedBy`] and [`DeprecationForm`] are re-exported at the crate
//! root because the notification readers outside this crate name them
//! (`cypherlite_executor::{DeprecatedBy, DeprecationForm}`). [`ProcArgMeta`]
//! stays crate-internal: a caller outside this crate reads a public
//! [`crate::ProcSignature`]'s parameters through
//! `ProcSignature::parameters`/`return_columns`, never by naming the
//! parameter type, so it needs no public path of its own.

/// One formal parameter or return column of a built-in function or
/// procedure: the `name`/`description`/`type`/`isDeprecated`/`default`
/// shape CIP-84's `argumentDescription`/`returnDescription` maps carry
/// (`default` only when the parameter actually has one).
pub(crate) struct ProcArgMeta {
    pub name: &'static str,
    pub description: &'static str,
    pub type_str: &'static str,
    pub deprecated: bool,
    pub default: Option<&'static str>,
}

impl ProcArgMeta {
    /// Compact constructor for the common case: no description, not
    /// deprecated, no default.
    pub(crate) const fn new(name: &'static str, type_str: &'static str) -> Self {
        Self::described(name, "", type_str)
    }

    /// Compact constructor for a parameter with a default value; not
    /// deprecated.
    pub(crate) const fn with_default(
        name: &'static str,
        description: &'static str,
        type_str: &'static str,
        default: &'static str,
    ) -> Self {
        Self {
            name,
            description,
            type_str,
            deprecated: false,
            default: Some(default),
        }
    }

    /// Compact constructor with a description; not deprecated, no default.
    pub(crate) const fn described(
        name: &'static str,
        description: &'static str,
        type_str: &'static str,
    ) -> Self {
        Self {
            name,
            description,
            type_str,
            deprecated: false,
            default: None,
        }
    }
}

/// A catalog entry's replacement (CIP-157's `deprecatedBy`): the display
/// text `SHOW FUNCTIONS`/`SHOW PROCEDURES` render and the replacement's
/// syntactic form. The form keeps a function/procedure identifier apart
/// from a Cypher clause, so a notification remedy can name each one for
/// what it is instead of guessing from the string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeprecatedBy {
    /// The exact `deprecatedBy` cell CIP-157 pins for the catalog rows.
    display: &'static str,
    form: DeprecationForm,
}

/// The replacement's syntactic form (the notification remedy's rendering).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeprecationForm {
    /// A function or procedure identifier, optionally a call
    /// (`elementId`, `elementId(n)`).
    Identifier(&'static str),
    /// A Cypher clause (`SEARCH`).
    Clause(&'static str),
}

impl DeprecatedBy {
    /// A replacement whose catalog display text is the identifier itself.
    pub const fn identifier(identifier: &'static str) -> Self {
        Self {
            display: identifier,
            form: DeprecationForm::Identifier(identifier),
        }
    }

    /// A replacement whose catalog display text is the clause's name.
    pub const fn clause(clause: &'static str) -> Self {
        Self {
            display: clause,
            form: DeprecationForm::Clause(clause),
        }
    }

    /// A replacement whose catalog display is prose prefixed by the
    /// identifier to render (CIP-157's `id` cell).
    pub const fn identified(identifier: &'static str, display: &'static str) -> Self {
        Self {
            display,
            form: DeprecationForm::Identifier(identifier),
        }
    }

    /// The catalog's `deprecatedBy` display text.
    pub fn display(self) -> &'static str {
        self.display
    }

    /// The replacement's syntactic form.
    pub fn form(self) -> DeprecationForm {
        self.form
    }
}
