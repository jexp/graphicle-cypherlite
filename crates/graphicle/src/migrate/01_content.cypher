-- Domain: content store (projects, content objects). Plan §2.1/§2.2.
-- $EE/$EN/$ER/$PH/$E/$NL/$CR decode the exporter's text escaping
-- (see scripts/migrate/export_store.py); migrate-store provides them.

-- @csv projects.csv
-- @verify MATCH (n:Project) RETURN count(n) AS n
LOAD CSV WITH HEADERS FROM $csv AS row
MATCH (w:Workspace {id: row.workspace_id})
MERGE (p:Project {id: row.id, workspace_id: row.workspace_id, project_key: row.project_key})
SET p.name = row.name,
    p.snapshot = row.snapshot,
    p.generation = toInteger(row.generation),
    p.metadata = row.metadata,
    p.created_at = row.created_at,
    p.updated_at = row.updated_at
MERGE (p)-[:IN_WORKSPACE]->(w);

-- @csv objects.csv
-- @verify MATCH (n:Object) RETURN count(n) AS n
LOAD CSV WITH HEADERS FROM $csv AS row
MATCH (p:Project {id: row.repository_id})
MERGE (o:Object {repository_id: row.repository_id, sha256: row.sha256})
SET o.content = replace(replace(replace(replace(row.content, $EE, $PH), $EN, $NL), $ER, $CR), $PH, $E),
    o.bytes = toInteger(row.bytes)
MERGE (p)-[:HAS_OBJECT]->(o);
