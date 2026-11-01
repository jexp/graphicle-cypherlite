//! Crate-level test support: the helpers shared across module test trees, so
//! a forest's tests never reach into another forest's harness. The `graph`
//! fixture tests drive real statements through [`exec_autocommit`]; the
//! `exec` tests re-export it from `exec::test_support` (one definition, both
//! paths).

use cypherlite_core::Result;

use crate::exec::test_support::{commit_scratch, exec_write, write_scratch};
use crate::exec::QueryResult;
use crate::graph::Graph;

/// One autocommit statement: `cypher` on a fresh scratch through `execute`,
/// committed when it succeeds and discarded when it fails.
pub(crate) fn exec_autocommit(g: &Graph, cypher: &str) -> Result<QueryResult> {
    let scratch = write_scratch(g);
    let result = exec_write(g, &scratch, cypher)?;
    commit_scratch(g, scratch);
    Ok(result)
}
