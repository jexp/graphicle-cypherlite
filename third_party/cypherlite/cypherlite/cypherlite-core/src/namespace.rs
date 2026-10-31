//! The fixed namespaces of a CypherLite store (ADR-0116 §1).
//!
//! Every store has exactly three namespaces: [`Namespace::Neo4j`] (the user
//! data graph), [`Namespace::System`] (CE system database; holds no graph
//! data) and [`Namespace::SystemOntologies`] (the read-only ontology graph).
//! None can be created, dropped or renamed, and names match
//! case-insensitively.

use std::fmt;
use std::str::FromStr;

/// One of the three fixed namespaces of a store.
///
/// Element ids name their namespace through the middle segment of
/// `<kind>:cypherlite[.<namespace>]:<n|r><id>` (ADR-0116 §7): `neo4j` keeps
/// the bare `cypherlite` segment, `system_ontologies` uses
/// `cypherlite.system_ontologies`, and `system` has no elements and no
/// segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Namespace {
    /// The user data graph. The only namespace Cypher can write.
    Neo4j,
    /// The CE system database: SHOW commands, supported administration
    /// commands and procedure calls only. Holds no graph data.
    System,
    /// The read-only ontology graph, written only through the `db.ontology.*`
    /// procedures.
    SystemOntologies,
}

impl Namespace {
    /// Every namespace, in canonical order: `neo4j`, `system`,
    /// `system_ontologies`.
    pub const ALL: [Namespace; 3] = [
        Namespace::Neo4j,
        Namespace::System,
        Namespace::SystemOntologies,
    ];

    /// The canonical (lowercase) namespace name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Namespace::Neo4j => "neo4j",
            Namespace::System => "system",
            Namespace::SystemOntologies => "system_ontologies",
        }
    }

    /// Parses a namespace name, case-insensitively (ADR-0116 §1). Returns
    /// `None` for any other name.
    pub fn parse(name: &str) -> Option<Namespace> {
        Namespace::ALL
            .iter()
            .find(|ns| name.eq_ignore_ascii_case(ns.as_str()))
            .copied()
    }

    /// Whether Cypher can write this namespace. Only `neo4j` is writable
    /// (ADR-0116 §5: read many, write one).
    pub fn is_writable(&self) -> bool {
        matches!(self, Namespace::Neo4j)
    }

    /// Whether this namespace is read-only to Cypher. Only
    /// `system_ontologies` is (ADR-0116 §1).
    pub fn is_read_only(&self) -> bool {
        matches!(self, Namespace::SystemOntologies)
    }

    /// The element-id middle segment for this namespace (ADR-0116 §7), or
    /// `None` for `system`, which has no elements.
    pub fn element_id_segment(&self) -> Option<&'static str> {
        match self {
            Namespace::Neo4j => Some("cypherlite"),
            Namespace::System => None,
            Namespace::SystemOntologies => Some("cypherlite.system_ontologies"),
        }
    }

    /// The inverse of [`Namespace::element_id_segment`]: maps an element-id
    /// middle segment back to its namespace. Returns `None` for any other
    /// segment (including `system`, which has none).
    pub fn from_element_id_segment(segment: &str) -> Option<Namespace> {
        Namespace::ALL
            .iter()
            .find(|ns| ns.element_id_segment() == Some(segment))
            .copied()
    }
}

/// The composite-database `graph.*` selector function names, lowercased
/// (ADR-0116 §3). One shared list: the planner's `use_target` resolves
/// these calls as `USE` selectors (with element-id resolution for
/// `graph.byElementId`) and rejects them with CE's 42N72 text in
/// expression position, while the executor's `SHOW FUNCTIONS` catalog
/// lists them as catalog-only entries (discoverable but not executable).
/// Two separate lists would drift.
pub const GRAPH_SELECTOR_FUNCTIONS: [&str; 4] = [
    "graph.names",
    "graph.byname",
    "graph.byelementid",
    "graph.propertiesbyname",
];

/// True for the composite-database `graph.*` selector names
/// (case-insensitive). The one predicate home — callers never spell the
/// membership test themselves. Allocation-free: compares with
/// `eq_ignore_ascii_case` like `Namespace::parse` does.
pub fn is_graph_selector_function_name(name: &str) -> bool {
    GRAPH_SELECTOR_FUNCTIONS
        .iter()
        .any(|s| name.eq_ignore_ascii_case(s))
}

