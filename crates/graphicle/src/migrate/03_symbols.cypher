-- Domain: code-graph symbols (live gf_nodes rows). Plan §2.4: one
-- :Symbol per (repository_id, stable_id) plus its live :SymbolVersion.
-- Only live rows are exported; history is rebuilt by re-projection.

-- @csv symbols.csv
-- @verify MATCH (n:Symbol) RETURN count(n) AS n
LOAD CSV WITH HEADERS FROM $csv AS row
MERGE (s:Symbol {repository_id: row.repository_id, stable_id: row.stable_id})
SET s.node_id = row.node_id,
    s.kind = row.kind,
    s.qualified_name = row.qualified_name,
    s.relpath = row.relpath
MERGE (s)-[:HAS_VERSION]->(v:SymbolVersion:Live {repository_id: row.repository_id, stable_id: row.stable_id})
SET v.node_id = row.node_id,
    v.data = replace(replace(replace(replace(row.data, $EE, $PH), $EN, $NL), $ER, $CR), $PH, $E),
    v.owner = row.owner,
    v.ord = toInteger(row.ord),
    v.valid_from_episode = row.valid_from_episode,
    v.created_at = row.created_at;
