#!/usr/bin/env python3
"""Export a legacy Graphicle SQLite store (`reviews.sqlite3`) to CSV files.

Usage: python3 export_store.py <reviews.sqlite3> <out-dir>

One CSV per domain (see DOMAINS below), written deterministically (fixed
column lists and ORDER BY) into <out-dir>, which is created when missing.
A JSON manifest mapping each CSV name to its row count is printed on
stdout; diagnostics go to stderr.

The source file is opened read-only (`mode=ro` URI) and never written.

Encoding contract with the CypherLite import scripts
(crates/graphicle/src/migrate/*.cypher): the engine's LOAD CSV reader
splits input on newlines, so text fields are escaped before writing:
U+0001 is the escape introducer, `\\x01n` is a newline, `\\x01r` a
carriage return, and a literal U+0001 doubles. The import scripts decode
with ordered replace() calls. Source text containing U+0002 (the decode
placeholder) is rejected here rather than migrated corrupt.

Standard library only; no third-party packages.
"""

import csv
import json
import sqlite3
import sys
from pathlib import Path

ESC = "\u0001"  # escape introducer in encoded text fields
PLACEHOLDER = "\u0002"  # reserved by the import-side decode chain


def encode(value):
    """Escape a text value for one-line CSV; None becomes the empty string."""
    if value is None:
        return ""
    text = str(value)
    if PLACEHOLDER in text:
        raise SystemExit(
            "export_store: a text field contains U+0002, which the import "
            "decode chain reserves; refusing to migrate it corrupt"
        )
    return text.replace(ESC, ESC + ESC).replace("\n", ESC + "n").replace("\r", ESC + "r")


# (csv name, source table, SELECT ... ORDER BY ...) — one per migrated
# domain. The column order is the CSV header; the import scripts name
# these columns. Every SELECT ends in a deterministic ORDER BY.
DOMAINS = [
    (
        "workspaces",
        "workspaces",
        "SELECT id, capability_hash, name, created_at, version "
        "FROM workspaces ORDER BY id",
    ),
    (
        "credentials",
        "credentials",
        "SELECT id, workspace_id, capability_hash, role, label, created_at, "
        "expires_at, revoked_at FROM credentials ORDER BY id",
    ),
    (
        "projects",
        "repo_projects",
        "SELECT id, workspace_id, project_key, name, snapshot, generation, "
        "metadata, created_at, updated_at FROM repo_projects ORDER BY id",
    ),
    (
        "objects",
        "repo_objects",
        "SELECT repository_id, sha256, content, bytes "
        "FROM repo_objects ORDER BY repository_id, sha256",
    ),
    (
        "memories",
        "repo_memories",
        "SELECT id, repository_id, kind, revision, state, title, body, data, "
        "created_at, updated_at FROM repo_memories ORDER BY id",
    ),
    # Live code-graph symbols only (history is rebuilt by re-projection):
    # valid_to_episode IS NULL, and rows without a stable_id in their
    # attributes are keyed by node_id there, as the projection does.
    (
        "symbols",
        "gf_nodes",
        "SELECT id AS ord, repository_id, node_id, "
        "coalesce(json_extract(attributes,'$.stable_id'), node_id) AS stable_id, "
        "json_extract(attributes,'$.kind') AS kind, "
        "json_extract(attributes,'$.qualified_name') AS qualified_name, "
        "json_extract(attributes,'$.relpath') AS relpath, "
        "coalesce(json_extract(attributes,'$.owner'), '') AS owner, "
        "attributes AS data, valid_from_episode, created_at "
        "FROM gf_nodes WHERE valid_to_episode IS NULL ORDER BY id",
    ),
]


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    source = Path(sys.argv[1])
    out_dir = Path(sys.argv[2])
    if not source.is_file():
        sys.exit(f"export_store: no such store file: {source}")
    out_dir.mkdir(parents=True, exist_ok=True)
    # Read-only URI open: the source store is the backup and never changes.
    db = sqlite3.connect(f"file:{source}?mode=ro", uri=True)
    present = {
        row[0]
        for row in db.execute("SELECT name FROM sqlite_master WHERE type='table'")
    }
    manifest = {}
    for name, table, query in DOMAINS:
        path = out_dir / f"{name}.csv"
        if table not in present:
            print(f"export_store: table {table} absent; writing an empty {name}.csv",
                  file=sys.stderr)
            # Header-only file: the import statement still runs, loading 0 rows.
            header = query.removeprefix("SELECT ").split(" FROM ")[0]
            columns = [c.strip().split(" AS ")[-1] for c in _split_columns(header)]
            rows = []
        else:
            cursor = db.execute(query)
            columns = [d[0] for d in cursor.description]
            rows = cursor.fetchall()
        with path.open("w", newline="", encoding="utf-8") as handle:
            writer = csv.writer(handle, lineterminator="\n")
            writer.writerow(columns)
            for row in rows:
                writer.writerow([encode(value) for value in row])
        manifest[name] = len(rows)
    db.close()
    json.dump(manifest, sys.stdout, indent=2)
    sys.stdout.write("\n")


def _split_columns(header: str):
    """Split a SELECT column list on top-level commas (no nesting here)."""
    return header.split(",")


if __name__ == "__main__":
    main()
