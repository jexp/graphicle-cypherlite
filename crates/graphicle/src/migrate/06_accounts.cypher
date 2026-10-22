-- Domain: account/identity data (am_oauth_accounts, am_provider_identities,
-- am_provider_emails). Plan §2.5 / oauth.rs. am_secrets (the ciphertext
-- vault) is intentionally not migrated: the graph keeps secrets as
-- (:Secret) ciphertext nodes that are workspace-key-encrypted, and a
-- plaintext SQLite vault cannot be re-keyed into the new store — accounts
-- keep their secret-id references but must be reconnected (see
-- docs/migrate-store.md).

-- @csv oauth_accounts.csv
-- @verify MATCH (n:OAuthAccount) RETURN count(n) AS n
LOAD CSV WITH HEADERS FROM $csv AS row
MERGE (a:OAuthAccount {id: row.id})
SET a.workspace_id = row.workspace_id,
    a.provider = row.provider,
    a.external_id = row.external_id,
    a.email = CASE row.email WHEN '' THEN null ELSE row.email END,
    a.display_name = row.display_name,
    a.token_secret = CASE row.token_secret WHEN '' THEN null ELSE row.token_secret END,
    a.refresh_secret = CASE row.refresh_secret WHEN '' THEN null ELSE row.refresh_secret END,
    a.expires_at = CASE row.expires_at WHEN '' THEN null ELSE row.expires_at END,
    a.scopes = row.scopes,
    a.state = row.state,
    a.updated_at = row.updated_at;

-- @csv provider_identities.csv
-- @verify MATCH (n:ProviderIdentity) RETURN count(n) AS n
LOAD CSV WITH HEADERS FROM $csv AS row
MERGE (i:ProviderIdentity {workspace_id: row.workspace_id, provider: row.provider,
                           external_id: row.external_id, credential_id: row.credential_id})
SET i.verified_at = row.verified_at;

-- @csv provider_emails.csv
-- @verify MATCH (n:ProviderEmail) RETURN count(n) AS n
LOAD CSV WITH HEADERS FROM $csv AS row
MERGE (e:ProviderEmail {workspace_id: row.workspace_id, provider: row.provider,
                        external_id: row.external_id})
SET e.email = row.email,
    e.verified_at = row.verified_at;
