//! The Markdown frontend: a rule document is ordinary Markdown. Each rule is
//! one fenced block whose info string names its kind: `concept`, `constraint`
//! or `group`. The block holds `key: value` metadata lines and, for concepts
//! and constraints, a nested ```cypher fence with the body. The fence is four
//! backticks so the inner fence can sit inside it.
//!
//! Fenced blocks whose info string is not a rule kind are prose or examples
//! and are skipped, contents and all.
use super::model::{self, Field, Kind, RawBlock, Reason, RuleDocument, RuleError};

/// Parse a Markdown rule document into the shared rule model.
pub fn parse(text: &str) -> Result<RuleDocument, RuleError> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks = Vec::new();
    let mut at = 0;
    while at < lines.len() {
        let Some((ticks, info)) = fence_open(lines[at]) else {
            at += 1;
            continue;
        };
        let line = at + 1;
        let kind = Kind::from_name(info);
        let Some(close) = fence_close(&lines, at + 1, ticks) else {
            if let Some(kind) = kind {
                return Err(RuleError::new(kind.name(), None, line, Reason::Unclosed));
            }
            at += 1;
            continue;
        };
        if let Some(kind) = kind {
            blocks.push(block(kind, &lines[at + 1..close], line)?);
        }
        at = close + 1;
    }
    model::build(blocks)
}

/// One rule block: its metadata lines and, when it has one, its cypher body.
fn block(kind: Kind, body: &[&str], line: usize) -> Result<RawBlock, RuleError> {
    let (meta, cypher) = split_body(kind, body, line)?;
    let fields = metadata(kind, &meta, line)?;
    Ok(RawBlock {
        kind,
        line,
        file: None,
        fields,
        body: cypher,
    })
}

/// Split a block into its metadata lines and the body of its nested
/// ```cypher fence (`None` when the block has no such fence).
fn split_body<'a>(
    kind: Kind,
    body: &[&'a str],
    line: usize,
) -> Result<(Vec<&'a str>, Option<String>), RuleError> {
    let Some(open) = body
        .iter()
        .position(|text| matches!(fence_open(text), Some((_, "cypher"))))
    else {
        return Ok((body.to_vec(), None));
    };
    let (ticks, _) = fence_open(body[open]).unwrap_or((3, ""));
    let Some(close) = fence_close(body, open + 1, ticks) else {
        return Err(RuleError::new(kind.name(), None, line, Reason::Unclosed));
    };
    if body[close + 1..].iter().any(|text| !text.trim().is_empty()) {
        return Err(RuleError::new(
            kind.name(),
            None,
            line,
            Reason::Invalid("text after the cypher body; put prose outside the rule block".into()),
        ));
    }
    let cypher = body[open + 1..close].join("\n").trim().to_string();
    Ok((body[..open].to_vec(), Some(cypher)))
}

/// `key: value` lines. Blank lines are ignored; a line that is not a pair is
/// an error. Which keys a kind accepts is checked by the shared model.
fn metadata(kind: Kind, meta: &[&str], line: usize) -> Result<Vec<Field>, RuleError> {
    let mut fields = Vec::new();
    for (offset, text) in meta.iter().enumerate() {
        let at = line + 1 + offset;
        if text.trim().is_empty() {
            continue;
        }
        match text.split_once(':') {
            Some((key, value)) if model::is_key(key.trim()) => fields.push(Field {
                key: key.trim().to_string(),
                value: value.trim().to_string(),
                line: at,
            }),
            _ => {
                return Err(RuleError::new(
                    kind.name(),
                    None,
                    line,
                    Reason::Invalid(format!("line {at} is not a 'key: value' pair")),
                ));
            }
        }
    }
    Ok(fields)
}

/// A line that opens a fence: the backtick count and the info string.
fn fence_open(line: &str) -> Option<(usize, &str)> {
    let text = line.trim_start();
    let ticks = text.bytes().take_while(|b| *b == b'`').count();
    (ticks >= 3).then(|| (ticks, text[ticks..].trim()))
}

