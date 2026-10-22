//! Workspace-bound authenticated encryption for provider secrets. The key is
//! the device key file `vault.key`, unless an installed extension holds it
//! ([`crate::extension::Extension::vault_key`]).
//!
//! Secrets live in the graph store as `(:Secret {id, workspace_id, name,
//! ciphertext, nonce, updated_at})` nodes; ciphertext and nonce are base64
//! strings (the graph value model has no byte-string type).
use crate::{
    Error, Result, id, now,
    store::{Store, s},
    store_graph::{self as graph, Params, b64d, b64e, json_to_value},
    string,
};
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use rand::{RngCore, rngs::OsRng};
use serde_json::{Value, json};
use std::io::{Read, Write};

impl Store {
    fn vault_key(&self) -> Result<[u8; 32]> {
        // An extension that holds the key answers for it, errors included.
        if let Some(key) = crate::extension::installed().vault_key(self) {
            return key;
        }
        let path = self.directory.join("vault.key");
        let mut temporary = tempfile::NamedTempFile::new_in(&*self.directory)?;
        let mut new_key = [0u8; 32];
        OsRng.fill_bytes(&mut new_key);
        temporary.write_all(&new_key)?;
        temporary.as_file().sync_all()?;
        match temporary.persist_noclobber(&path) {
            Ok(_) => Ok(new_key),
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                let mut options = std::fs::OpenOptions::new();
                options.read(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.custom_flags(libc::O_NOFOLLOW);
                }
                let mut file = options.open(&path)?;
                let metadata = file.metadata()?;
                if !metadata.is_file() || metadata.len() != 32 {
                    return Err(Error::bad("Invalid local vault key"));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if metadata.permissions().mode() & 0o077 != 0 {
                        return Err(Error::bad("Vault key permissions must be owner-only"));
                    }
                }
                let mut key = [0u8; 32];
                file.read_exact(&mut key)?;
                Ok(key)
            }
            Err(e) => Err(e.error.into()),
        }
    }
    pub fn save_secret(&self, workspace: &str, name: &str, value: &str) -> Result<String> {
        if value.is_empty() || value.len() > 32768 {
            return Err(Error::bad("Credential must contain 1–32768 bytes"));
        }
        let identifier = id();
        let key = self.vault_key()?;
        let cipher =
            Aes256Gcm::new_from_slice(&key).map_err(|_| Error::bad("Vault key invalid"))?;
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        // Frozen AEAD associated data: stored credentials are sealed with it, so
        // changing it makes them undecryptable. `secret` opens with the same value.
        let aad = format!("Graphicle/v1/{workspace}/{identifier}/{name}");
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: value.as_bytes(),
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| Error::bad("Credential encryption failed"))?;
        self.graph().transaction(|db| {
            graph::write(
                db,
                "CREATE (s:Secret {id: $id, workspace_id: $workspace, name: $name, ciphertext: $ciphertext, nonce: $nonce, updated_at: $updated_at})",
                Params::from([
                    ("id".into(), json_to_value(&json!(identifier))),
                    ("workspace".into(), json_to_value(&json!(workspace))),
                    ("name".into(), json_to_value(&json!(name))),
                    ("ciphertext".into(), json_to_value(&json!(b64e(&ciphertext)))),
                    ("nonce".into(), json_to_value(&json!(b64e(&nonce)))),
                    ("updated_at".into(), json_to_value(&json!(now()))),
                ]),
            )?;
            Ok(())
        })?;
        Ok(identifier)
    }
    pub fn secret(&self, workspace: &str, identifier: &str) -> Result<String> {
        let row = self.graph().read(|db| {
            graph::one(
                db,
                "MATCH (s:Secret {workspace_id: $workspace, id: $id}) RETURN s.name AS name, s.ciphertext AS ciphertext, s.nonce AS nonce",
                Params::from([
                    ("workspace".into(), json_to_value(&json!(workspace))),
                    ("id".into(), json_to_value(&json!(identifier))),
                ]),
            )
        })?
        .ok_or_else(|| Error::bad("Stored credential is unavailable"))?;
        let name = s(&row, "name").to_string();
        let bytes = b64d(s(&row, "ciphertext"))?;
        let nonce = b64d(s(&row, "nonce"))?;
        if nonce.len() != 12 {
            return Err(Error::bad("Credential integrity check failed"));
        }
        let cipher = Aes256Gcm::new_from_slice(&self.vault_key()?)
            .map_err(|_| Error::bad("Vault key invalid"))?;
        // Frozen AEAD associated data: the value the credential was sealed with
        // above; changing it makes stored credentials undecryptable.
        let aad = format!("Graphicle/v1/{workspace}/{identifier}/{name}");
        let plain = cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &bytes,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| Error::bad("Credential integrity check failed"))?;
        String::from_utf8(plain).map_err(|_| Error::bad("Credential encoding invalid"))
    }
    pub fn put_credential(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace, token, "admin", |_, _| Ok(()))?;
        let identifier = self.save_secret(
            workspace,
            string(args, "name", 100)?,
            string(args, "value", 32768)?,
        )?;
        // Revalidate after writing; a concurrently revoked caller cannot use the result.
        if self
            .authorized(workspace, token, "admin", |_, _| Ok(()))
            .is_err()
        {
            self.graph().transaction(|db| {
                graph::write(
                    db,
                    "MATCH (s:Secret {id: $id}) DELETE s",
                    Params::from([("id".into(), json_to_value(&json!(identifier)))]),
                )
            })?;
            return Err(Error::denied());
        }
        Ok(json!({"id":identifier,"credential_ref":format!("secret:{identifier}"),"stored":true}))
    }
    pub fn list_credentials(&self, workspace: &str, token: &str) -> Result<Value> {
        self.authorized(workspace, token, "admin", |_, _| {
            Ok(json!(self.graph().read(|db| {
                graph::rows(
                    db,
                    "MATCH (s:Secret {workspace_id: $workspace})
                     WHERE NOT s.name STARTS WITH 'oauth-verifier:'
                     RETURN s.id AS id, s.name AS name, s.updated_at AS updated_at
                     ORDER BY s.name",
                    Params::from([("workspace".into(), json_to_value(&json!(workspace)))]),
                )
            })?))
        })
    }
    pub fn remove_credential(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace,token,"admin",|db,_|{
            let reference=string(args,"credential_ref",100)?;let identifier=reference.strip_prefix("secret:").ok_or_else(||Error::bad("Expected stored credential reference"))?;
            let projects=crate::store::rows(db,"SELECT id FROM repo_projects WHERE workspace_id=?",[workspace])?.iter().map(|r| s(r,"id").to_string()).collect::<Vec<_>>();
            let (affected, removed) = self.graph().transaction(|gdb| {
                let oauth_refs: Vec<String> = graph::rows(
                    gdb,
                    "MATCH (a:OAuthAccount {workspace_id: $workspace, token_secret: $secret}) RETURN a.id AS id",
                    Params::from([
                        ("workspace".into(), json_to_value(&json!(workspace))),
                        ("secret".into(), json_to_value(&json!(identifier))),
                    ]),
                )?
                .iter()
                .map(|r| format!("oauth:{}", s(r, "id")))
                .collect();
                let affected: Vec<String> = graph::rows(
                    gdb,
                    "MATCH (c:Connector) WHERE c.repository_id IN $projects AND (c.credential_env = $reference OR c.credential_env IN $oauth_refs) RETURN DISTINCT c.repository_id AS repository_id",
                    Params::from([
                        ("projects".into(), json_to_value(&json!(projects))),
                        ("reference".into(), json_to_value(&json!(reference))),
                        ("oauth_refs".into(), json_to_value(&json!(oauth_refs))),
                    ]),
                )?
                .iter()
                .map(|r| s(r, "repository_id").to_string())
                .collect();
                // A removed secret immediately stops every connector referencing it.
                graph::write(
                    gdb,
                    "MATCH (c:Connector) WHERE c.repository_id IN $projects AND (c.credential_env = $reference OR c.credential_env IN $oauth_refs) SET c.state='paused', c.last_sync=NULL, c.generation=c.generation+1",
                    Params::from([
                        ("projects".into(), json_to_value(&json!(projects))),
                        ("reference".into(), json_to_value(&json!(reference))),
                        ("oauth_refs".into(), json_to_value(&json!(oauth_refs))),
                    ]),
                )?;
                let removed = graph::write(
                    gdb,
                    "MATCH (s:Secret {workspace_id: $workspace, id: $id}) DELETE s",
                    Params::from([
                        ("workspace".into(), json_to_value(&json!(workspace))),
                        ("id".into(), json_to_value(&json!(identifier))),
                    ]),
                )?
                .nodes_deleted
                    > 0;
                Ok((affected, removed))
            })?;
            for project in affected{crate::jobs::clear_model_cache(&self.graph(),&project)?;crate::store::invalidate(db,&project)?;}
            Ok(json!({"removed":removed}))
        })
    }
    pub fn resolve_credential(&self, workspace: &str, reference: &str) -> Result<String> {
        if let Some(identifier) = reference.strip_prefix("secret:") {
            return self.secret(workspace, identifier);
        }
        if let Some(identifier) = reference.strip_prefix("oauth:") {
            let account = self.graph().read(|db| {
                graph::one(
                    db,
                    "MATCH (a:OAuthAccount {workspace_id: $workspace, id: $id, state: 'active'}) RETURN a.token_secret AS token_secret, a.refresh_secret AS refresh_secret, a.expires_at AS expires_at",
                    Params::from([
                        ("workspace".into(), json_to_value(&json!(workspace))),
                        ("id".into(), json_to_value(&json!(identifier))),
                    ]),
                )
            })?
            .ok_or_else(|| Error::bad("Connected account is unavailable"))?;
            if account["expires_at"]
                .as_str()
                .is_some_and(|t| t < now().as_str())
            {
                return Err(Error::bad(
                    "Connected account token expired; refresh the account",
                ));
            }
            return self.secret(workspace, s(&account, "token_secret"));
        }
        if reference.is_empty()
            || reference.len() > 100
            || !reference
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            return Err(Error::bad("Invalid credential reference"));
        }
        std::env::var(reference)
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Error::bad("Credential is unavailable; add it in Connections"))
    }
}
