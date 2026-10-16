//! Intersect explicit local sharing with current provider-side membership:
//! the read-time enforcement of `permission_mode: provider`. It fails closed
//! when a document's provider permissions are missing, incomplete or older
//! than the connector allows, for example in a store whose provider
//! permissions no installed extension keeps current.
//!
//! The account side lives in the graph store: `(:ProviderIdentity)`,
//! `(:OAuthAccount)`, `(:ProviderEmail)`. A grant matches by list membership
//! against the principal's identities — no `json_each` joins.
use crate::{
    Result,
    store::{Principal, s},
    store_graph::{self as graph, GraphStore, Params, json_to_value},
};
use serde_json::{Value, json};

pub fn allows(
    store: &GraphStore,
    workspace: &str,
    provider: &str,
    permission: &Value,
    p: &Principal,
    max_age: i64,
) -> Result<bool> {
    if permission["complete"] != true {
        return Ok(false);
    }
    let fresh = permission["checked_at"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .is_some_and(|t| {
            (0..=max_age).contains(&chrono::Utc::now().signed_duration_since(t).num_seconds())
        });
    if !fresh {
        return Ok(false);
    }
    // The source-fetch credential has demonstrated access for the importing owner.
    if p.role == "owner" {
        return Ok(true);
    }
    let (identities, accounts) = store.read(|db| {
        let identities = graph::rows(
            db,
            "MATCH (i:ProviderIdentity {workspace_id: $workspace, provider: $provider, credential_id: $credential}) RETURN i.external_id AS external_id",
            Params::from([
                ("workspace".into(), json_to_value(&json!(workspace))),
                ("provider".into(), json_to_value(&json!(provider))),
                ("credential".into(), json_to_value(&json!(p.credential))),
            ]),
        )?;
        let mut accounts = graph::rows(
            db,
            "MATCH (a:OAuthAccount {workspace_id: $workspace, provider: $provider, state: 'active'}) RETURN a.external_id AS external_id, a.email AS email",
            Params::from([
                ("workspace".into(), json_to_value(&json!(workspace))),
                ("provider".into(), json_to_value(&json!(provider))),
            ]),
        )?;
        accounts.extend(graph::rows(
            db,
            "MATCH (e:ProviderEmail {workspace_id: $workspace, provider: $provider}) RETURN e.external_id AS external_id, e.email AS email",
            Params::from([
                ("workspace".into(), json_to_value(&json!(workspace))),
                ("provider".into(), json_to_value(&json!(provider))),
            ]),
        )?);
        Ok((identities, accounts))
    })?;
    for grant in permission["principals"].as_array().into_iter().flatten() {
        if grant["type"] == "anyone" {
            return Ok(true);
        }
        for identity in &identities {
            if grant["type"] == "user" && grant["id"] == identity["external_id"] {
                return Ok(true);
            }
            if let Some(account) = accounts
                .iter()
                .find(|a| a["external_id"] == identity["external_id"])
            {
                let email = s(account, "email").to_lowercase();
                if !email.is_empty()
                    && grant["type"] == "email"
                    && s(grant, "email").eq_ignore_ascii_case(&email)
                {
                    return Ok(true);
                }
                if !email.is_empty()
                    && grant["type"] == "domain"
                    && email
                        .rsplit_once('@')
                        .is_some_and(|(_, domain)| domain.eq_ignore_ascii_case(s(grant, "domain")))
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}
