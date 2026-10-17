-- Domain: timeline assertions (gf_timeline). Plan §2.4 / temporal_graph.rs:
-- (:Assertion {repository_id, branch, episode, valid_us, recorded_us,
-- origin, seq})-[:ON_TIMELINE]->(:Project). The SQL rowid becomes the
-- store-wide `seq` tiebreak (the role it played in SQL); the trailing
-- WITH seeds the temporal counter past the imported ids so later
-- assertions (which draw from that counter) never reuse a migrated seq.

-- @csv timeline.csv
-- @verify MATCH (n:Assertion) RETURN count(n) AS n
LOAD CSV WITH HEADERS FROM $csv AS row
MATCH (p:Project {id: row.repository_id})
MERGE (a:Assertion {seq: toInteger(row.id)})
SET a.repository_id = row.repository_id,
    a.branch = row.branch,
    a.episode = row.episode,
    a.valid_us = toInteger(row.valid_us),
    a.recorded_us = toInteger(row.recorded_us),
    a.origin = row.origin
MERGE (a)-[:ON_TIMELINE]->(p)
WITH max(toInteger(row.id)) AS max_seq
MERGE (c:Counter {name: 'temporal_seq'}) SET c.value = max_seq;
