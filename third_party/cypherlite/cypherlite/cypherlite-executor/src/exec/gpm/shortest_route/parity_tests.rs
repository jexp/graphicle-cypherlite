//! Route-versus-GPM parity for `MATCH`-clause shortest paths (ADR-0186).
//! Three fixed cyclic graphs and a seeded corpus of small random graphs,
//! with self-loops and parallel relationships, run the routed selector ×
//! direction × bound × endpoint × type matrix twice: on the BFS route and
//! forced onto `match_gpm_path`. Single-path selectors must agree on
//! (start, end, length) per row and return valid trails; all-path
//! selectors must agree on (start, end, trail).

use std::collections::HashSet;

use cypherlite_core::{NodeId, Path, Value};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::exec::test_support::{build, Built};

use super::fixtures::{run, Counts};
use super::with_forced_gpm;

/// Corpus seed; a failure names it with the graph index.
const SEED: u64 = 0x0186_0192;
/// Graphs in the corpus.
const GRAPHS: usize = 160;
/// Graphs with more relationships skip unbounded `[*]`: GPM enumerates
/// every trail, which grows exponentially with the relationship count.
const UNBOUNDED_MAX_RELS: usize = 8;
/// Corpus graph `i` runs the matrix combinations whose index is `i` modulo
/// `STRIDE`, so each combination runs on `GRAPHS / STRIDE` graphs. Fixed
/// graphs run every combination.
const STRIDE: usize = 12;

/// A routed selector: its text before and after the pattern, and whether it
/// returns every shortest trail rather than one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Selector {
    open: &'static str,
    close: &'static str,
    all: bool,
}

#[rustfmt::skip]
const SELECTORS: [Selector; 6] = [
    Selector { open: "shortestPath(", close: ")", all: false },
    Selector { open: "allShortestPaths(", close: ")", all: true },
    Selector { open: "ANY SHORTEST ", close: "", all: false },
    Selector { open: "SHORTEST 1 ", close: "", all: false },
    Selector { open: "SHORTEST 1 GROUP ", close: "", all: true },
    Selector { open: "ALL SHORTEST ", close: "", all: true },
];

/// A length suffix and its minimum and maximum hops (`None`: unbounded).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Bound {
    suffix: &'static str,
    min: u32,
    max: Option<u32>,
}

#[rustfmt::skip]
const BOUNDS: [Bound; 4] = [
    Bound { suffix: "*", min: 1, max: None },
    Bound { suffix: "*..2", min: 1, max: Some(2) },
    Bound { suffix: "*1..3", min: 1, max: Some(3) },
    Bound { suffix: "*0..2", min: 0, max: Some(2) },
];

/// A type expression and the types it admits (empty: every type).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Types {
    expr: &'static str,
    admits: &'static [&'static str],
}

#[rustfmt::skip]
const TYPES: [Types; 3] = [
    Types { expr: "", admits: &[] },
    Types { expr: ":A", admits: &["A"] },
    Types { expr: ":A|B", admits: &["A", "B"] },
];