/// The parsed shape of an element id (ADR-0116 §7):
/// `<kind>:<segment>:<n|r><id>`. This is the one parser for that three-part
/// structure, shared by `USE graph.byElementId(...)` resolution and
/// `db.nameFromElementId`; it only splits the shape — callers decide how
/// much of it to validate (`db.nameFromElementId` checks the kind prefix
/// and rejects an empty entity segment, but deliberately does not validate
/// the entity segment's contents).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElementIdRef<'a> {
    /// The first segment: the element kind (`4` node, `5` relationship).
    pub kind: &'a str,
    /// The middle segment: names the namespace
    /// (`cypherlite[.<namespace>]`).
    pub segment: &'a str,
    /// The third segment: the entity reference (`n<id>` / `r<id>`), opaque.
    pub id: &'a str,
}

impl<'a> ElementIdRef<'a> {
    /// Split an element id into its three colon-separated segments, or
    /// `None` when it does not have exactly that shape.
    pub fn parse(id: &'a str) -> Option<ElementIdRef<'a>> {
        let mut parts = id.split(':');
        let parsed = ElementIdRef {
            kind: parts.next()?,
            segment: parts.next()?,
            id: parts.next()?,
        };
        if parts.next().is_some() {
            return None;
        }
        Some(parsed)
    }

    /// The namespace the middle segment names (ADR-0116 §7), or `None`
    /// when it names no namespace.
    pub fn namespace(&self) -> Option<Namespace> {
        Namespace::from_element_id_segment(self.segment)
    }
}

/// The element kind an element id encodes: node (`4`/`n`) or relationship
/// (`5`/`r`). The digit and the entity prefix travel together, so a
/// mismatched pair cannot be expressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ElementKind {
    Node,
    Rel,
}

impl ElementKind {
    /// The first segment of the wire form (`4` node, `5` relationship).
    fn kind_segment(self) -> &'static str {
        match self {
            ElementKind::Node => "4",
            ElementKind::Rel => "5",
        }
    }

    /// The entity prefix of the third segment (`n`/`r`).
    fn entity_prefix(self) -> char {
        match self {
            ElementKind::Node => 'n',
            ElementKind::Rel => 'r',
        }
    }
}

/// The element id string of a node in `namespace` (`4:<segment>:n<id>`),
/// built through the same validated namespace the parser accepts — an
/// ad-hoc segment could emit an id the engine's own parser refuses.
pub fn node_element_id(id: i64, namespace: Namespace) -> Option<String> {
    element_id(id, namespace, ElementKind::Node)
}

/// The element id string of a relationship in `namespace`
/// (`5:<segment>:r<id>`).
pub fn rel_element_id(id: i64, namespace: Namespace) -> Option<String> {
    element_id(id, namespace, ElementKind::Rel)
}

/// The node id encoded in a `cypherlite` element id, or `None` for a
/// relationship id, a foreign namespace, or a malformed value.
pub fn parse_node_element_id(s: &str) -> Option<i64> {
    parse_element_id(s, ElementKind::Node)
}

/// The relationship id encoded in a `cypherlite` element id, or `None`.
pub fn parse_rel_element_id(s: &str) -> Option<i64> {
    parse_element_id(s, ElementKind::Rel)
}

/// One owner of the element-id wire form: the constructor and the decoder
/// share the kind digit, the namespace (validated through the canonical
/// [`Namespace::element_id_segment`] mapping — a bare `starts_with` would
/// accept `cypherliteXYZ`), and the entity prefix.
fn element_id(id: i64, namespace: Namespace, kind: ElementKind) -> Option<String> {
    let segment = namespace.element_id_segment()?;
    Some(format!(
        "{}:{}:{}{}",
        kind.kind_segment(),
        segment,
        kind.entity_prefix(),
        id
    ))
}

fn parse_element_id(s: &str, kind: ElementKind) -> Option<i64> {
    let parsed = ElementIdRef::parse(s)?;
    if parsed.kind != kind.kind_segment() {
        return None;
    }
    parsed.namespace()?;
    parsed.id.strip_prefix(kind.entity_prefix())?.parse().ok()
}

