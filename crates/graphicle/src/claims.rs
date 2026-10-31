//! Grounded interpretation drafts and explicitly reviewed temporal revisions.
use crate::{Error, Result, digest, id, native, now, string};
use crate::{
    memory,
    store::{Principal, Store, repo, s},
    store_graph::{self, GraphStore},
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// One claim by id (optionally state-scoped), as a plain record.
fn claim_row(
    db: &mut cypherlite::Connection,
    repository: &str,
    claim_id: &str,
    state: Option<&str>,
) -> Result<Option<Value>> {
    let row = if let Some(state) = state {
        store_graph::one(
            db,
            "MATCH (c:Claim {repository_id: $repo, id: $id, state: $state}) RETURN c",
            store_graph::params(&json!({"repo": repository, "id": claim_id, "state": state})),
        )?
    } else {
        store_graph::one(
            db,
            "MATCH (c:Claim {repository_id: $repo, id: $id}) RETURN c",
            store_graph::params(&json!({"repo": repository, "id": claim_id})),
        )?
    };
    match row {
        Some(row) => {
            let mut record = store_graph::node(&row, "c");
            crate::store::decode(&mut record, "data")?;
            Ok(Some(record))
        }
        None => Ok(None),
    }
}

pub fn current(
    db: &Connection,
    graph: &GraphStore,
    repository: &Value,
    p: &Principal,
) -> Result<Vec<Value>> {
    let memories = memory::list(db, graph, repository, p, true)?;
    let allowed: BTreeMap<&str, &Value> = memories
        .iter()
        .filter(|m| m["state"] == "reviewed" && m["stale"] != true)
        .map(|m| (s(m, "id"), m))
        .collect();
    let mut output = Vec::new();
    for row in graph.read(|db| {
        store_graph::rows(
            db,
            "MATCH (c:Claim {repository_id: $repo, state: 'active'}) RETURN c ORDER BY c.id",
            store_graph::params(&json!({"repo": s(repository, "id")})),
        )
    })? {
        let mut row = store_graph::node(&row, "c");
        crate::store::decode(&mut row, "data")?;
        if let Some(memory) = allowed.get(s(&row, "memory_id")) {
            row["memory_revision"] = memory["revision"].clone();
            output.push(row);
        }
    }
    Ok(output)
}
/// The claims a caller proposes, with the context they were read from and
/// the provider that would write them: the claims themselves, the source
/// documents they cite (`sources`, the view the result is stamped with),
/// the memories they make statements about (`targets`), and the provider
/// record saved with them.
pub struct Proposal<'a> {
    pub claims: &'a Value,
    pub sources: &'a [Value],
    pub targets: &'a [Value],
    pub provider: &'a Value,
}

