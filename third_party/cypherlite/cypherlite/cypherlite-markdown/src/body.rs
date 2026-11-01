//! Node file body rules (ADR-0406 §2, ADR-0460): the body is the text of
//! the node's ordered body properties — the first renders bare, each
//! subsequent one as a `## <property>` section; a single present property
//! is the bare body 1:1, and headings inside a section are markdown
//! text, not structure.

use cypherlite_core::{Error, Result};

/// The one property that renders as the file body when no list is
/// configured — ADR-0406's rule, ADR-0460's default list `["body"]`.
/// The storage codec's default-list resolution consults it.
pub const BODY_KEY: &str = "body";

fn body_error(message: impl Into<String>) -> Error {
    Error::storage(format!("markdown vault body: {}", message.into()))
}

/// The body's NUL contract, one spelling for both directions: a NUL
/// byte cannot survive a body round trip, so the parser and the writer
/// both refuse it — the writer never emits a file the next open would
/// reject.
fn reject_nul(text: &str) -> Result<()> {
    if text.contains('\u{0}') {
        return Err(body_error("NUL byte in body text"));
    }
    Ok(())
}

/// Can this text be a bare body segment? The render→parse cycle is the
/// rule: a segment renders surrounded by separators and the next parse
/// normalizes outer whitespace away, so the text survives as itself iff
/// it is non-empty and carries no leading or trailing whitespace — blank
/// or outer-whitespace text demotes to the frontmatter property exactly
/// like a non-string value. The one body-representability rule every
/// consumer consults (the reader [`NodeDoc::body_values`](crate::doc::NodeDoc)
/// and the section parse below).
pub(crate) fn representable_as_body(text: &str) -> bool {
    !text.is_empty() && text == text.trim()
}

/// Parse the text after the frontmatter fence into the body property's
/// value. Leading blank lines and trailing whitespace are normalized
/// away; interior text — including headings — is preserved verbatim.
/// The normalization is the reader's tolerance for hand-authored
/// layout, and it only ever moves text toward the canonical form: the
/// writer's half of the same domain is [`representable_as_body`], which
/// refuses outer whitespace, so a rendered file always round-trips
/// through this parse unchanged.
pub fn parse_body(text: &str) -> Result<String> {
    reject_nul(text)?;
    Ok(text.trim().to_string())
}

/// Render the body text for a file: nothing when there is no body,
/// otherwise a leading blank line (the fence/body separator) plus the
/// body. Refuses a NUL body and outer whitespace with authored errors
/// (the reject_nul precedent), so a checkpoint never writes a file the
/// next open would silently rewrite — the two ends of the body domain
/// agree by construction, not by prose.
pub(crate) fn render_body(body: &str) -> Result<String> {
    reject_nul(body)?;
    if body.is_empty() {
        return Ok(String::new());
    }
    if !representable_as_body(body) {
        return Err(body_error(
            "body text carries leading or trailing whitespace the next open would \
             normalize away",
        ));
    }
    Ok(format!("\n{body}\n"))
}

/// The section boundary line for one non-first body property: an exact
/// ATX heading, no leading indent, its text exactly the property name.
/// The one marker spelling the parse below recognizes and the render
/// below emits — never a second, forgiving matcher.
fn section_heading(name: &str) -> String {
    format!("{ATX_PREFIX}{name}")
}

/// The one heading-recognition spelling both halves of the section
/// grammar consult: a line is a marker heading iff it is an exact ATX
/// heading (no leading indent) whose first word — compared through the
/// case fold, like every other identity in the vault — names a
/// non-first body property. The marker word is a boundary, so
/// `## notebook` never matches `notes`; the rest of the line after it
/// is the section's first content line, keeping its heading: `## notes
/// Title of the notes` opens the `notes` section with
/// `## Title of the notes` as its first line — a titled section stays a
/// heading, and the property's value never carries the marker word —
/// and
/// `## Notes` matches `notes` exactly like `## notes`. The parse treats
/// a match as a section boundary; the writer's [`reject_marker_lines`]
/// refuses the same match inside a value — one recognition, never two
/// that can drift.
fn marker_heading<'a>(line: &'a str, props: &[String]) -> Option<(usize, &'a str)> {
    let heading = line.trim_end().strip_prefix(ATX_PREFIX)?;
    let (marker, rest) = heading
        .split_once(char::is_whitespace)
        .unwrap_or((heading, ""));
    let rest = rest.trim();
    props.iter().enumerate().skip(1).find_map(|(index, name)| {
        // The vault's one identity fold — the same fold
        // `validate_body_properties` judges duplicates through.
        (fold_case(name) == fold_case(marker)).then_some((index, rest))
    })
}