impl Types {
    fn admit(self, t: &str) -> bool {
        self.admits.is_empty() || self.admits.contains(&t)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Dir {
    Out,
    In,
    Both,
}

const DIRS: [Dir; 3] = [Dir::Out, Dir::In, Dir::Both];

impl Dir {
    fn arrows(self) -> (&'static str, &'static str) {
        match self {
            Dir::Out => ("-", "->"),
            Dir::In => ("<-", "-"),
            Dir::Both => ("-", "-"),
        }
    }

    fn rev(self) -> Dir {
        match self {
            Dir::Out => Dir::In,
            Dir::In => Dir::Out,
            Dir::Both => Dir::Both,
        }
    }

    /// The node reached from `at` over relationship `from → to`, if this
    /// direction may follow it from there.
    fn step(self, at: usize, (from, to): (usize, usize)) -> Option<usize> {
        match self {
            Dir::Out => (from == at).then_some(to),
            Dir::In => (to == at).then_some(from),
            Dir::Both if from == at => Some(to),
            Dir::Both => (to == at).then_some(from),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Ends {
    /// `(a:X)…(b:Y)`, neither bound.
    Unbound,
    /// `MATCH (a:X), (b:Y) MATCH … (a)…(b)`.
    BothBound,
    /// `MATCH (a:X) MATCH … (a)…(b:Y)`.
    StartBound,
    /// `MATCH (b:Y) MATCH … (a:X)…(b)`.
    EndBound,
    /// `(a:X)…(a)`.
    Same,
    /// `(a)…(b)`, unbound and unlabeled.
    Plain,
    /// `MATCH (a:X) MATCH … (a)…(a)`.
    SameBound,
}

const ENDS: [Ends; 7] = [
    Ends::Unbound,
    Ends::BothBound,
    Ends::StartBound,
    Ends::EndBound,
    Ends::Same,
    Ends::Plain,
    Ends::SameBound,
];

impl Ends {
    /// Prior `MATCH`, start node pattern, end node pattern, end column.
    fn parts(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            Ends::Unbound => ("", "(a:X)", "(b:Y)", "b"),
            Ends::BothBound => ("MATCH (a:X), (b:Y) ", "(a)", "(b)", "b"),
            Ends::StartBound => ("MATCH (a:X) ", "(a)", "(b:Y)", "b"),
            Ends::EndBound => ("MATCH (b:Y) ", "(a:X)", "(b)", "b"),
            Ends::Same => ("", "(a:X)", "(a)", "a AS b"),
            Ends::Plain => ("", "(a)", "(b)", "b"),
            Ends::SameBound => ("MATCH (a:X) ", "(a)", "(a)", "a AS b"),
        }
    }

    fn same(self) -> bool {
        matches!(self, Ends::Same | Ends::SameBound)
    }

    fn start_label(self) -> Option<&'static str> {
        (self != Ends::Plain).then_some("X")
    }

    fn end_label(self) -> Option<&'static str> {
        matches!(
            self,
            Ends::Unbound | Ends::BothBound | Ends::StartBound | Ends::EndBound
        )
        .then_some("Y")
    }
}

/// One cell of the query matrix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Combo {
    sel: Selector,
    dir: Dir,
    bound: Bound,
    ends: Ends,
    types: Types,
}

fn matrix() -> Vec<Combo> {
    let mut out = Vec::new();
    for sel in SELECTORS {
        for dir in DIRS {
            for bound in BOUNDS {
                for ends in ENDS {
                    for types in TYPES {
                        out.push(Combo {
                            sel,
                            dir,
                            bound,
                            ends,
                            types,
                        });
                    }
                }
            }
        }
    }
    out
}

impl Combo {
    /// The query; `bare` drops the `:X` and `:Y` of the shortest-path
    /// pattern's endpoints.
    fn query(self, bare: bool) -> String {
        let Selector { open, close, .. } = self.sel;
        let (l, r) = self.dir.arrows();
        let (prior, start, end, end_col) = self.ends.parts();
        let bare_or = |p: &str, label| if bare { p.replace(label, "") } else { p.into() };
        let (start, end) = (bare_or(start, ":X"), bare_or(end, ":Y"));
        let (types, bound) = (self.types.expr, self.bound.suffix);
        format!(
            "{prior}MATCH p = {open}{start}{l}[r{types}{bound}]{r}{end}{close} \
             RETURN p, r, a, {end_col}"
        )
    }

    /// Input rows of the shortest-path `MATCH`: one per prior-`MATCH` row,
    /// or the single empty row when nothing is bound.
    fn input_rows(self, spec: &Spec) -> usize {
        let (x, y) = (spec.with("X").len(), spec.with("Y").len());
        match self.ends {
            Ends::Unbound | Ends::Same | Ends::Plain => 1,
            Ends::BothBound => x * y,
            Ends::StartBound | Ends::SameBound => x,
            Ends::EndBound => y,
        }
    }

    /// Whether `n` has a relationship this pattern may follow along `dir`.
    fn adj(self, spec: &Spec, n: usize, dir: Dir) -> bool {
        spec.rels
            .iter()
            .any(|&(a, b, t)| self.types.admit(t) && dir.step(n, (a, b)).is_some())
    }

