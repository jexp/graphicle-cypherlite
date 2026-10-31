//! Parsing: bytes → the ordered value model (ADR-0227 §3/§4).
//!
//! A strict-JSON sniff comes first: input that parses as JSON is parsed as
//! JSON (via serde_json into the ordered [`Value`]); anything else is parsed
//! as YAML 1.2 via `saphyr`. Both paths land in the same ordered value
//! model, so both syntaxes behave identically after parsing.

use saphyr::{LoadableYamlNode, Scalar, Yaml};
use saphyr_parser::{Event, Parser};

use crate::error::OntologyError;
use crate::value::Value;

/// The syntax of an ontology document, detected by a strict JSON parse
/// (ADR-0227 §3: a document that parses as JSON counts as JSON, anything
/// else as YAML).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentFormat {
    /// The document parses as strict JSON.
    Json,
    /// Anything else (parsed as YAML 1.2).
    Yaml,
}

impl DocumentFormat {
    /// The `db.ontology.get` format argument naming this format
    /// (CIP-287: `"json"` or `"yaml"`).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            DocumentFormat::Json => "json",
            DocumentFormat::Yaml => "yaml",
        }
    }
}

/// Detect the stored format of a document by a strict JSON parse
/// (ADR-0227 §3).
#[must_use]
pub fn detect_format(bytes: &[u8]) -> DocumentFormat {
    if serde_json::from_slice::<serde::de::IgnoredAny>(bytes).is_ok() {
        DocumentFormat::Json
    } else {
        DocumentFormat::Yaml
    }
}

/// The deepest structural nesting an ontology document may use — the same
/// 128-level cap serde_json enforces natively on the JSON path. On the
/// YAML path the cap is screened from the `saphyr_parser` event stream
/// before the value tree is built: saphyr recurses per nesting level with
/// no cap of its own, so without the screen a deeply nested document (e.g.
/// a one-line `"- "` × 100_000 run) overflows the stack and aborts the
/// process instead of failing with a typed error.
const MAX_DOCUMENT_DEPTH: usize = 128;

/// Parse raw document bytes into the ordered value model, sniffing the
/// syntax per ADR-0227 §3. Constructs outside the value model — non-string
/// mapping keys, custom tags, anchors and aliases, non-finite floats,
/// multiple YAML documents — are rejected here (ADR-0227 §4).
pub fn parse_document_value(bytes: &[u8]) -> Result<(Value, DocumentFormat), OntologyError> {
    let text = std::str::from_utf8(bytes)?;
    match detect_format(bytes) {
        DocumentFormat::Json => {
            let value = serde_json::from_str::<Value>(text).map_err(|e| {
                // serde_json enforces a 128-level recursion limit natively;
                // the strict-JSON sniff uses its non-recursive `IgnoredAny`
                // path, so a too-deep document reaches this parse and is
                // rejected here. serde_json exposes no recursion-limit
                // classifier (no `is_recursion_limit` in 1.0); the error
                // message is the stable surface it gives us.
                if e.is_syntax() && e.to_string().starts_with("recursion limit exceeded") {
                    OntologyError::NestingTooDeep {
                        limit: MAX_DOCUMENT_DEPTH,
                    }
                } else {
                    OntologyError::Syntax {
                        syntax: "JSON",
                        message: e.to_string(),
                    }
                }
            })?;
            Ok((value, DocumentFormat::Json))
        }
        DocumentFormat::Yaml => parse_yaml(text).map(|v| (v, DocumentFormat::Yaml)),
    }
}

/// Pre-parse screen for the YAML path (ADR-0227 §3/§4): one pass over the
/// non-recursive `saphyr_parser` event stream, rejecting what must not be
/// allowed to reach the recursive `Yaml::load_from_str` tree builder.
///
/// * **Depth** — count open collections and fail with
///   [`OntologyError::NestingTooDeep`] as soon as more than
///   [`MAX_DOCUMENT_DEPTH`] are open at once. The event parser is
///   non-recursive (an explicit state stack, verified against the `"- "`
///   × 100_000 attack payload), so the screen itself cannot overflow.
///   Every `SequenceStart`/`MappingStart` is one level, for block and flow
///   style alike; quoted scalars, comments and block-scalar content never
///   produce collection events, so they cannot inflate or spoof the count.
/// * **Anchors and aliases** — fail with
///   [`OntologyError::UnsupportedConstruct`] at the first `Event::Alias`
///   or anchor-carrying event (a nonzero anchor ID; saphyr anchor IDs
///   start at 1, 0 means no anchor). The value model has no alias concept
///   (ADR-0227 §4), and saphyr's loader resolves aliases *eagerly*, by
///   cloning the anchored subtree for each reference while building the
///   tree, so a small alias-chain document — the billion-laughs shape, a
///   ~600 B 10-way fan-out — expands into gigabytes during
///   `Yaml::load_from_str`: past the depth screen above (event depth ~4)
///   and before any post-parse envelope check can run, riding a fully
///   valid document via an extension payload (which validation checks in
///   place and carries on the root value tree). The screen rejects at the
///   first anchor event, before any anchored subtree is expanded. No
///   representable document is lost: an anchor carries no data, and an
///   alias only denotes a copy of one, which can always be written out
///   literally.
///
/// The scanner's own flow-nesting cap surfaces as the same typed depth
/// error; a reference to an undefined anchor fails inside the parser
/// before any `Event::Alias` is emitted, so the screen reports that error
/// as the same construct rejection; any other scan error ends the screen
/// early — the real parser below reports the syntax error.
fn screen_yaml_events(text: &str) -> Result<(), OntologyError> {
    let mut depth = 0usize;
    for item in Parser::new_from_str(text) {
        match item {
            // An anchor on any node kind (scalar, sequence, mapping), or
            // an alias reference to one.
            Ok((
                Event::Scalar(_, _, anchor_id, _)
                | Event::SequenceStart(anchor_id, _)
                | Event::MappingStart(anchor_id, _),
                _,
            )) if anchor_id > 0 => return Err(anchors_and_aliases_not_supported()),
            Ok((Event::Alias(_), _)) => return Err(anchors_and_aliases_not_supported()),
            Ok((Event::SequenceStart(..) | Event::MappingStart(..), _)) => {
                depth += 1;
                if depth > MAX_DOCUMENT_DEPTH {
                    return Err(OntologyError::NestingTooDeep {
                        limit: MAX_DOCUMENT_DEPTH,
                    });
                }
            }
            Ok((Event::SequenceEnd | Event::MappingEnd, _)) => depth -= 1,
            Ok(_) => {}
            Err(e) => {
                // The scanner caps flow nesting on its own: its
                // `flow_level` is a u8 and overflows at 256 nested flow
                // collections. That can only trip past this screen's own
                // cap, so surface it as the same typed error. A reference
                // to an undefined anchor never emits an `Event::Alias` —
                // the parser fails first — so report that error as the
                // same construct rejection. Any other scan error ends the
                // screen — the real parse below reports the syntax error.
                return match e.info() {
                    "recursion limit exceeded" => Err(OntologyError::NestingTooDeep {
                        limit: MAX_DOCUMENT_DEPTH,
                    }),
                    "while parsing node, found unknown anchor" => {
                        Err(anchors_and_aliases_not_supported())
                    }
                    _ => Ok(()),
                };
            }
        }
    }
    Ok(())
}