pub fn propose(
    db: &Connection,
    graph: &GraphStore,
    repository: &Value,
    p: &Principal,
    proposal: Proposal<'_>,
) -> Result<Value> {
    let Proposal {
        claims,
        sources,
        targets,
        provider,
    } = proposal;
    let identifier = s(repository, "id");
    let checked = native(
        "interpretation.validate",
        json!({"scope":identifier,"claims":claims,"sources":sources,"target_ids":targets.iter().map(|t|t["id"].clone()).collect::<Vec<_>>()}),
    )?;
    let docs = memory::documents(db, repository, p)?;
    // Stamp the view of the sources the claims were read from, not of every
    // document: a claim then carries no trace of unrelated sources, which
    // forgetting one of them requires.
    let read: Vec<Value> = docs
        .iter()
        .filter(|d| sources.iter().any(|x| x["id"] == d["id"]))
        .cloned()
        .collect();
    let source_state = memory::state(&read);
    let mut output = Vec::new();
    for claim in checked["claims"].as_array().unwrap() {
        let claim_id = s(claim, "id");
        if let Some(prior) = graph.read(|g| claim_row(g, identifier, claim_id, None))? {
            output.push(json!({"claim_id":claim_id,"memory_id":prior["memory_id"],"state":prior["state"],"replayed":true}));
            continue;
        }
        let mut data =
            memory::dependencies(db, identifier, claim["evidence"].as_array().unwrap(), &docs)?;
        for source in sources {
            if let Some(doc) = docs
                .iter()
                .find(|d| d["id"] == source["id"] && d["source_kind"] == "connector_source")
            {
                data["native_sources"][s(doc, "id")] = doc["sha256"].clone();
            }
        }
        data["claim"] = claim.clone();
        data["provider"] = provider.clone();
        data["notes"] = json!({});
        data["authority"] = json!("unreviewed_model_interpretation");
        if let Some(target) = claim["target_id"].as_str() {
            let target = targets
                .iter()
                .find(|t| s(t, "id") == target)
                .ok_or_else(|| Error::bad("Revision target disappeared"))?;
            data["target_revision"] = json!({"claim_id":target["id"],"memory_id":target["memory_id"],"revision":target["memory_revision"]});
            data["notes"][s(target, "memory_id")] = target["memory_revision"].clone();
        }
        let title = format!("{} · {}", s(claim, "subject"), s(claim, "relation"));
        let mut body = format!(
            "{}\n\nAttribution: {}. Kind: {}.",
            s(claim, "statement"),
            s(claim, "speaker"),
            s(claim, "modality")
        );
        for condition in claim["conditions"].as_array().unwrap() {
            body.push_str(&format!("\nCondition: {}", condition.as_str().unwrap()));
        }
        if !claim["valid_at"].is_null() {
            body.push_str(&format!("\nValid from: {}", s(claim, "valid_at")));
        }
        if !claim["invalid_at"].is_null() {
            body.push_str(&format!("\nValid until: {}", s(claim, "invalid_at")));
        }
        let title = graphicle_memory::text::prefix(&title, 120, 500);
        let draft = memory::save(
            db,
            graph,
            identifier,
            None,
            None,
            "observation",
            &title,
            &body,
            "draft",
            &data,
        )?;
        let mut persisted = claim.clone();
        persisted["knowledge_at"] = json!(now());
        persisted["target_revision"] = data["target_revision"].clone();
        graph.transaction(|g| {
            store_graph::rows(
                g,
                "CREATE (c:Claim {id: $id, repository_id: $repo, memory_id: $memory_id, \
                 state: 'draft', data: $data, source_state: $source_state, \
                 created_at: $at, updated_at: $at}) \
                 WITH c MATCH (m:Memory {id: $memory_id}) CREATE (c)-[:ABOUT_MEMORY]->(m) \
                 RETURN c.id AS id",
                store_graph::params(&json!({"id":claim_id,"repo":identifier,
                    "memory_id":s(&draft,"id"),"data":persisted.to_string(),"source_state":source_state,
                    "at":now()})),
            )?;
            Ok(())
        })?;
        output.push(json!({"claim_id":claim_id,"memory_id":draft["id"],"revision":draft["revision"],"state":"draft","action":claim["action"]}));
    }
    Ok(
        json!({"drafts":output,"quotes_validated":true,"semantic_entailment_verified":false,"approval_required":true}),
    )
}
pub fn apply_memory_review(
    db: &Connection,
    graph: &GraphStore,
    repository: &str,
    draft_id: &str,
    result_id: &str,
    decision: &str,
) -> Result<()> {
    let claim = graph.read(|g| {
        Ok(store_graph::one(
            g,
            "MATCH (c:Claim {repository_id: $repo, memory_id: $mid}) RETURN c",
            store_graph::params(&json!({"repo": repository, "mid": draft_id})),
        )?
        .map(|row| store_graph::node(&row, "c")))
    })?;
    let Some(mut claim) = claim else {
        return Ok(());
    };
    crate::store::decode(&mut claim, "data")?;
    let mut data = claim["data"].clone();
    if decision != "approve" {
        graph.transaction(|g| {
            store_graph::rows(
                g,
                "MATCH (c:Claim {id: $id}) SET c.state = 'rejected', c.updated_at = $at \
                 RETURN c.id AS id",
                store_graph::params(&json!({"id": s(&claim, "id"), "at": now()})),
            )?;
            Ok(())
        })?;
        return Ok(());
    }
    let action = s(&data, "action").to_string();
    let stamp = now();
    if let Some(target) = data["target_id"].as_str() {
        let mut old = graph
            .read(|g| claim_row(g, repository, target, Some("active")))?
            .ok_or_else(|| Error::conflict("Claim target is no longer current"))?;
        let memory = memory::memory_row(graph, repository, s(&old, "memory_id"))?
            .ok_or_else(|| Error::conflict("Target memory disappeared"))?;
        if memory["revision"] != data["target_revision"]["revision"] {
            return Err(Error::conflict("Target revision changed before approval"));
        }
        if ["SUPERSEDE", "CORRECT", "RETRACT"].contains(&action.as_str()) {
            let mut old_data = memory["data"].clone();
            old_data["retirement"] = json!({"action":action,"by_claim":claim["id"],"knowledge_at":stamp,"valid_at":data["valid_at"]});
            memory::save(
                db,
                graph,
                repository,
                Some(s(&memory, "id")),
                memory["revision"].as_i64(),
                s(&memory, "kind"),
                s(&memory, "title"),
                s(&memory, "body"),
                "retired",
                &old_data,
            )?;
            old["data"]["retired_at"] = json!(stamp);
            old["data"]["retirement_action"] = json!(action);
            if action == "SUPERSEDE" && !data["valid_at"].is_null() {
                old["data"]["invalid_at"] = data["valid_at"].clone();
            }
            graph.transaction(|g| {
                store_graph::rows(
                    g,
                    "MATCH (c:Claim {id: $id}) \
                     SET c.state = 'retired', c.data = $data, c.updated_at = $at \
                     CREATE (e:ClaimEvent {id: $eid, repository_id: $repo, claim_id: $id, \
                         action: 'RETIRE', data: $data, created_at: $at})-[:FOR]->(c) \
                     RETURN c.id AS id",
                    store_graph::params(&json!({"id":s(&old,"id"),"data":old["data"].to_string(),
                        "at":stamp,"eid":id(),"repo":repository})),
                )?;
                Ok(())
            })?;
        }
        // Target history remains provenance, without a live freshness dependency
        // on a fact that this revision deliberately superseded.
        if let Some(mut memory) = memory::memory_row(graph, repository, result_id)? {
            if let Some(notes) = memory["data"]["notes"].as_object_mut() {
                notes.remove(s(&old, "memory_id"));
            }
            memory["data"]["historical_target"] = data["target_revision"].clone();
            let mut journal = memory.clone();
            journal["data"] = json!(memory["data"].to_string());
            graph.transaction(|g| {
                store_graph::rows(
                    g,
                    "MATCH (m:Memory {id: $id}) SET m.data = $data \
                     WITH m MATCH (r:MemoryRevision {memory_id: $id, revision: m.revision}) \
                     SET r.data = $raw RETURN m.id AS id",
                    store_graph::params(
                        &json!({"id": result_id, "data": memory["data"].to_string(),
                        "raw": journal.to_string()}),
                    ),
                )?;
                Ok(())
            })?;
        }
    }
    data["approved_at"] = json!(stamp);
    let state = if ["KEEP", "CONFIRM", "RETRACT"].contains(&action.as_str()) {
        "event"
    } else {
        "active"
    };
    let mut entities = Vec::new();
    if state == "active" {
        for facet in ["subject", "object"] {
            let name = s(&data, facet);
            let provenance = data["evidence"]
                .as_array()
                .unwrap()
                .first()
                .map(|v| v["id"].clone());
            entities.push(digest(
                &json!({"repository":repository,"name":name,"speaker":data["speaker"],"source":provenance}),
            ));
        }
    }
    let claim_id = s(&claim, "id").to_string();
    graph.transaction(|g| {
        store_graph::rows(
            g,
            "MATCH (c:Claim {id: $id}) \
             SET c.memory_id = $mid, c.state = $state, c.data = $data, c.updated_at = $at \
             CREATE (e:ClaimEvent {id: $eid, repository_id: $repo, claim_id: $id, \
                 action: $action, data: $data, created_at: $at})-[:FOR]->(c) \
             RETURN c.id AS id",
            store_graph::params(&json!({"id":claim_id,"mid":result_id,"state":state,
                "data":data.to_string(),"at":stamp,"eid":id(),"repo":repository,"action":action})),
        )?;
        if state == "active" {
            for (facet, entity_id) in ["subject", "object"].iter().zip(&entities) {
                let provenance = data["evidence"]
                    .as_array()
                    .unwrap()
                    .first()
                    .map(|v| v["id"].clone());
                store_graph::rows(
                    g,
                    "MERGE (e:Entity {id: $id}) ON CREATE SET e.repository_id = $repo, \
                     e.name = $name, e.kind = 'source_scoped', e.data = $data, \
                     e.created_at = $at RETURN e.id AS id",
                    store_graph::params(&json!({"id":entity_id,"repo":repository,
                        "name":s(&data, facet),"at":stamp,
                        "data":json!({"speaker":data["speaker"],"source":provenance}).to_string()})),
                )?;
            }
            store_graph::rows(
                g,
                "MATCH (c:Claim {id: $cid}) \
                 OPTIONAL MATCH (c)-[rs:HAS_SUBJECT]->() DELETE rs \
                 WITH c OPTIONAL MATCH (c)-[ro:HAS_OBJECT]->() DELETE ro \
                 WITH c MATCH (s:Entity {id: $sid}), (o:Entity {id: $oid}) \
                 CREATE (c)-[:HAS_SUBJECT]->(s) CREATE (c)-[:HAS_OBJECT]->(o) \
                 RETURN c.id AS id",
                store_graph::params(
                    &json!({"cid":claim_id,"sid":entities[0],"oid":entities[1]}),
                ),
            )?;
        }
        Ok(())
    })?;
    Ok(())
}
pub fn prepare(
    db: &Connection,
    graph: &GraphStore,
    repository: &Value,
    p: &Principal,
    args: &Value,
) -> Result<Value> {
    let docs = memory::documents(db, repository, p)?;
    let limit = crate::integer(args, "limit", 4, 12)?.max(1);
    // Any byte of a document: an indexed file or a converted or connector
    // document (earlier builds: 512 KiB).
    let limits = crate::limits::current();
    let start = crate::integer(
        args,
        "start_byte",
        0,
        limits.file_bytes.max(limits.document_text_bytes),
    )?;
    let mut sources = Vec::new();
    for doc in docs
        .iter()
        .filter(|d| args["source_id"].as_str().is_none_or(|id| s(d, "id") == id))
        .take(limit)
    {
        let read = memory::read_source(doc, &json!({"start_byte":start,"line_count":100}))?;
        let text = graphicle_memory::text::prefix(s(&read, "quote"), 1500, 8000);
        sources.push(json!({"id":doc["id"],"text":text,"sha256":doc["sha256"],"recorded_at":doc["recorded_at"],"scope":repository["id"],"start_byte":read["byte_start"],"next_byte":read["byte_start"].as_u64().unwrap_or(0)+text.len() as u64}));
    }
    if sources.is_empty() {
        return Err(Error::bad("No authorized sources to interpret"));
    }
    let targets = current(db, graph, repository, p)?
        .into_iter()
        .filter(|t| {
            t["data"]["evidence"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|e| sources.iter().any(|d| d["id"] == e["id"]))
        })
        .take(12)
        .collect::<Vec<_>>();
    let context = json!({"sources":sources,"source_state":memory::state(&docs),"targets":targets,"scope":repository["id"]});
    Ok(
        json!({"context":context,"sha256":digest(&context),"contract":native("interpretation.contract",json!({}))?,"model_calls":0}),
    )
}
impl Store {
    pub fn interpretation(
        &self,
        workspace: &str,
        token: &str,
        args: &Value,
        propose_draft: bool,
    ) -> Result<Value> {
        self.authorized(
            workspace,
            token,
            if propose_draft { "propose" } else { "read" },
            |db, p| {
                let repository = repo(db, p, string(args, "repository_id", 100)?)?;
                let prepared = prepare(db, &self.graph(), &repository, p, args)?;
                if !propose_draft {
                    return Ok(prepared);
                }
                if args["context_sha256"] != prepared["sha256"] {
                    return Err(Error::conflict(
                        "Interpretation context changed; prepare it again",
                    ));
                }
                propose(
                    db,
                    &self.graph(),
                    &repository,
                    p,
                    Proposal {
                        claims: &args["claims"],
                        sources: prepared["context"]["sources"].as_array().unwrap(),
                        targets: prepared["context"]["targets"].as_array().unwrap(),
                        provider: &json!({"kind":"caller_supplied","model_calls":0}),
                    },
                )
            },
        )
    }