    /// Whether some search ADR-0186 prescribes for this query starts where
    /// a relationship can be followed, so the route must expand one.
    /// Searches whose answer is fixed without looking (a pair whose end
    /// cannot be entered, a zero-length partition) do not count.
    fn expansion_required(self, spec: &Spec) -> bool {
        let (d, back) = (self.dir, self.dir.rev());
        let (xs, ys) = (spec.with("X"), spec.with("Y"));
        let closed =
            |s: usize| self.bound.min == 1 && self.adj(spec, s, d) && self.adj(spec, s, back);
        match self.ends {
            Ends::Unbound | Ends::StartBound => {
                !ys.is_empty() && xs.iter().any(|&s| self.adj(spec, s, d))
            }
            Ends::Plain => (0..spec.labels.len()).any(|s| self.adj(spec, s, d)),
            Ends::EndBound => !xs.is_empty() && ys.iter().any(|&e| self.adj(spec, e, back)),
            Ends::BothBound => xs.iter().any(|&s| {
                ys.iter().any(|&e| {
                    if s == e {
                        closed(s)
                    } else {
                        self.adj(spec, s, d) && self.adj(spec, e, back)
                    }
                })
            }),
            Ends::Same | Ends::SameBound => xs.iter().any(|&s| closed(s)),
        }
    }
}

/// One graph: per-node labels and `(from, to, type)` relationships, both by
/// index.
struct Spec {
    labels: Vec<&'static [&'static str]>,
    rels: Vec<(usize, usize, &'static str)>,
}

impl Spec {
    /// `n` nodes, all `:X:Y` so bound endpoints range over every node, for
    /// queries with bare endpoints.
    fn fixed(n: usize, rels: &[(usize, usize, &'static str)]) -> Spec {
        let (labels, rels) = (vec![&["X", "Y"][..]; n], rels.to_vec());
        Spec { labels, rels }
    }

    fn generate(rng: &mut StdRng) -> Spec {
        let n = rng.random_range(4..=7);
        let labels = (0..n)
            .map(|_| -> &'static [&'static str] {
                match rng.random_range(0..4) {
                    0 => &[],
                    1 => &["X"],
                    2 => &["Y"],
                    _ => &["X", "Y"],
                }
            })
            .collect();
        // Mostly small enough for unbounded GPM enumeration; the rest run
        // bounded patterns only.
        let m = if rng.random_bool(0.85) {
            rng.random_range(0..=UNBOUNDED_MAX_RELS)
        } else {
            rng.random_range(UNBOUNDED_MAX_RELS + 1..=12)
        };
        let mut rels: Vec<(usize, usize, &'static str)> = Vec::with_capacity(m);
        for _ in 0..m {
            let t = if rng.random_bool(0.5) { "A" } else { "B" };
            // Reuse an earlier relationship's endpoints now and then, for
            // parallel and antiparallel pairs.
            let (a, b) = if !rels.is_empty() && rng.random_bool(0.2) {
                let (a, b, _) = rels[rng.random_range(0..rels.len())];
                if rng.random_bool(0.5) {
                    (a, b)
                } else {
                    (b, a)
                }
            } else {
                (rng.random_range(0..n), rng.random_range(0..n))
            };
            rels.push((a, b, t));
        }
        Spec { labels, rels }
    }

    fn with(&self, label: &str) -> Vec<usize> {
        (0..self.labels.len())
            .filter(|&i| self.labels[i].contains(&label))
            .collect()
    }

    fn has_self_loop(&self) -> bool {
        self.rels.iter().any(|&(a, b, _)| a == b)
    }

    fn has_parallel_pair(&self) -> bool {
        let mut seen = HashSet::new();
        self.rels
            .iter()
            .any(|&(a, b, _)| a != b && !seen.insert((a, b)))
    }

    /// The graph as Rust literals, ready for a named regression test.
    fn describe(&self) -> String {
        format!("labels: {:?}\n  rels: {:?}", self.labels, self.rels)
    }
}

/// Cyclic graphs with parallel relationships and self-loops.
#[rustfmt::skip]
fn fixed_specs() -> [Spec; 3] {
    [
        Spec::fixed(4, &[(0, 1, "A"), (1, 0, "A"), (0, 1, "B"), (1, 2, "A"),
                         (2, 2, "A"), (2, 3, "B"), (3, 1, "A")]),
        Spec::fixed(5, &[(0, 1, "A"), (1, 2, "A"), (2, 0, "B"), (2, 3, "A"),
                         (3, 4, "A"), (4, 2, "A"), (0, 0, "B")]),
        Spec::fixed(5, &[(0, 1, "A"), (0, 2, "A"), (1, 3, "B"), (2, 3, "A"),
                         (3, 4, "A"), (4, 0, "A"), (1, 2, "B")]),
    ]
}

/// A returned row by spec index.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Trail {
    start: usize,
    end: usize,
    rels: Vec<usize>,
}

/// What route and GPM rows are compared by: the whole trail for all-path
/// selectors; for single-path ones its length, since which trail among
/// ties they return is unspecified.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Trail(Trail),
    Length { start: usize, end: usize, len: u32 },
}

/// Validate one `RETURN p, r, a, b` row against the graph and the pattern
/// and return its trail.
fn check_row(spec: &Spec, built: &Built, c: Combo, row: &[Value]) -> Result<Trail, String> {
    let [Value::Path(p), Value::List(r), Value::Node(a), Value::Node(b)] = row else {
        return Err(format!("unexpected row shape {row:?}"));
    };
    let Path { nodes, rels } = &**p;
    if nodes.len() != rels.len() + 1 {
        return Err(format!(
            "{} nodes for {} relationships",
            nodes.len(),
            rels.len()
        ));
    }
    let node = |id: NodeId| {
        built
            .nodes
            .iter()
            .position(|&n| n == id)
            .ok_or_else(|| format!("unknown node id {id}"))
    };
    let (start, end) = (node(nodes[0].id)?, node(nodes[nodes.len() - 1].id)?);
    let (a, b) = (node(a.id)?, node(b.id)?);
    if (start, end) != (a, b) {
        return Err(format!("path runs {start}..{end}, a/b are {a}/{b}"));
    }
    let len = rels.len() as u32;
    if len < c.bound.min || c.bound.max.is_some_and(|m| len > m) {
        return Err(format!("length {len} outside the bounds"));
    }
    let mut trail = Vec::new();
    for (i, rel) in rels.iter().enumerate() {
        let k = built
            .rels
            .iter()
            .position(|&r| r == rel.id)
            .ok_or_else(|| format!("unknown relationship id {}", rel.id))?;
        let (from, to, t) = spec.rels[k];
        if trail.contains(&k) {
            return Err(format!("relationship #{k} repeats"));
        }
        if !c.types.admit(t) {
            return Err(format!("relationship #{k} has type {t}"));
        }
        let (at, next) = (node(nodes[i].id)?, node(nodes[i + 1].id)?);
        if c.dir.step(at, (from, to)) != Some(next) {
            return Err(format!(
                "relationship #{k} ({from})-[:{t}]->({to}) does not lead {at} to {next} {:?}",
                c.dir
            ));
        }
        trail.push(k);
    }
    for (n, label) in [(start, c.ends.start_label()), (end, c.ends.end_label())] {
        if let Some(label) = label.filter(|l| !spec.labels[n].contains(l)) {
            return Err(format!("node {n} lacks :{label}"));
        }
    }
    if c.ends.same() && start != end {
        return Err(format!("(a)…(a) path runs {start}..{end}"));
    }
    let r_rels: Vec<_> = r
        .iter()
        .map(|v| match v {
            Value::Relationship(x) => Some(x),
            _ => None,
        })
        .collect();
    if r_rels.len() != rels.len()
        || r_rels
            .iter()
            .zip(rels)
            .any(|(x, y)| x.map(|arc| arc.as_ref()) != Some(y))
    {
        return Err(format!("r = {r:?} is not relationships(p)"));
    }
    Ok(Trail {
        start,
        end,
        rels: trail,
    })
}

/// The sorted comparison keys of `trails`.
fn keys(c: Combo, trails: &[Trail]) -> Vec<Key> {
    let mut out: Vec<Key> = trails
        .iter()
        .map(|t| {
            if c.sel.all {
                Key::Trail(t.clone())
            } else {
                let (start, end, len) = (t.start, t.end, t.rels.len() as u32);
                Key::Length { start, end, len }
            }
        })
        .collect();
    out.sort();
    out
}

#[test]
fn route_matches_gpm_on_fixed_and_random_graphs() {
    let mut rng = StdRng::seed_from_u64(SEED);
    let specs: Vec<Spec> = (0..GRAPHS).map(|_| Spec::generate(&mut rng)).collect();
    assert!(specs.len() >= 150);
    assert!(specs
        .iter()
        .all(|s| (4..=7).contains(&s.labels.len()) && s.rels.len() <= 12));
    assert!(specs.iter().any(Spec::has_self_loop), "no self-loop");
    assert!(
        specs.iter().any(Spec::has_parallel_pair),
        "no parallel pair"
    );
    // Fixed graphs run every combination with bare endpoints, which corpus
    // queries never have; corpus graphs run their stride and feed the
    // coverage sets.
    let fixed = fixed_specs();
    let fixed_graphs = fixed.iter().enumerate().map(|(i, s)| (s, i, false));
    let graphs = fixed_graphs.chain(specs.iter().enumerate().map(|(i, s)| (s, i, true)));

    let matrix = matrix();
    let mut ran = HashSet::new();
    let mut routed = HashSet::new();
    let mut expanded = HashSet::new();
    let mut failures = Vec::new();
    let mut cases_run = 0usize;
    for (spec, i, corpus) in graphs {
        let name = if corpus {
            format!("seed {SEED:#x}, graph {i}")
        } else {
            format!("fixed graph {i}")
        };
        let built = build(&spec.labels, &spec.rels);
        let g = &built.graph;
        for (ci, &c) in matrix.iter().enumerate() {
            if (corpus && ci % STRIDE != i % STRIDE)
                || (c.bound.max.is_none() && spec.rels.len() > UNBOUNDED_MAX_RELS)
            {
                continue;
            }
            let q = c.query(!corpus);
            let fail = |detail: String| {
                format!("{name}:\n  {}\n  query: {q}\n  {detail}", spec.describe())
            };
            let (route_rows, counts) = run(g, &q);
            let (gpm_rows, gpm_counts) = with_forced_gpm(|| run(g, &q));
            cases_run += 1;
            assert_eq!(
                gpm_counts,
                Counts::default(),
                "forced GPM still took the route: {q}"
            );
            let checked = |rows: &[Vec<Value>], side: &str| {
                rows.iter()
                    .map(|row| check_row(spec, &built, c, row))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| fail(format!("{side} row invalid: {e}")))
            };
            let (route, gpm) = match (checked(&route_rows, "route"), checked(&gpm_rows, "GPM")) {
                (Ok(route), Ok(gpm)) => (route, gpm),
                (Err(e), _) | (_, Err(e)) => {
                    failures.push(e);
                    continue;
                }
            };
            let (route_keys, gpm_keys) = (keys(c, &route), keys(c, &gpm));
            if route_keys != gpm_keys {
                failures.push(fail(format!("route {route_keys:?}\n  GPM   {gpm_keys:?}")));
            }
            let want_runs = c.input_rows(spec) as u64;
            if counts.route_runs != want_runs {
                failures.push(fail(format!(
                    "route ran {} times for {want_runs} input rows",
                    counts.route_runs
                )));
            }
            let must_expand =
                c.expansion_required(spec) || route.iter().any(|t| !t.rels.is_empty());
            if must_expand && counts.expands == 0 {
                failures.push(fail("the route expanded nothing".into()));
            }
            if corpus {
                ran.insert(c);
                if counts.route_runs > 0 {
                    routed.insert(c);
                }
                if counts.expands > 0 {
                    expanded.insert(c);
                }
            }
        }
    }
    eprintln!(
        "shortest_parity: cases_run = {cases_run} on {} fixed and {GRAPHS} corpus graphs \
         (seed {SEED:#x})",
        fixed.len()
    );
    assert!(
        failures.is_empty(),
        "{} divergences, first ones:\n{}",
        failures.len(),
        failures[..failures.len().min(8)].join("\n\n")
    );
    for c in &matrix {
        assert!(ran.contains(c), "{c:?} never ran");
        assert!(routed.contains(c), "{c:?} never took the route");
        // `(a)-[*0..2]-(a)` only ever yields the zero-length path.
        if !(c.ends.same() && c.bound.min == 0) {
            assert!(expanded.contains(c), "{c:?} never expanded on the route");
        }
    }
}
