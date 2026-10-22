-- Domain: reviewed memory (repo_memories). Plan §2.3; memory.rs keys
-- Memory nodes by id with repository_id as a scoping property.

-- @csv memories.csv
-- @verify MATCH (n:Memory) RETURN count(n) AS n
LOAD CSV WITH HEADERS FROM $csv AS row
MERGE (m:Memory {id: row.id})
SET m.repository_id = row.repository_id,
    m.kind = row.kind,
    m.revision = toInteger(row.revision),
    m.state = row.state,
    m.title = replace(replace(replace(replace(row.title, $EE, $PH), $EN, $NL), $ER, $CR), $PH, $E),
    m.body = replace(replace(replace(replace(row.body, $EE, $PH), $EN, $NL), $ER, $CR), $PH, $E),
    m.data = replace(replace(replace(replace(row.data, $EE, $PH), $EN, $NL), $ER, $CR), $PH, $E),
    m.created_at = row.created_at,
    m.updated_at = row.updated_at;