/// The line that closes a fence opened with `ticks` backticks: at least that
/// many backticks and nothing else.
fn fence_close(lines: &[&str], from: usize, ticks: usize) -> Option<usize> {
    (from..lines.len()).find(|&at| {
        let text = lines[at].trim();
        text.len() >= ticks && text.bytes().all(|b| b == b'`')
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch_rules::{Severity, parse_markdown};

    const DOC: &str = "\
# Layers

Prose is ignored, and so is this example:

`````markdown
````concept
id: arch:Example
````
`````

````concept
id: arch:Layer/Persistence
requires: [arch:Module/*]
severity: MINOR
description: Persistence modules
```cypher
MATCH (m:Module) WHERE m.name =~ '.*\\.db'
MERGE (l:Layer {name: 'Persistence'})
```
````

````constraint
id: arch:NoCycles
severity: BLOCKER
description: No module cycles
```cypher
MATCH p = (m1:Module)-[:DEPENDS_ON*2..6]->(m1) RETURN p
```
````

````group
id: arch:default
include: [arch:*]
overrides: [arch:NoCycles=MAJOR]
description: Default pack
````
";

    fn error(text: &str) -> RuleError {
        parse(text).expect_err("document should be rejected")
    }

    #[test]
    fn parses_concepts_constraints_and_groups() {
        let document = parse(DOC).unwrap();
        assert_eq!(document.concepts.len(), 1);
        assert_eq!(document.constraints.len(), 1);
        assert_eq!(document.groups.len(), 1);

        let concept = &document.concepts[0];
        assert_eq!(concept.id, "arch:Layer/Persistence");
        assert_eq!(concept.requires, vec!["arch:Module/*"]);
        assert_eq!(concept.severity, Severity::Minor);
        assert_eq!(concept.description, "Persistence modules");
        assert!(concept.cypher.starts_with("MATCH (m:Module)"));
        assert!(concept.cypher.ends_with("MERGE (l:Layer {name: 'Persistence'})"));
        assert_eq!(concept.line, 11);

        let constraint = &document.constraints[0];
        assert_eq!(constraint.severity, Severity::Blocker);
        assert!(constraint.requires.is_empty());

        let group = &document.groups[0];
        assert_eq!(group.include, vec!["arch:*"]);
        assert_eq!(group.overrides, vec![("arch:NoCycles".to_string(), Severity::Major)]);
    }

    #[test]
    fn concept_severity_defaults_to_info() {
        let document = parse(
            "````concept\nid: arch:Plain\n```cypher\nMATCH (n) RETURN n\n```\n````\n",
        )
        .unwrap();
        assert_eq!(document.concepts[0].severity, Severity::Info);
    }

    #[test]
    fn rule_sha_is_stable_across_reparses_and_moves_with_content() {
        let first = parse(DOC).unwrap();
        let again = parse(DOC).unwrap();
        assert_eq!(first.concepts[0].rule_sha, again.concepts[0].rule_sha);
        assert_eq!(first.groups[0].rule_sha, again.groups[0].rule_sha);

        // Moving a block down the document keeps its content digest.
        let shifted = parse(&format!("\n\n{DOC}")).unwrap();
        assert_eq!(first.constraints[0].rule_sha, shifted.constraints[0].rule_sha);
        assert_ne!(first.constraints[0].line, shifted.constraints[0].line);

        let edited = parse(&DOC.replace("MAJOR", "MINOR")).unwrap();
        assert_ne!(first.groups[0].rule_sha, edited.groups[0].rule_sha);
        let reworded = parse(&DOC.replace("MATCH p", "MATCH q")).unwrap();
        assert_ne!(first.constraints[0].rule_sha, reworded.constraints[0].rule_sha);
    }

    #[test]
    fn constraint_without_severity_names_block_and_line() {
        let text = "# Doc\n\n````constraint\nid: arch:NoSeverity\n```cypher\nMATCH (n) RETURN n\n```\n````\n";
        let error = error(text);
        assert_eq!(error.line, 3);
        assert_eq!(error.id.as_deref(), Some("arch:NoSeverity"));
        assert_eq!(error.reason, Reason::MissingKey("severity"));
        assert_eq!(
            error.to_string(),
            "constraint block 'arch:NoSeverity' at line 3: missing required key 'severity'"
        );
    }

    #[test]
    fn unknown_key_and_bad_severity_name_the_block() {
        let unknown = error(
            "````concept\nid: arch:Typo\nseverty: INFO\n```cypher\nMATCH (n) RETURN n\n```\n````\n",
        );
        assert_eq!(unknown.line, 1);
        assert!(unknown.to_string().contains("concept block 'arch:Typo' at line 1"));
        assert!(unknown.to_string().contains("unknown key 'severty'"));

        let severity = error(
            "````constraint\nid: arch:Loud\nseverity: LOUD\n```cypher\nMATCH (n) RETURN n\n```\n````\n",
        );
        assert!(severity.to_string().contains("severity 'LOUD'"));
        assert_eq!(severity.line, 1);
    }

    #[test]
    fn structural_faults_are_typed() {
        let duplicate = error(&format!("{DOC}\n````concept\nid: arch:NoCycles\n```cypher\nRETURN 1\n```\n````\n"));
        assert_eq!(duplicate.reason, Reason::DuplicateId);
        assert_eq!(duplicate.id.as_deref(), Some("arch:NoCycles"));

        let body = error("````concept\nid: arch:NoBody\nseverity: INFO\n````\n");
        assert_eq!(body.reason, Reason::MissingBody);

        let unclosed = error("# Doc\n\n````concept\nid: arch:Open\n");
        assert_eq!(unclosed.line, 3);
        assert_eq!(unclosed.reason, Reason::Unclosed);

        let wildcard = error("````group\nid: arch:Bad\ninclude: [arch:*Foo]\n````\n");
        assert!(wildcard.to_string().contains("'*' is allowed only as the last character"));

        let empty_group = error("````group\nid: arch:Empty\ninclude: []\n````\n");
        assert!(empty_group.to_string().contains("at least one rule"));
    }

    #[test]
    fn format_doc_example_parses() {
        // The example in docs/architecture-rules.md is the documented layout;
        // it must parse, and the outer fence must hide it from the document.
        let doc = include_str!("../../../../docs/architecture-rules.md");
        let start = doc.find("`````markdown\n# Architecture rules").unwrap() + "`````markdown\n".len();
        let end = start + doc[start..].find("\n`````\n").unwrap();
        let example = parse(&doc[start..end]).unwrap();
        assert_eq!(example.concepts.len(), 2);
        assert_eq!(example.constraints.len(), 2);
        assert_eq!(example.groups.len(), 1);
        assert!(parse(doc).unwrap().concepts.is_empty());
    }

    #[test]
    fn foreign_fences_are_skipped_whole() {
        // A rule block shown inside a five-backtick Markdown example is prose,
        // and a later real rule still parses.
        let text = "```rust\nfn main() {}\n```\n\n`````markdown\n````concept\nid: arch:Shown\n```cypher\nRETURN 1\n```\n````\n`````\n\n````concept\nid: arch:Real\n```cypher\nRETURN 2\n```\n````\n";
        let document = parse_markdown(text).unwrap();
        assert_eq!(document.concepts.len(), 1);
        assert_eq!(document.concepts[0].id, "arch:Real");
    }
}