/// The vault's one identity relation spelled as a function (the storage
/// codec's `fold_case` twin): the Unicode case fold that marker
/// recognition and duplicate validation both consult — never an
/// ASCII-only shortcut, which would silently re-attribute non-ASCII
/// names' sections.
pub fn fold_case(value: &str) -> String {
    value.to_lowercase()
}

/// The section grammar's one ATX literal: the prefix every marker
/// heading is recognized by, every section heading is rendered with,
/// and every kept title heading re-emits.
const ATX_PREFIX: &str = "## ";

/// Parse the text after the frontmatter fence into the present body
/// properties' values (ADR-0460): `props[0]` owns the text before the
/// first section boundary, each later entry owns the `## <name>`-headed
/// section of its name. Returns the present pairs in `props` order —
/// the file's section order is layout, never identity. Fails closed on
/// a section that appears twice; a blank preamble or section leaves its
/// property absent; an empty `props` list reads no body at all.
pub(crate) fn parse_body_properties(
    text: &str,
    props: &[String],
    file: &str,
) -> Result<Vec<(String, String)>> {
    reject_nul(text)?;
    // An empty list reads no body at all — there is no segment 0 to
    // accumulate into.
    if props.is_empty() {
        return Ok(Vec::new());
    }
    // One state table per property — segment 0 IS the first property's
    // (the preamble); the cursor starts there and a marker heading moves
    // it. A segment's presence is its marker's appearance record: no
    // parallel `seen` table, the appears-twice refusal reads the same
    // Option the text accumulates in.
    let mut segments: Vec<Option<String>> = vec![None; props.len()];
    let mut current = 0usize;
    for line in text.split('\n') {
        if let Some((index, trailing)) = marker_heading(line, props) {
            if segments[index].is_some() {
                return Err(body_error(format!(
                    "{file}: body section '{}' appears twice",
                    props[index]
                )));
            }
            // The marker arm always creates the segment — even for a
            // bare marker with no text — so the existence check above
            // is the appears-twice record, and a section with no
            // content still parses to an absent property below.
            let segment = segments[index].get_or_insert_with(String::new);
            current = index;
            // The marker's trailing text keeps its heading: `## notes
            // Title of the notes` opens the section with
            // `## Title of the notes` as its first line, so a titled
            // section stays a heading in the note. A whitespace-only
            // remainder was an artificial marker line (`## notes`) and
            // contributes nothing.
            if !trailing.is_empty() {
                segment.push_str(ATX_PREFIX);
                segment.push_str(trailing);
                segment.push('\n');
            }
            continue;
        }
        let segment = segments[current].get_or_insert_with(String::new);
        segment.push_str(line);
        segment.push('\n');
    }
    let mut values = Vec::new();
    for (index, name) in props.iter().enumerate() {
        // Every segment is parsed by the one `parse_body` tolerance, so
        // a section's hand-authored layout is normalized exactly like
        // the bare body's.
        let segment = segments[index].as_deref().unwrap_or("");
        let parsed = parse_body(segment)?;
        if representable_as_body(&parsed) {
            values.push((name.clone(), parsed));
        }
    }
    Ok(values)
}