    pub fn claim_history(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace, token, "read", |db, p| {
            let repository = repo(db, p, string(args, "repository_id", 100)?)?;
            let graph = self.graph();
            let memories = memory::list(db, &graph, &repository, p, true)?;
            let mut claims = Vec::new();
            let as_of = args["as_of"]
                .as_str()
                .map(graphicle_memory::temporal::instant)
                .transpose()
                .map_err(Error::bad)?;
            let known_at = args["known_at"]
                .as_str()
                .map(graphicle_memory::temporal::instant)
                .transpose()
                .map_err(Error::bad)?;
            for row in graph.read(|g| {
                store_graph::rows(
                    g,
                    "MATCH (c:Claim {repository_id: $repo}) RETURN c ORDER BY c.created_at, c.id",
                    store_graph::params(&json!({"repo": s(&repository, "id")})),
                )
            })? {
                let mut row = store_graph::node(&row, "c");
                crate::store::decode(&mut row, "data")?;
                if !memories.iter().any(|m| m["id"] == row["memory_id"]) {
                    continue;
                }
                // Reconstruct the knowledge view from immutable events. The
                // current row may contain an invalidation learned much later.
                if let Some(at) = known_at {
                    let mut selected = None;
                    for mut event in graph.read(|g| {
                        store_graph::rows(
                            g,
                            "MATCH (e:ClaimEvent {repository_id: $repo, claim_id: $cid}) \
                             RETURN e.action AS action, e.data AS data, e.created_at AS created_at \
                             ORDER BY e.created_at, e.id",
                            store_graph::params(
                                &json!({"repo":s(&repository,"id"),"cid":s(&row,"id")}),
                            ),
                        )
                    })? {
                        if graphicle_memory::temporal::instant(s(&event, "created_at"))
                            .map_err(Error::bad)?
                            <= at
                        {
                            crate::store::decode(&mut event, "data")?;
                            selected = Some(event);
                        }
                    }
                    let Some(event) = selected else {
                        continue;
                    };
                    row["data"] = event["data"].clone();
                    row["state"] = json!(if event["action"] == "RETIRE" {
                        "retired"
                    } else if ["KEEP", "CONFIRM", "RETRACT"].contains(&s(&event, "action")) {
                        "event"
                    } else {
                        "active"
                    });
                    row["updated_at"] = event["created_at"].clone();
                }
                let data = &row["data"];
                if (known_at.is_some() || as_of.is_some()) && data["approved_at"].is_null() {
                    continue;
                }
                if (known_at.is_some() || as_of.is_some())
                    && row["state"] == "retired"
                    && data["retirement_action"] != "SUPERSEDE"
                {
                    continue;
                }
                if let Some(at) = as_of {
                    let lower = data["valid_at"]
                        .as_str()
                        .map(graphicle_memory::temporal::instant)
                        .transpose()
                        .map_err(Error::bad)?;
                    let upper = data["invalid_at"]
                        .as_str()
                        .map(graphicle_memory::temporal::instant)
                        .transpose()
                        .map_err(Error::bad)?;
                    if lower.is_some_and(|t| t > at) || upper.is_some_and(|t| t <= at) {
                        continue;
                    }
                }
                claims.push(row);
            }
            Ok(json!({"claims":claims,"unknown_dates_are_not_inferred":true}))
        })
    }
}