/// The typed rejection for every anchor and alias (ADR-0227 §4): the
/// JSON-equivalent value model has no alias concept, and saphyr's loader
/// expands alias chains by value-copy, so they are refused in the
/// pre-parse event screen — before the loader can expand them.
fn anchors_and_aliases_not_supported() -> OntologyError {
    OntologyError::UnsupportedConstruct("anchors and aliases are not supported".to_owned())
}

/// Parse YAML 1.2 text into the ordered value model.
///
/// The pre-parse screen ([`screen_yaml_events`]) runs first: saphyr's
/// loader resolves aliases eagerly (by cloning anchored subtrees) and
/// recurses per nesting level, so the screen rejects anchors and aliases
/// — and over-deep nesting — before `load_from_str` can expand or
/// recurse on them.
fn parse_yaml(text: &str) -> Result<Value, OntologyError> {
    screen_yaml_events(text)?;
    let docs = Yaml::load_from_str(text).map_err(|e| OntologyError::Syntax {
        syntax: "YAML",
        message: e.to_string(),
    })?;
    match docs.len() {
        0 => Err(OntologyError::Syntax {
            syntax: "YAML",
            message: "the document is empty".to_owned(),
        }),
        1 => yaml_to_value(&docs[0]),
        n => Err(OntologyError::UnsupportedConstruct(format!(
            "expected a single YAML document, found {n} (multi-document input is not supported)"
        ))),
    }
}

/// Convert one parsed YAML node into the JSON-equivalent value model,
/// rejecting everything outside it (ADR-0227 §4).
fn yaml_to_value(node: &Yaml<'_>) -> Result<Value, OntologyError> {
    match node {
        Yaml::Value(scalar) => Ok(match scalar {
            Scalar::Null => Value::Null,
            Scalar::Boolean(b) => Value::Boolean(*b),
            Scalar::Integer(i) => Value::Integer(*i),
            Scalar::FloatingPoint(f) => {
                let f = f.into_inner();
                if f.is_finite() {
                    Value::Float(f)
                } else {
                    return Err(OntologyError::UnsupportedConstruct(
                        "non-finite floats (.inf/.nan) are not supported".to_owned(),
                    ));
                }
            }
            Scalar::String(s) => Value::String(s.to_string()),
        }),
        Yaml::Sequence(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(yaml_to_value(item)?);
            }
            Ok(Value::Array(out))
        }
        Yaml::Mapping(mapping) => {
            let mut map = indexmap::IndexMap::with_capacity(mapping.len());
            for (key, value) in mapping {
                let key = match key {
                    Yaml::Value(Scalar::String(s)) => s.to_string(),
                    _ => {
                        return Err(OntologyError::UnsupportedConstruct(
                            "non-string mapping keys are not supported".to_owned(),
                        ));
                    }
                };
                // Check-then-move, converting first so a bad value keeps
                // reporting before a duplicate key: the key is cloned
                // nowhere on the success path.
                let converted = yaml_to_value(value)?;
                if map.contains_key(&key) {
                    return Err(OntologyError::UnsupportedConstruct(format!(
                        "duplicate mapping key '{key}'"
                    )));
                }
                map.insert(key, converted);
            }
            Ok(Value::Object(map))
        }
        Yaml::Tagged(tag, _) => Err(OntologyError::UnsupportedConstruct(format!(
            "custom tags are not supported (found tag '{tag}')"
        ))),
        // Unreachable via `load_from_str`: the loader resolves aliases
        // eagerly by cloning, and the pre-parse screen rejects every
        // anchor and alias before it runs. Kept so the conversion still
        // fails closed, in the same error family, if a future loader ever
        // hands it an unresolved alias node.
        Yaml::Alias(id) => Err(OntologyError::UnsupportedConstruct(format!(
            "anchors and aliases are not supported (unresolved alias, anchor id {id})"
        ))),
        Yaml::Representation(..) | Yaml::BadValue => Err(OntologyError::Syntax {
            syntax: "YAML",
            message: "unresolvable scalar representation".to_owned(),
        }),
    }
}