/// Render the present body properties' values as the file body
/// (ADR-0460): the list's **first entry alone is the bare body 1:1**
/// (ADR-0406's form); every other present property renders under its
/// `## <name>` heading, in `props` order. The bare slot belongs to the
/// first *list* entry, never to the first *present* value — a later
/// entry rendered bare would re-parse as the first entry's text, so it
/// keeps its heading even when alone. The writer's fail-closed half of
/// the section grammar: a value containing another property's heading
/// line would be re-split at the next open (silently for that property,
/// as a duplicate for its own), so the render refuses it — the
/// representable_as_body precedent, one grammar class wider.
/// `values` are the present pairs in `props` order (the shape
/// [`NodeDoc::body_values`](crate::doc::NodeDoc) produces) — borrowed,
/// never re-collected owned copies.
pub(crate) fn render_body_properties(props: &[String], values: &[(&str, &str)]) -> Result<String> {
    let mut text = String::new();
    for (name, value) in values {
        // The precondition enforced, not assumed: a value naming a
        // property outside the list would render a heading the next
        // open never treats as a boundary — its text silently absorbed
        // into the preceding segment.
        if !props.iter().any(|listed| listed == name) {
            return Err(body_error(format!(
                "body property '{name}' is not in the configured list"
            )));
        }
        if props.first().map(String::as_str) == Some(*name) {
            text.push_str(value);
        } else {
            // The blank-line separator sits *between* segments: a
            // section-only body starts on its heading line, or the
            // outer-whitespace refusal below would fire on the render's
            // own output.
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&section_heading(name));
            text.push_str("\n\n");
            text.push_str(value);
        }
    }
    reject_marker_lines(props, values)?;
    render_body(&text)
}