impl fmt::Display for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Namespace {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Namespace::parse(s).ok_or(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_is_case_insensitive() {
        assert_eq!(Namespace::parse("neo4j"), Some(Namespace::Neo4j));
        assert_eq!(Namespace::parse("Neo4j"), Some(Namespace::Neo4j));
        assert_eq!(Namespace::parse("NEO4J"), Some(Namespace::Neo4j));
        assert_eq!(Namespace::parse("SYSTEM"), Some(Namespace::System));
        assert_eq!(
            Namespace::parse("System_Ontologies"),
            Some(Namespace::SystemOntologies)
        );
        assert_eq!("neo4j".parse::<Namespace>(), Ok(Namespace::Neo4j));
        assert_eq!(
            "SYSTEM_ONTOLOGIES".parse::<Namespace>(),
            Ok(Namespace::SystemOntologies)
        );
    }

    #[test]
    fn parse_rejects_unknown_names() {
        assert_eq!(Namespace::parse(""), None);
        assert_eq!(Namespace::parse("nosuch"), None);
        assert_eq!(Namespace::parse("systemx"), None);
        assert_eq!(Namespace::parse("system_ontology"), None);
        assert!("movies".parse::<Namespace>().is_err());
    }

    #[test]
    fn display_and_as_str_are_canonical() {
        assert_eq!(Namespace::Neo4j.as_str(), "neo4j");
        assert_eq!(Namespace::System.as_str(), "system");
        assert_eq!(Namespace::SystemOntologies.as_str(), "system_ontologies");
        assert_eq!(Namespace::Neo4j.to_string(), "neo4j");
        assert_eq!(Namespace::System.to_string(), "system");
        assert_eq!(Namespace::SystemOntologies.to_string(), "system_ontologies");
    }

    #[test]
    fn only_neo4j_is_writable() {
        assert!(Namespace::Neo4j.is_writable());
        assert!(!Namespace::System.is_writable());
        assert!(!Namespace::SystemOntologies.is_writable());
    }

    #[test]
    fn only_system_ontologies_is_read_only() {
        assert!(!Namespace::Neo4j.is_read_only());
        assert!(!Namespace::System.is_read_only());
        assert!(Namespace::SystemOntologies.is_read_only());
    }

    #[test]
    fn element_id_segments_round_trip() {
        assert_eq!(Namespace::Neo4j.element_id_segment(), Some("cypherlite"));
        assert_eq!(Namespace::System.element_id_segment(), None);
        assert_eq!(
            Namespace::SystemOntologies.element_id_segment(),
            Some("cypherlite.system_ontologies")
        );
        for ns in Namespace::ALL {
            if let Some(segment) = ns.element_id_segment() {
                assert_eq!(Namespace::from_element_id_segment(segment), Some(ns));
            }
        }
        // `system` has no segment, and unknown segments map to nothing.
        assert_eq!(Namespace::from_element_id_segment("system"), None);
        assert_eq!(
            Namespace::from_element_id_segment("cypherlite.system"),
            None
        );
        assert_eq!(Namespace::from_element_id_segment("neo4j"), None);
        assert_eq!(Namespace::from_element_id_segment(""), None);
    }

    #[test]
    fn element_id_ref_parses_the_three_part_shape() {
        let parsed = ElementIdRef::parse("4:cypherlite.system_ontologies:r9").unwrap();
        assert_eq!(parsed.kind, "4");
        assert_eq!(parsed.segment, "cypherlite.system_ontologies");
        assert_eq!(parsed.id, "r9");
        assert_eq!(parsed.namespace(), Some(Namespace::SystemOntologies));
        assert_eq!(
            ElementIdRef::parse("5:cypherlite:n1").unwrap().namespace(),
            Some(Namespace::Neo4j)
        );
        // Only the shape is split: kind and entity segment stay opaque.
        let parsed = ElementIdRef::parse("4:cypherlite:999999").unwrap();
        assert_eq!(parsed.id, "999999");
        // Not exactly three segments, or a segment naming no namespace.
        assert_eq!(ElementIdRef::parse("garbage"), None);
        assert_eq!(ElementIdRef::parse("4:cypherlite"), None);
        assert_eq!(ElementIdRef::parse("4:cypherlite:n1:extra"), None);
        assert_eq!(ElementIdRef::parse("4::n1").unwrap().namespace(), None);
        assert_eq!(
            ElementIdRef::parse("4:00000000-0000-0000-0000-000000000000:n1")
                .unwrap()
                .namespace(),
            None
        );
    }

    #[test]
    fn graph_selector_functions_are_the_four_composite_selectors() {
        assert_eq!(
            GRAPH_SELECTOR_FUNCTIONS,
            [
                "graph.names",
                "graph.byname",
                "graph.byelementid",
                "graph.propertiesbyname"
            ]
        );
        // Already lowercase: `is_graph_selector_function_name` matches
        // case-insensitively against them (`eq_ignore_ascii_case`).
        for name in GRAPH_SELECTOR_FUNCTIONS {
            assert_eq!(name, name.to_ascii_lowercase());
        }
        assert!(is_graph_selector_function_name("Graph.ByName"));
        assert!(!is_graph_selector_function_name("graph.x"));
    }

    #[test]
    fn all_lists_every_namespace_in_order() {
        assert_eq!(
            Namespace::ALL,
            [
                Namespace::Neo4j,
                Namespace::System,
                Namespace::SystemOntologies
            ]
        );
    }
}
