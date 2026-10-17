-- Domain: ownership spine (workspaces, credentials). Plan §2.1.
-- Sections start with `-- @csv <file>`; `$csv` is the file's URL,
-- provided by `graphicle migrate-store` (or `cypherlite query --param`).

-- @csv workspaces.csv
-- @verify MATCH (n:Workspace) RETURN count(n) AS n
LOAD CSV WITH HEADERS FROM $csv AS row
MERGE (w:Workspace {id: row.id})
SET w.capability_hash = row.capability_hash,
    w.name = row.name,
    w.created_at = row.created_at,
    w.version = toInteger(row.version);

-- @csv credentials.csv
-- @verify MATCH (n:Credential) RETURN count(n) AS n
LOAD CSV WITH HEADERS FROM $csv AS row
MATCH (w:Workspace {id: row.workspace_id})
MERGE (c:Credential {id: row.id})
SET c.capability_hash = row.capability_hash,
    c.role = row.role,
    c.label = row.label,
    c.created_at = row.created_at,
    c.expires_at = CASE row.expires_at WHEN '' THEN null ELSE row.expires_at END,
    c.revoked_at = CASE row.revoked_at WHEN '' THEN null ELSE row.revoked_at END
MERGE (c)-[:IN_WORKSPACE]->(w);