/// The section grammar's writer guard: no present value may carry any
/// non-first property's heading line — the next open would split the
/// file there, misreading the layout as structure. The recognition is
/// [`marker_heading`], the parse side's own — the two ends cannot
/// drift.
fn reject_marker_lines(props: &[String], values: &[(&str, &str)]) -> Result<()> {
    for (name, value) in values {
        for line in value.split('\n') {
            if marker_heading(line, props).is_some() {
                return Err(body_error(format!(
                    "body property '{name}' contains a '{}' section heading line \
                     the next open would re-split",
                    line.trim_end()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body_props;

    #[test]
    fn bare_body_round_trips() {
        let text = "\nKing of Ithaca.\n";
        assert_eq!(parse_body(text).unwrap(), "King of Ithaca.");
        assert_eq!(render_body("King of Ithaca.").unwrap(), text);
    }

    #[test]
    fn headings_are_text_not_structure() {
        let text = "\n## Weird opening\n\nBut it is the body.\n";
        let parsed = parse_body(text).unwrap();
        assert_eq!(parsed, "## Weird opening\n\nBut it is the body.");
        assert_eq!(parse_body(&render_body(&parsed).unwrap()).unwrap(), parsed);
        // A one-property list keeps exactly that rule: no section
        // boundaries exist, so every heading is the body's own text.
        let list = body_props(&["body"]);
        let values = parse_body_properties(text, &list, "f.md").unwrap();
        assert_eq!(
            values,
            vec![(
                "body".into(),
                "## Weird opening\n\nBut it is the body.".into()
            )]
        );
        assert_eq!(
            render_body_properties(
                &list,
                &[("body", "## Weird opening\n\nBut it is the body.")]
            )
            .unwrap(),
            render_body("## Weird opening\n\nBut it is the body.").unwrap()
        );
    }

    #[test]
    fn blank_body_is_empty() {
        assert_eq!(parse_body("\n  \n").unwrap(), "");
        assert_eq!(render_body("").unwrap(), "");
        assert!(
            parse_body_properties("\n  \n", &body_props(&["body"]), "f.md")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn interior_text_is_kept_verbatim() {
        let text = "\nline1\n\n  indented\n\n\ttabbed\n";
        assert_eq!(parse_body(text).unwrap(), "line1\n\n  indented\n\n\ttabbed");
    }

    #[test]
    fn nul_fails_closed_in_both_directions() {
        for text in ["a\u{0}b", "\u{0}"] {
            assert!(parse_body(text).is_err(), "parse accepted {text:?}");
            assert!(render_body(text).is_err(), "render accepted {text:?}");
            assert!(
                parse_body_properties(text, &body_props(&["a", "b"]), "f.md").is_err(),
                "section parse accepted {text:?}"
            );
        }
    }

    #[test]
    fn representable_is_the_roundtrip_domain() {
        // Blank text and outer-whitespace text alike fail the
        // render→parse cycle, so both demote to the frontmatter
        // property; interior whitespace is preserved verbatim
        // and stays representable.
        for text in ["", " ", "\n\t ", " x ", "x ", " x"] {
            assert!(!representable_as_body(text), "{text:?} must demote");
        }
        assert!(representable_as_body("x"));
        assert!(representable_as_body("line1\n\n  indented\n\ttabbed"));
    }

    #[test]
    fn outer_whitespace_body_fails_closed_at_render() {
        // The writer refuses a body the next parse would normalize away
        // (the reject_nul precedent) instead of emitting a file the
        // next open silently rewrites.
        for text in [" x", "x ", " x ", "\nx"] {
            assert!(render_body(text).is_err(), "render accepted {text:?}");
        }
        assert_eq!(render_body("x").unwrap(), "\nx\n");
    }

    #[test]
    fn one_present_property_is_the_bare_body() {
        let list = body_props(&["summary", "notes"]);
        let text = "\nJust the summary.\n";
        let values = parse_body_properties(text, &list, "f.md").unwrap();
        assert_eq!(values, vec![("summary".into(), "Just the summary.".into())]);
        assert_eq!(
            render_body_properties(&list, &[("summary", "Just the summary.")]).unwrap(),
            "\nJust the summary.\n"
        );
        // A later entry present alone keeps its heading: rendered bare
        // it would re-parse as the first entry's text.
        assert_eq!(
            render_body_properties(&list, &[("notes", "Only notes.")]).unwrap(),
            "\n## notes\n\nOnly notes.\n"
        );
        assert_eq!(
            parse_body_properties("\n## notes\n\nOnly notes.\n", &list, "f.md").unwrap(),
            vec![("notes".into(), "Only notes.".into())]
        );
    }

    #[test]
    fn several_present_properties_render_first_bare_then_sections() {
        let list = body_props(&["summary", "notes", "review"]);
        let values = vec![
            ("summary".to_string(), "The primary text.".to_string()),
            ("notes".to_string(), "Scratch thoughts.".to_string()),
            ("review".to_string(), "Verdict: keep.".to_string()),
        ];
        let rendered = render_body_properties(
            &list,
            &[
                ("summary", "The primary text."),
                ("notes", "Scratch thoughts."),
                ("review", "Verdict: keep."),
            ],
        )
        .unwrap();
        assert_eq!(
            rendered,
            "\nThe primary text.\n\n## notes\n\nScratch thoughts.\n\n## review\n\nVerdict: keep.\n"
        );
        // File order is layout: a hand-shuffled file parses to the same
        // configured-order values.
        let shuffled =
            "\nThe primary text.\n\n## review\n\nVerdict: keep.\n\n## notes\n\nScratch thoughts.\n";
        assert_eq!(
            parse_body_properties(shuffled, &list, "f.md").unwrap(),
            values
        );
        // And the canonical render round-trips through the parse.
        assert_eq!(
            parse_body_properties(&rendered, &list, "f.md").unwrap(),
            values
        );
    }

    #[test]
    fn absent_properties_are_absent_both_directions() {
        let list = body_props(&["summary", "notes"]);
        // A file with only the first property: `notes` stays absent.
        let values = parse_body_properties("\nOnly summary.\n", &list, "f.md").unwrap();
        assert_eq!(values, vec![("summary".into(), "Only summary.".into())]);
        // A blank section leaves its property absent (the blank-text
        // demotion rule, per section).
        let text = "\nSummary.\n\n## notes\n\n  \n";
        assert_eq!(
            parse_body_properties(text, &list, "f.md").unwrap(),
            vec![("summary".into(), "Summary.".into())]
        );
        // A blank preamble with a present section: the first property is
        // absent, the section is present.
        let text = "\n\n## notes\n\nKept.\n";
        assert_eq!(
            parse_body_properties(text, &list, "f.md").unwrap(),
            vec![("notes".into(), "Kept.".into())]
        );
    }

    #[test]
    fn a_duplicate_section_fails_closed() {
        let list = body_props(&["summary", "notes"]);
        let text = "\nS.\n\n## notes\n\nOne.\n\n## notes\n\nTwo.\n";
        let err = parse_body_properties(text, &list, "f.md").unwrap_err();
        assert!(
            err.to_string()
                .contains("body section 'notes' appears twice"),
            "{err}"
        );
    }

    #[test]
    fn unmatched_headings_and_the_first_property_heading_are_content() {
        let list = body_props(&["summary", "notes"]);
        // A heading naming the FIRST property is never a boundary; one
        // naming nothing in the list is markdown text wherever it sits.
        let text = "\n## summary\n\nProse intro.\n\n## aside\n\nStill summary.\n";
        assert_eq!(
            parse_body_properties(text, &list, "f.md").unwrap(),
            vec![(
                "summary".into(),
                "## summary\n\nProse intro.\n\n## aside\n\nStill summary.".into()
            )]
        );
        // Inside a section, an unmatched heading stays in that section.
        let text = "\nIntro.\n\n## notes\n\n## scratch\n\nNote text.\n";
        assert_eq!(
            parse_body_properties(text, &list, "f.md").unwrap(),
            vec![
                ("summary".into(), "Intro.".into()),
                ("notes".into(), "## scratch\n\nNote text.".into()),
            ]
        );
    }

    #[test]
    fn a_value_containing_a_marker_heading_fails_closed_at_render() {
        let list = body_props(&["summary", "notes"]);
        // Another property's marker would silently steal the tail…
        let stolen = [
            (
                "summary",
                "See below.\n\n## notes\n\nReal notes live elsewhere.",
            ),
            ("notes", "N."),
        ];
        let err = render_body_properties(&list, &stolen).unwrap_err();
        assert!(
            err.to_string().contains("the next open would re-split"),
            "{err}"
        );
        // …and the property's own marker would arrive twice at parse.
        let duplicated = [("summary", "S."), ("notes", "N.\n\n## notes\n\nAgain.")];
        let err = render_body_properties(&list, &duplicated).unwrap_err();
        assert!(
            err.to_string().contains("the next open would re-split"),
            "{err}"
        );
    }

    #[test]
    fn the_marker_word_is_case_folded_and_carries_trailing_text() {
        // The marker is a word boundary, compared through the case fold:
        // `## Notes — title text` opens the notes section with the
        // trailing text as a kept heading line, `## NOTES` matches, and
        // `## notebook` never matches `notes`.
        let list = body_props(&["summary", "notes"]);
        let text = "\nS.\n\n## Notes — the good stuff\n\nN.\n";
        let values = parse_body_properties(text, &list, "f.md").unwrap();
        assert_eq!(
            values,
            vec![
                ("summary".into(), "S.".into()),
                ("notes".into(), "## — the good stuff\n\nN.".into()),
            ]
        );
        // The canonical render puts the kept heading on its own line —
        // the same value, normalized layout — and re-parses to itself.
        let rendered = render_body_properties(
            &list,
            &[("summary", "S."), ("notes", "## — the good stuff\n\nN.")],
        )
        .unwrap();
        assert_eq!(rendered, "\nS.\n\n## notes\n\n## — the good stuff\n\nN.\n");
        assert_eq!(
            parse_body_properties(&rendered, &list, "f.md").unwrap(),
            values
        );
        // Casing alone, no trailing text — through the Unicode-aware
        // fold, non-ASCII names included.
        assert_eq!(
            parse_body_properties("\nS.\n\n## NOTES\n\nN.\n", &list, "f.md").unwrap(),
            vec![
                ("summary".into(), "S.".into()),
                ("notes".into(), "N.".into()),
            ]
        );
        let unicode = body_props(&["summary", "übersicht"]);
        assert_eq!(
            parse_body_properties("\n\n## Übersicht\n\nText.\n", &unicode, "f.md").unwrap(),
            vec![("übersicht".into(), "Text.".into())]
        );
        // A longer word sharing the prefix is plain content.
        assert_eq!(
            parse_body_properties("\nS.\n\n## notebook\n\nN.\n", &list, "f.md").unwrap(),
            vec![("summary".into(), "S.\n\n## notebook\n\nN.".into())]
        );
    }

    #[test]
    fn trailing_whitespace_on_a_marker_line_is_tolerated() {
        // The reader tolerates hand-authored trailing whitespace on a
        // boundary line (trim_end), exactly like outer body whitespace.
        let list = body_props(&["summary", "notes"]);
        let text = "\nS.\n\n## notes   \n\nN.\n";
        assert_eq!(
            parse_body_properties(text, &list, "f.md").unwrap(),
            vec![
                ("summary".into(), "S.".into()),
                ("notes".into(), "N.".into())
            ]
        );
    }

    #[test]
    fn an_empty_list_reads_no_body() {
        assert!(parse_body_properties("\nAnything.\n", &[], "f.md")
            .unwrap()
            .is_empty());
    }
}
