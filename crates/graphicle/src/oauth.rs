//! Authorization-code connections with one-use state, PKCE and encrypted tokens.
//!
//! Accounts, pending requests and provider identities live in the graph
//! store: `(:OAuthAccount)-[:USES]->(:Secret)`, `(:OAuthPending)`,
//! `(:ProviderIdentity)` (see `cypherlite-migration.md` §2.5).
use crate::{
    Error, Result, digest, id, local, model, now,
    store::{Store, authorize_hash, s},
    store_graph::{self as graph, Params, json_to_value},
    string,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The `RETURN` projection standing in for `SELECT * FROM am_oauth_accounts`.
const ACCOUNT_COLS: &str = "a.id AS id, a.workspace_id AS workspace_id, a.provider AS provider, a.external_id AS external_id, a.email AS email, a.display_name AS display_name, a.token_secret AS token_secret, a.refresh_secret AS refresh_secret, a.expires_at AS expires_at, a.scopes AS scopes, a.state AS state, a.updated_at AS updated_at";
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub client_id: String,
    pub client_secret_ref: Option<String>,
    pub authorization_url: String,
    pub token_url: String,
    pub identity_url: String,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub pkce: bool,
}
fn providers(store: &Store, workspace: &str) -> Result<BTreeMap<String, Provider>> {
    let path = store.directory.join(format!("oauth-{workspace}.json"));
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    if path.is_symlink() {
        return Err(Error::bad("OAuth configuration cannot be a symlink"));
    }
    let bytes = std::fs::read(path)?;
    if bytes.len() > 65536 {
        return Err(Error::bad("OAuth configuration exceeds limits"));
    }
    serde_json::from_slice(&bytes).map_err(Into::into)
}
/// The OAuth providers of the core's connectors; an installed extension's
/// sources may use others (`extension::Source::oauth_provider`).
const CORE_PROVIDERS: &[&str] = &["github", "slack", "notion", "gdrive", "atlassian"];
fn validate(name: &str, p: &Provider) -> Result<()> {
    if !(CORE_PROVIDERS.contains(&name)
        || crate::extension::installed().oauth_source(name).is_some())
        || p.client_id.is_empty()
        || p.client_id.len() > 1000
        || p.scopes.len() > 30
    {
        return Err(Error::bad("Invalid OAuth provider"));
    }
    for value in [&p.authorization_url, &p.token_url, &p.identity_url] {
        model::checked_url(value)?;
    }
    if p.scopes
        .iter()
        .any(|v| v.is_empty() || v.len() > 200 || v.contains(['\r', '\n']))
    {
        return Err(Error::bad("Invalid OAuth scope"));
    }
    if !p.pkce && p.client_secret_ref.is_none() {
        return Err(Error::bad(
            "A confidential OAuth client secret is required when PKCE is unavailable",
        ));
    }
    Ok(())
}
pub fn configure(store: &Store, workspace: &str, capability: &str, args: &Value) -> Result<Value> {
    store.authorized(workspace, capability, "admin", |_, _| Ok(()))?;
    let name = string(args, "provider", 20)?;
    let config: Provider = serde_json::from_value(args["config"].clone())?;
    validate(name, &config)?;
    if let Some(reference) = &config.client_secret_ref {
        store.resolve_credential(workspace, reference)?;
    }
    let mut all = providers(store, workspace)?;
    all.insert(name.to_string(), config);
    // Configuration is an administrator-owned local file; it never accepts a path from sources.
    local::write_private(
        &store.directory.join(format!("oauth-{workspace}.json")),
        &serde_json::to_vec_pretty(&all)?,
    )?;
    Ok(json!({"configured":name}))
}
pub fn status(store: &Store, workspace: &str, capability: &str) -> Result<Value> {
    store.authorized(workspace,capability,"read",|_,p|{
        let accounts=if p.permits("admin"){store.graph().read(|db|graph::rows(db,"MATCH (a:OAuthAccount {workspace_id: $workspace}) RETURN a.id AS id, a.provider AS provider, a.external_id AS external_id, a.email AS email, a.display_name AS display_name, a.expires_at AS expires_at, a.scopes AS scopes, a.state AS state ORDER BY a.display_name",Params::from([("workspace".into(),json_to_value(&json!(workspace)))])))?}else{vec![]};
        Ok(json!({"providers":providers(store,workspace)?.keys().collect::<Vec<_>>(),"accounts":accounts}))
    })
}
pub fn start(
    store: &Store,
    workspace: &str,
    capability: &str,
    name: &str,
    redirect: &str,
) -> Result<Value> {
    let principal = store.authorized(workspace, capability, "admin", |_, p| Ok(p.clone()))?;
    let all = providers(store, workspace)?;
    let config = all
        .get(name)
        .ok_or_else(|| Error::bad("Register this OAuth application in Connections first"))?;
    validate(name, config)?;
    model::checked_url(redirect)?;
    let mut random = [0u8; 32];
    OsRng.fill_bytes(&mut random);
    let verifier = URL_SAFE_NO_PAD.encode(random);
    OsRng.fill_bytes(&mut random);
    let state = URL_SAFE_NO_PAD.encode(random);
    let state_hash = digest(&json!(state));
    let binding =
        json!({"verifier":verifier,"configuration":digest(&serde_json::to_value(config)?)})
            .to_string();
    let secret = store.save_secret(workspace, &format!("oauth-verifier:{state_hash}"), &binding)?;
    let expires = (chrono::Utc::now() + chrono::Duration::minutes(10))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    store.transaction(|db|{
        authorize_hash(db,workspace,&principal.hash,"admin")?;
        Ok(())
    })?;
    store.graph().transaction(|db|{
        // Expired or used requests go, with the secrets that bind their verifiers
        // (the SQL store cascaded the row with its secret).
        graph::write(db,"MATCH (p:OAuthPending {workspace_id: $workspace}) WHERE p.expires_at < $now OR p.used = true WITH p, p.verifier_secret AS secret DETACH DELETE p WITH collect(secret) AS secrets MATCH (s:Secret) WHERE s.id IN secrets DELETE s",Params::from([("workspace".into(),json_to_value(&json!(workspace))),("now".into(),json_to_value(&json!(now())))]))?;
        let count=graph::one(db,"MATCH (p:OAuthPending {workspace_id: $workspace}) RETURN count(p) AS count",Params::from([("workspace".into(),json_to_value(&json!(workspace)))]))?.and_then(|r|r["count"].as_i64()).unwrap_or(0);
        if count>=20{return Err(Error::bad("Too many pending connection requests; allow them to expire"));}
        graph::write(db,"CREATE (p:OAuthPending {state_hash: $state_hash, workspace_id: $workspace, principal_hash: $principal, provider: $provider, redirect_uri: $redirect, verifier_secret: $secret, expires_at: $expires, used: false})",Params::from([
            ("state_hash".into(),json_to_value(&json!(state_hash))),
            ("workspace".into(),json_to_value(&json!(workspace))),
            ("principal".into(),json_to_value(&json!(principal.hash))),
            ("provider".into(),json_to_value(&json!(name))),
            ("redirect".into(),json_to_value(&json!(redirect))),
            ("secret".into(),json_to_value(&json!(secret))),
            ("expires".into(),json_to_value(&json!(expires))),
        ]))?;
        Ok(())
    })?;
    let mut url = model::checked_url(&config.authorization_url)?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", &config.client_id)
            .append_pair("redirect_uri", redirect)
            .append_pair("response_type", "code")
            .append_pair("state", &state);
        q.append_pair(
            if name == "slack" {
                "user_scope"
            } else {
                "scope"
            },
            &config.scopes.join(if name == "slack" { "," } else { " " }),
        );
        if config.pkce {
            q.append_pair("code_challenge_method", "S256").append_pair(
                "code_challenge",
                &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
            );
        }
        if name == "gdrive" {
            q.append_pair("access_type", "offline")
                .append_pair("prompt", "consent");
        }
        if name == "notion" {
            q.append_pair("owner", "user");
        }
        if name == "atlassian" {
            q.append_pair("audience", "api.atlassian.com")
                .append_pair("prompt", "consent");
        }
    }
    Ok(json!({"authorization_url":url.as_str(),"expires_at":expires,"provider":name}))
}
async fn exchange(
    client: &reqwest::Client,
    store: &Store,
    workspace: &str,
    name: &str,
    p: &Provider,
    form: &[(String, String)],
) -> Result<Value> {
    let mut request = client
        .post(&p.token_url)
        .header("Accept", "application/json");
    let mut values = form.to_vec();
    values.push(("client_id".into(), p.client_id.clone()));
    if let Some(secret) = &p.client_secret_ref {
        let secret = store.resolve_credential(workspace, secret)?;
        if name == "notion" {
            request = request.basic_auth(&p.client_id, Some(secret));
        } else {
            values.push(("client_secret".into(), secret));
        }
    }
    if ["notion", "atlassian"].contains(&name) {
        request = request.json(&values.iter().cloned().collect::<BTreeMap<_, _>>());
    } else {
        request = request.form(&values);
    }
    let value = model::bounded_json(
        request
            .send()
            .await
            .map_err(|_| Error::bad("OAuth token exchange failed"))?,
    )
    .await?;
    if value.get("error").is_some() || value.get("ok") == Some(&json!(false)) {
        return Err(Error::bad(
            "Provider declined authorization; reconnect the account",
        ));
    }
    Ok(value)
}
fn token_fields<'a>(value: &'a Value, name: &str) -> &'a Value {
    if name == "slack" && value["authed_user"]["access_token"].is_string() {
        &value["authed_user"]
    } else {
        value
    }
}
fn access(value: &Value, name: &str) -> Result<String> {
    Ok(string(token_fields(value, name), "access_token", 32768)?.to_string())
}
pub async fn callback(
    store: &Store,
    client: &reqwest::Client,
    name: &str,
    state: &str,
    code: &str,
) -> Result<Value> {
    if state.len() > 200 || code.len() > 8192 || code.is_empty() {
        return Err(Error::bad("Invalid OAuth response"));
    }
    let state_hash = digest(&json!(state));
    let pending=store.graph().transaction(|db|{
        let p=graph::one(db,"MATCH (p:OAuthPending {state_hash: $state_hash, provider: $provider, used: false}) WHERE p.expires_at > $now RETURN p.workspace_id AS workspace_id, p.principal_hash AS principal_hash, p.redirect_uri AS redirect_uri, p.verifier_secret AS verifier_secret",Params::from([("state_hash".into(),json_to_value(&json!(state_hash))),("provider".into(),json_to_value(&json!(name))),("now".into(),json_to_value(&json!(now())))]))?.ok_or_else(||Error::bad("Connection request expired or was already used"))?;
        graph::write(db,"MATCH (p:OAuthPending {state_hash: $state_hash}) SET p.used = true",Params::from([("state_hash".into(),json_to_value(&json!(state_hash)))]))?;
        Ok(p)
    })?;
    store.transaction(|db|{
        authorize_hash(db,s(&pending,"workspace_id"),s(&pending,"principal_hash"),"admin")?;
        Ok(())
    })?;
    let workspace = s(&pending, "workspace_id");
    let all = providers(store, workspace)?;
    let config = all
        .get(name)
        .ok_or_else(|| Error::bad("OAuth provider was removed"))?;
    let bound: Value =
        serde_json::from_str(&store.secret(workspace, s(&pending, "verifier_secret"))?)?;
    if bound["configuration"] != digest(&serde_json::to_value(config)?) {
        return Err(Error::conflict(
            "OAuth application changed; start a new connection",
        ));
    }
    let verifier = string(&bound, "verifier", 200)?.to_string();
    let mut form = vec![
        ("grant_type".into(), "authorization_code".into()),
        ("code".into(), code.into()),
        ("redirect_uri".into(), s(&pending, "redirect_uri").into()),
    ];
    if config.pkce {
        form.push(("code_verifier".into(), verifier));
    }
    let tokens = exchange(client, store, workspace, name, config, &form).await?;
    let token = access(&tokens, name)?;
    let mut request = client
        .get(&config.identity_url)
        .bearer_auth(&token)
        .header("User-Agent", "Graphicle/0.3");
    if name == "notion" {
        request = request.header("Notion-Version", "2025-09-03");
    }
    let who = model::bounded_json(
        request
            .send()
            .await
            .map_err(|_| Error::bad("Account identity request failed"))?,
    )
    .await?;
    if who.get("ok") == Some(&json!(false)) {
        return Err(Error::bad("Could not verify provider account"));
    }
    // A provider an extension's source uses reads its own identity response.
    let added = if CORE_PROVIDERS.contains(&name) {
        None
    } else {
        crate::extension::installed()
            .oauth_source(name)
            .and_then(|source| source.oauth_identity(&who))
    };
    let external = match name {
        "github" => who["id"]
            .as_i64()
            .map(|v| v.to_string())
            .or_else(|| who["id"].as_str().map(str::to_string)),
        "slack" => who["user_id"].as_str().map(str::to_string),
        "gdrive" => who["sub"].as_str().map(str::to_string),
        "atlassian" => who["account_id"].as_str().map(str::to_string),
        "notion" => tokens["owner"]["user"]["id"]
            .as_str()
            .or(who["id"].as_str())
            .map(str::to_string),
        _ => added.as_ref().map(|(account, _)| account.clone()),
    }
    .filter(|v| !v.is_empty())
    .ok_or_else(|| Error::bad("Provider did not return a stable account identity"))?;
    let display = match added {
        Some((_, display)) if !display.is_empty() => display,
        _ => who["name"]
            .as_str()
            .or(who["login"].as_str())
            .or(who["user"].as_str())
            .unwrap_or(&external)
            .to_string(),
    };
    let email = if name == "gdrive" && who["email_verified"] == true {
        who["email"].as_str()
    } else {
        None
    };
    let token_secret = store.save_secret(workspace, &format!("{name}:access"), &token)?;
    let token_metadata = token_fields(&tokens, name);
    let refresh_secret = token_metadata["refresh_token"]
        .as_str()
        .map(|s| store.save_secret(workspace, &format!("{name}:refresh"), s))
        .transpose()?;
    let expiry = token_metadata["expires_in"]
        .as_i64()
        .filter(|n| *n > 0)
        .map(|n| {
            (chrono::Utc::now() + chrono::Duration::seconds(n.min(31536000)))
                .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
        });
    let identifier = id();
    let principal = store.transaction(|db| {
        authorize_hash(db, workspace, s(&pending, "principal_hash"), "admin")
    })?;
    store.graph().transaction(|db|{
        graph::write(db,"CREATE (a:OAuthAccount {id: $id, workspace_id: $workspace, provider: $provider, external_id: $external, email: $email, display_name: $display, token_secret: $token_secret, refresh_secret: $refresh_secret, expires_at: $expiry, scopes: $scopes, state: 'active', updated_at: $now})",Params::from([
            ("id".into(),json_to_value(&json!(identifier))),
            ("workspace".into(),json_to_value(&json!(workspace))),
            ("provider".into(),json_to_value(&json!(name))),
            ("external".into(),json_to_value(&json!(external))),
            ("email".into(),email.as_ref().map(|e|json_to_value(&json!(e))).unwrap_or(cypherlite::Value::Null)),
            ("display".into(),json_to_value(&json!(display))),
            ("token_secret".into(),json_to_value(&json!(token_secret))),
            ("refresh_secret".into(),refresh_secret.as_ref().map(|r|json_to_value(&json!(r))).unwrap_or(cypherlite::Value::Null)),
            ("expiry".into(),expiry.as_ref().map(|e|json_to_value(&json!(e))).unwrap_or(cypherlite::Value::Null)),
            ("scopes".into(),json_to_value(&json!(token_metadata["scope"].as_str().unwrap_or("")))),
            ("now".into(),json_to_value(&json!(now()))),
        ]))?;
        let mut used_secrets = vec![&token_secret];
        if let Some(refresh) = &refresh_secret {
            used_secrets.push(refresh);
        }
        for secret in used_secrets {
            graph::write(db,"MATCH (a:OAuthAccount {id: $id}), (s:Secret {id: $secret}) MERGE (a)-[:USES]->(s)",Params::from([("id".into(),json_to_value(&json!(identifier))),("secret".into(),json_to_value(&json!(secret)))]))?;
        }
        graph::write(db,"MERGE (i:ProviderIdentity {workspace_id: $workspace, provider: $provider, external_id: $external, credential_id: $credential}) SET i.verified_at = $now",Params::from([
            ("workspace".into(),json_to_value(&json!(workspace))),
            ("provider".into(),json_to_value(&json!(name))),
            ("external".into(),json_to_value(&json!(external))),
            ("credential".into(),json_to_value(&json!(principal.credential))),
            ("now".into(),json_to_value(&json!(now()))),
        ]))?;
        graph::write(db,"MATCH (p:OAuthPending {state_hash: $state_hash}) DELETE p",Params::from([("state_hash".into(),json_to_value(&json!(state_hash)))]))?;
        graph::write(db,"MATCH (s:Secret {id: $id}) DELETE s",Params::from([("id".into(),json_to_value(&json!(s(&pending,"verifier_secret"))))]))?;
        Ok(json!({"connected":true,"provider":name,"account_id":identifier,"display_name":display,"credential_ref":format!("oauth:{identifier}")}))
    })
}
pub async fn refresh(
    store: &Store,
    client: &reqwest::Client,
    workspace: &str,
    capability: &str,
    identifier: &str,
) -> Result<Value> {
    let principal = store.authorized(workspace, capability, "admin", |_, p| Ok(p.hash.clone()))?;
    refresh_as(store, client, workspace, &principal, "admin", identifier).await
}
async fn refresh_as(
    store: &Store,
    client: &reqwest::Client,
    workspace: &str,
    principal: &str,
    permission: &str,
    identifier: &str,
) -> Result<Value> {
    store.transaction(|db| {
        authorize_hash(db, workspace, principal, permission).map(|_| ())
    })?;
    let account = store.graph().read(|db| {
        graph::one(
            db,
            &format!("MATCH (a:OAuthAccount {{workspace_id: $workspace, id: $id, state: 'active'}}) RETURN {ACCOUNT_COLS}"),
            Params::from([
                ("workspace".into(), json_to_value(&json!(workspace))),
                ("id".into(), json_to_value(&json!(identifier))),
            ]),
        )
    })?
    .ok_or_else(|| Error::bad("Account unavailable"))?;
    let name = s(&account, "provider");
    let all = providers(store, workspace)?;
    let profile = profile_for(&all, name)?;
    let refresh = store.secret(workspace, s(&account, "refresh_secret"))?;
    let response = exchange(
        client,
        store,
        workspace,
        name,
        profile,
        &[
            ("grant_type".into(), "refresh_token".into()),
            ("refresh_token".into(), refresh),
        ],
    )
    .await?;
    let access = store.save_secret(
        workspace,
        &format!("{name}:access"),
        &access(&response, name)?,
    )?;
    let token_metadata = token_fields(&response, name);
    let renewed = token_metadata["refresh_token"]
        .as_str()
        .map(|v| store.save_secret(workspace, &format!("{name}:refresh"), v))
        .transpose()?;
    let expires = token_metadata["expires_in"]
        .as_i64()
        .filter(|n| *n > 0)
        .map(|n| {
            (chrono::Utc::now() + chrono::Duration::seconds(n.min(31536000)))
                .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
        });
    let result=store.transaction(|db|{
        authorize_hash(db,workspace,principal,permission)?;
        Ok(())
    }).and_then(|_| store.graph().transaction(|db|{
        let found=graph::one(db,"MATCH (a:OAuthAccount {workspace_id: $workspace, id: $id, state: 'active', token_secret: $old_token}) RETURN a.id AS id",Params::from([
            ("workspace".into(),json_to_value(&json!(workspace))),
            ("id".into(),json_to_value(&json!(identifier))),
            ("old_token".into(),json_to_value(&json!(s(&account,"token_secret")))),
        ]))?;
        if found.is_none(){return Err(Error::conflict("Account changed while refreshing"));}
        graph::write(db,"MATCH (a:OAuthAccount {id: $id}) SET a.token_secret=$access, a.refresh_secret=CASE WHEN $renewed IS NULL THEN a.refresh_secret ELSE $renewed END, a.expires_at=$expires, a.updated_at=$now",Params::from([
            ("id".into(),json_to_value(&json!(identifier))),
            ("access".into(),json_to_value(&json!(access))),
            ("renewed".into(),renewed.as_ref().map(|r|json_to_value(&json!(r))).unwrap_or(cypherlite::Value::Null)),
            ("expires".into(),expires.as_ref().map(|e|json_to_value(&json!(e))).unwrap_or(cypherlite::Value::Null)),
            ("now".into(),json_to_value(&json!(now()))),
        ]))?;
        let mut replaced = vec![s(&account,"token_secret").to_string()];
        if renewed.is_some(){replaced.push(s(&account,"refresh_secret").to_string());}
        for secret in replaced {
            graph::write(db,"MATCH (s:Secret {id: $id}) DELETE s",Params::from([("id".into(),json_to_value(&json!(secret)))]))?;
        }
        Ok(json!({"refreshed":true,"expires_at":expires}))
    }));
    if result.is_err() {
        store.graph().transaction(|db| {
            let mut stale = vec![access.clone()];
            if let Some(renewed) = &renewed {
                stale.push(renewed.clone());
            }
            for secret in stale {
                graph::write(
                    db,
                    "MATCH (s:Secret {id: $id}) DELETE s",
                    Params::from([("id".into(), json_to_value(&json!(secret)))]),
                )?;
            }
            Ok(())
        })?;
    }
    result
}
fn profile_for<'a>(profiles: &'a BTreeMap<String, Provider>, name: &str) -> Result<&'a Provider> {
    profiles
        .get(name)
        .ok_or_else(|| Error::bad("Provider unavailable"))
}
/// Called only after a connector job has passed its normal source/actor guard.
pub(crate) async fn for_connector(
    store: &Store,
    client: &reqwest::Client,
    job: &crate::jobs::Job,
    reference: &str,
) -> Result<String> {
    if let Some(identifier) = reference.strip_prefix("oauth:") {
        store.transaction(|db| {
            authorize_hash(db, &job.workspace, &job.principal_hash, "collect").map(|_| ())
        })?;
        let account = store.graph().read(|db| {
            graph::one(
                db,
                &format!("MATCH (a:OAuthAccount {{workspace_id: $workspace, id: $id, state: 'active'}}) RETURN {ACCOUNT_COLS}"),
                Params::from([
                    ("workspace".into(), json_to_value(&json!(job.workspace))),
                    ("id".into(), json_to_value(&json!(identifier))),
                ]),
            )
        })?
        .ok_or_else(|| Error::bad("Connected account unavailable"))?;
        let expiring = account["expires_at"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .is_some_and(|at| at < chrono::Utc::now() + chrono::Duration::seconds(60));
        if expiring {
            refresh_as(
                store,
                client,
                &job.workspace,
                &job.principal_hash,
                "collect",
                identifier,
            )
            .await?;
        }
    }
    store.resolve_credential(&job.workspace, reference)
}
pub fn disconnect(
    store: &Store,
    workspace: &str,
    capability: &str,
    identifier: &str,
) -> Result<Value> {
    store.authorized(workspace,capability,"admin",|db,_|{
        let account=store.graph().read(|db|graph::one(db,&format!("MATCH (a:OAuthAccount {{workspace_id: $workspace, id: $id}}) RETURN {ACCOUNT_COLS}"),Params::from([("workspace".into(),json_to_value(&json!(workspace))),("id".into(),json_to_value(&json!(identifier)))])))?.ok_or_else(||Error::bad("Account unavailable"))?;
        let reference=format!("oauth:{identifier}");
        let project_ids=crate::store::rows(db,"SELECT id FROM repo_projects WHERE workspace_id=?",[workspace])?.iter().map(|r| s(r,"id").to_string()).collect::<Vec<_>>();
        let projects=store.graph().transaction(|db|{
            let projects=graph::rows(db,"MATCH (c:Connector) WHERE c.credential_env=$reference AND c.repository_id IN $projects RETURN c.repository_id AS repository_id",Params::from([("reference".into(),json_to_value(&json!(reference))),("projects".into(),json_to_value(&json!(project_ids)))]))?;
            graph::write(db,"MATCH (c:Connector) WHERE c.credential_env=$reference AND c.repository_id IN $projects SET c.state='paused', c.last_sync=NULL, c.generation=c.generation+1",Params::from([("reference".into(),json_to_value(&json!(reference))),("projects".into(),json_to_value(&json!(project_ids)))]))?;
            graph::write(db,"MATCH (a:OAuthAccount {id: $id}) DETACH DELETE a",Params::from([("id".into(),json_to_value(&json!(identifier)))]))?;
            for secret in [s(&account,"token_secret"),s(&account,"refresh_secret")] {
                if !secret.is_empty() {
                    graph::write(db,"MATCH (s:Secret {id: $id}) DELETE s",Params::from([("id".into(),json_to_value(&json!(secret)))]))?;
                }
            }
            Ok(projects)
        })?;
        for p in projects{crate::jobs::clear_model_cache(&store.graph(),s(&p,"repository_id"))?;crate::store::invalidate(db,s(&p,"repository_id"))?;}
        Ok(json!({"disconnected":true,"provider_token_revoked":false}))
    })
}
pub async fn resources(
    store: &Store,
    client: &reqwest::Client,
    workspace: &str,
    token: &str,
    args: &Value,
) -> Result<Value> {
    let account_id = string(args, "account_id", 100)?;
    store.authorized(workspace,token,"admin",|_,_|Ok(()))?;
    let account=store.graph().read(|db|{
        graph::one(db,&format!("MATCH (a:OAuthAccount {{workspace_id: $workspace, id: $id, provider: 'atlassian', state: 'active'}}) RETURN {ACCOUNT_COLS}"),Params::from([("workspace".into(),json_to_value(&json!(workspace))),("id".into(),json_to_value(&json!(account_id)))]))
    })?.ok_or_else(||Error::bad("Choose a connected Atlassian account"))?;
    if account["expires_at"]
        .as_str()
        .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
        .is_some_and(|at| at < chrono::Utc::now() + chrono::Duration::seconds(60))
    {
        refresh(store, client, workspace, token, account_id).await?;
    }
    let profiles = providers(store, workspace)?;
    let profile = profile_for(&profiles, "atlassian")?;
    let origin = model::checked_url(&profile.identity_url)?
        .origin()
        .ascii_serialization();
    let response = model::bounded_json(
        client
            .get(format!("{origin}/oauth/token/accessible-resources"))
            .bearer_auth(store.resolve_credential(workspace, &format!("oauth:{account_id}"))?)
            .send()
            .await
            .map_err(|_| Error::bad("Cannot list authorized Atlassian sites"))?,
    )
    .await?;
    let rows = response
        .as_array()
        .filter(|v| v.len() <= 500)
        .ok_or_else(|| Error::bad("Invalid Atlassian site list"))?;
    let sites: Vec<Value> = rows
        .iter()
        .map(|r| {
            string(r, "id", 200)?;
            string(r, "name", 300)?;
            model::checked_url(string(r, "url", 2000)?)?;
            Ok(json!({"id":r["id"],"name":r["name"],"url":r["url"],"scopes":r["scopes"]}))
        })
        .collect::<Result<_>>()?;
    store.authorized(workspace, token, "admin", |_, _| Ok(json!({"sites":sites})))
}
