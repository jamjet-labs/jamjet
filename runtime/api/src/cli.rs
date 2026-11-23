//! `jamjet-server`'s command line: no arguments serves; `token create` issues
//! an API token, `token revoke` revokes one, and `migrate` brings a Postgres
//! schema up to this build.
//!
//! Before this there was no way to issue a token: `create_token` had no
//! caller outside tests, so a server with authentication on refused every
//! request until someone wrote a row into `api_tokens` by hand.

use crate::config::ApiConfig;
use crate::startup::{shown_url, store_kind, StoreKind};
use crate::store::TokenStore;
use jamjet_audit::{ActorType, AuditBackend, AuditEnricher, AuditQuery, AuditRecord, AuditSigner};
use jamjet_state::postgres::{MigrateError, SchemaState};
use jamjet_state::{ApiToken, DEFAULT_TENANT};

pub const USAGE: &str = "\
Usage:
  jamjet-server                      start the runtime (configured by environment variables)
  jamjet-server token create --role <role> [--tenant <id>] [--name <name>]
                                     issue an API token and print it once
  jamjet-server token revoke <token id>
                                     revoke a token (its id is printed when it is issued)
  jamjet-server migrate [--check]    bring a Postgres schema up to this build, or with
                                     --check only report where it stands
  jamjet-server help                 show this message

Roles: operator, developer, reviewer, viewer.

The token commands open the database the server uses: DATABASE_URL, or
.jamjet/runtime.db in the working directory. It must hold the server's
schema: start the server against it once first, or on Postgres run
`jamjet-server migrate`. Only the token's SHA-256 hash is stored; the
token itself is printed once, on stdout. Each issue and revocation is sealed
into the audit log (token_issued, token_revoked), so run them with the audit
signing settings the server uses: they refuse when the log's newest entry is
signed and this environment cannot sign the same way.

`migrate` reads DATABASE_URL as the token commands do. On Postgres it applies
the pending migrations and exits 0; it exits 3 when the database is newer than
this build and 4 when a migration is half done or changed. `--check` changes
nothing and exits 0 current, 2 behind, 3 newer, 4 half done or changed. On
SQLite it migrates the file as the server does when it opens it.";

/// `migrate --check`: the schema is behind this build (or empty).
pub const MIGRATE_BEHIND: i32 = 2;
/// `migrate`: the schema is newer than this build.
pub const MIGRATE_AHEAD: i32 = 3;
/// `migrate`: a migration is half done, or differs from this build's.
pub const MIGRATE_DIRTY: i32 = 4;

/// The roles a token may have, as `api_tokens.role` stores them.
pub const ROLES: &[&str] = &["operator", "developer", "reviewer", "viewer"];

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Serve,
    Help,
    TokenCreate(TokenRequest),
    TokenRevoke(String),
    /// `migrate`, or with `check` only report where the schema stands.
    Migrate {
        check: bool,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub struct TokenRequest {
    pub role: String,
    pub tenant: String,
    pub name: String,
}

/// Parse the arguments after the program name.
///
/// No arguments serves, as it always has. Anything unrecognised is an error
/// rather than ignored: a mistyped subcommand must not start a server.
pub fn parse_args(args: &[String]) -> Result<Command, String> {
    let mut it = args.iter().map(String::as_str);
    match it.next() {
        None => Ok(Command::Serve),
        Some("help" | "-h" | "--help") => Ok(Command::Help),
        Some("token") => match it.next() {
            Some("create") => parse_token_create(it.collect()),
            Some("revoke") => match (it.next(), it.next()) {
                (Some(id), None) if !id.trim().is_empty() && !id.starts_with('-') => {
                    Ok(Command::TokenRevoke(id.to_string()))
                }
                _ => Err("`token revoke` takes one token id".into()),
            },
            Some(other) => Err(format!("unknown token command {other:?}")),
            None => Err("missing token command (`token create` or `token revoke`)".into()),
        },
        Some("migrate") => match (it.next(), it.next()) {
            (None, _) => Ok(Command::Migrate { check: false }),
            (Some("--check"), None) => Ok(Command::Migrate { check: true }),
            _ => Err("`migrate` takes no argument but `--check`".into()),
        },
        Some(other) => Err(format!("unknown argument {other:?}")),
    }
}

fn parse_token_create(args: Vec<&str>) -> Result<Command, String> {
    let mut role = None;
    let mut tenant = None;
    let mut name = None;
    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        let slot = match flag {
            "--role" => &mut role,
            "--tenant" => &mut tenant,
            "--name" => &mut name,
            other => return Err(format!("unknown option {other:?} for `token create`")),
        };
        let value = it
            .next()
            .filter(|v| !v.starts_with("--"))
            .ok_or_else(|| format!("{flag} needs a value"))?;
        if slot.replace(value.to_string()).is_some() {
            return Err(format!("{flag} given twice"));
        }
    }
    let role = role.ok_or("--role is required")?;
    if !ROLES.contains(&role.as_str()) {
        return Err(format!(
            "unknown role {role:?}; the roles are {}",
            ROLES.join(", ")
        ));
    }
    let tenant = tenant.unwrap_or_else(|| DEFAULT_TENANT.to_string());
    if tenant.trim().is_empty() {
        return Err("--tenant must not be empty".into());
    }
    let name = name.unwrap_or_else(|| format!("{role}-cli"));
    if name.trim().is_empty() {
        return Err("--name must not be empty".into());
    }
    // As for every identifier the engine records: the name and tenant reach
    // the audit log, and an invisible character makes a name read as another.
    for (flag, value) in [("--tenant", &tenant), ("--name", &name)] {
        if value.chars().any(jamjet_ir::is_forbidden_identifier_char) {
            return Err(format!(
                "{flag} must not contain control characters or invisible format characters"
            ));
        }
    }
    Ok(Command::TokenCreate(TokenRequest { role, tenant, name }))
}

/// Issue a token into the database at `database_url`. Returns the token text
/// (shown once, never stored) and what was stored.
pub async fn create_token(
    database_url: &str,
    req: &TokenRequest,
) -> anyhow::Result<(String, ApiToken)> {
    TokenStore::open(database_url)
        .await?
        .create_token(req)
        .await
}

/// The audit log of `store`, signed as the server's environment says
/// (`AuditSigner::from_env`). Refused when the log's newest entry is signed
/// and this environment cannot sign the same way ([`refuse_another_key`]).
async fn audit_log(store: &TokenStore) -> anyhow::Result<AuditEnricher> {
    // The one switch these commands read, read strictly: a typo is an error,
    // not "off". Here, not before the command runs, so a revoke revokes
    // first whatever the switch says.
    crate::startup::env_flag(jamjet_audit::signer::ALLOW_INSECURE_KEY_ENV)?;
    let backend = store.audit().await?;
    let audit = AuditEnricher::new(backend.clone());
    refuse_another_key(backend.as_ref(), audit.signer()).await?;
    if !audit.signer().can_sign() {
        eprintln!(
            "warning: no audit signing key in this environment, so the audit entry is \
             left unsigned; run with the key the server uses"
        );
    }
    Ok(audit)
}

/// Refuse to append to a log whose newest entry is signed when `signer`
/// cannot sign as it was signed: with no key, another HMAC key, or another
/// Ed25519 key. The entry would sit in a signed chain unsigned, or signed
/// with a key the server's verifier rejects, so the log would no longer
/// verify from that entry on. A log whose newest entry is unsigned has no
/// key to keep to.
///
/// After a key rotation the server seals [`seal_key_change`] at its next
/// start, so the newest entry is the new key's and these commands work again
/// under it.
pub async fn refuse_another_key(
    backend: &dyn AuditBackend,
    signer: &AuditSigner,
) -> anyhow::Result<()> {
    let Some((signed_with, here)) = key_mismatch(backend, signer).await? else {
        return Ok(());
    };
    anyhow::bail!(
        "the audit log's newest entry is signed with {signed_with}, and this environment has \
         {here}, so the entry for this command would break the log's signatures. Run it with \
         the audit signing settings the server uses ({}, {} or {}/{}). After a key change, \
         start the server once with the new key: it records the change, and the commands \
         work under the new key from then on.",
        jamjet_audit::signer::SIGNING_MODE_ENV,
        jamjet_audit::signer::SIGNING_KEY_ENV,
        jamjet_audit::signer::ED25519_KEY_ENV,
        jamjet_audit::signer::ED25519_KEY_FILE_ENV,
    )
}

/// When the log's newest entry is signed and `signer` cannot verify it: what
/// it was signed with, and what `signer` has. `None` when they agree, the
/// newest entry is unsigned, or the log is empty.
async fn key_mismatch(
    backend: &dyn AuditBackend,
    signer: &AuditSigner,
) -> anyhow::Result<Option<(String, String)>> {
    let newest = backend
        .query(&AuditQuery {
            limit: 1,
            ..Default::default()
        })
        .await
        .map_err(|e| anyhow::anyhow!("cannot read the audit log's newest entry: {e}"))?;
    let Some(head) = newest.into_iter().next() else {
        return Ok(None);
    };
    let (Some(hash), Some(signature)) = (head.entry_hash.as_deref(), head.signature.as_deref())
    else {
        return Ok(None);
    };
    if signer.can_sign() && signer.verify(hash.as_bytes(), signature) {
        return Ok(None);
    }
    let signed_with = match signature
        .strip_prefix(jamjet_audit::signer::ED25519_SIG_PREFIX)
        .and_then(|rest| rest.split_once(':'))
    {
        Some((kid, _)) => format!("Ed25519 key {kid}"),
        None => "an HMAC key".to_string(),
    };
    let here = match (signer.can_sign(), signer.kid()) {
        (false, _) => "no signing key".to_string(),
        (true, Some(kid)) => format!("Ed25519 key {kid}"),
        (true, None) => "another HMAC key".to_string(),
    };
    Ok(Some((signed_with, here)))
}

/// At server start: when the log's newest entry is signed and the server's
/// key cannot verify it (the key was rotated, or the mode changed), seal an
/// `audit_key_changed` record under the new key, naming both. Without it a
/// rotation locked the token commands out for good: they refuse a log whose
/// newest entry they cannot verify, the newest entry was the old key's, and
/// with every token revoked nothing else made the server write a new one.
///
/// Returns whether a record was sealed. The record is on the chain like any
/// entry, so a verifier sees where one key's entries end and the next's begin.
pub async fn seal_key_change(
    backend: &dyn AuditBackend,
    audit: &AuditEnricher,
) -> anyhow::Result<bool> {
    let Some((previous, current)) = key_mismatch(backend, audit.signer()).await? else {
        return Ok(false);
    };
    let current = match current.as_str() {
        "another HMAC key" => "an HMAC key".to_string(),
        other => other.to_string(),
    };
    audit
        .seal_record(AuditRecord {
            event_type: "audit_key_changed".to_string(),
            actor_id: "system".to_string(),
            actor_type: ActorType::System,
            tenant_id: DEFAULT_TENANT.to_string(),
            detail: serde_json::json!({
                "type": "audit_key_changed",
                "previous": previous,
                "current": current,
            }),
        })
        .await
        .map_err(|e| anyhow::anyhow!("the audit key change could not be recorded: {e}"))?;
    Ok(true)
}

/// The audit record of a token issued or revoked. Separation of duties is
/// keyed on the token id (`token:<id>`), so who holds which token must be on
/// record, a reviewer token minted for oneself included. The command runs
/// with database access, not a token, so the actor is `cli`.
pub fn token_record(
    event_type: &str,
    id: &str,
    tenant: &str,
    role: &str,
    name: &str,
) -> AuditRecord {
    AuditRecord {
        event_type: event_type.to_string(),
        actor_id: "cli".to_string(),
        actor_type: ActorType::Human,
        tenant_id: tenant.to_string(),
        detail: serde_json::json!({
            "type": event_type,
            "token_id": id,
            "principal": format!("token:{id}"),
            "role": role,
            "tenant_id": tenant,
            "name": name,
        }),
    }
}

/// Issue a token and seal `token_issued` into the audit log. A token the log
/// cannot record is revoked again and the command fails: no token is handed
/// out off the record.
pub async fn issue_token(
    database_url: &str,
    req: &TokenRequest,
) -> anyhow::Result<(String, ApiToken)> {
    // The database first: its refusal says how to make one.
    let store = TokenStore::open(database_url).await?;
    let audit = audit_log(&store).await?;
    let (token, info) = store.create_token(req).await?;
    let record = token_record(
        "token_issued",
        &info.id,
        &info.tenant_id,
        &info.role,
        &info.name,
    );
    if let Err(e) = audit.seal_record(record).await {
        let undone = store.revoke_token(&info.id).await;
        anyhow::bail!(
            "the audit log could not record the token, so it was not issued ({e}){}",
            match undone {
                Ok(Some(_)) => String::new(),
                _ => format!(
                    "; revoking it failed too, revoke {} with `jamjet-server token revoke {}`",
                    info.id, info.id
                ),
            }
        );
    }
    Ok((token, info))
}

/// `jamjet-server token create`: issue the token and print it on stdout, and
/// everything else on stderr, so `TOKEN=$(jamjet-server token create ...)`
/// captures only the token.
pub async fn run_token_create(req: &TokenRequest) -> anyhow::Result<()> {
    let database_url = token_database()?;
    let (token, info) = issue_token(&database_url, req).await?;
    // The id first: if stdout is closed, the operator still learns which
    // token to revoke.
    eprintln!(
        "Created token {} in {} (role {}, tenant {}, name {:?}). It is \
         shown once; only its SHA-256 hash is stored.",
        info.id,
        shown_url(&database_url),
        info.role,
        info.tenant_id,
        info.name
    );
    {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        writeln!(out, "{token}")
            .and_then(|_| out.flush())
            .map_err(|e| {
                anyhow::anyhow!(
                    "could not print the token ({e}); revoke {} with \
                     `jamjet-server token revoke {}`",
                    info.id,
                    info.id
                )
            })?;
    }
    if info.tenant_id != DEFAULT_TENANT {
        eprintln!(
            "warning: the runtime is single-tenant and refuses tokens of any tenant but \
             {DEFAULT_TENANT} unless {}=1 is set, and then they act in {DEFAULT_TENANT}.",
            crate::auth::TENANT_TOKENS_AS_DEFAULT_ENV
        );
    }
    Ok(())
}

/// The database the token commands use: the server's, refused for the
/// memory backend, whose tokens live inside the server process.
fn token_database() -> anyhow::Result<String> {
    if std::env::var("STORAGE_BACKEND").as_deref() == Ok("memory") {
        anyhow::bail!(
            "STORAGE_BACKEND=memory keeps tokens inside the server process, so no \
             other process can add one. Use SQLite or Postgres (unset STORAGE_BACKEND) \
             to use tokens."
        );
    }
    // Not `database_url`: that creates `.jamjet`, and a command run in the
    // wrong directory must not leave one behind.
    Ok(crate::startup::existing_database_url(&ApiConfig::default()))
}

/// Revoke a token and seal `token_revoked` into the audit log. The token is
/// revoked whether or not the log records it (revoking is the safe side);
/// a record that fails is reported as an error.
pub async fn revoke_token(database_url: &str, id: &str) -> anyhow::Result<()> {
    let store = TokenStore::open(database_url).await?;
    // Revoke first: a log that cannot be opened, or is signed with another
    // key, must not keep a token alive.
    let Some(revoked) = store.revoke_token(id).await? else {
        anyhow::bail!("no active token {id:?} in {}", shown_url(database_url));
    };
    let audit = audit_log(&store).await.map_err(|e| {
        anyhow::anyhow!("token {id} is revoked, but the audit log could not record it: {e}")
    })?;
    let record = token_record(
        "token_revoked",
        id,
        &revoked.tenant_id,
        &revoked.role,
        &revoked.name,
    );
    audit.seal_record(record).await.map_err(|e| {
        anyhow::anyhow!("token {id} is revoked, but the audit log could not record it: {e}")
    })
}

/// `jamjet-server token revoke <id>`.
pub async fn run_token_revoke(id: &str) -> anyhow::Result<()> {
    let database_url = token_database()?;
    revoke_token(&database_url, id).await?;
    eprintln!("Revoked token {id}. It is refused from the next request on.");
    Ok(())
}

/// `jamjet-server migrate [--check]`: the exit code, after saying on stdout
/// where the schema stands, or on stderr why it cannot go on. Never prints
/// the URL's password.
pub async fn run_migrate(check: bool) -> i32 {
    match migrate_command(check).await {
        Ok((code, said)) => {
            if code == 0 {
                println!("{said}");
            } else {
                eprintln!("jamjet-server migrate: {said}");
            }
            code
        }
        Err(e) => {
            eprintln!("jamjet-server migrate: {e}");
            1
        }
    }
}

async fn migrate_command(check: bool) -> anyhow::Result<(i32, String)> {
    let config = ApiConfig::default();
    let storage = std::env::var("STORAGE_BACKEND").ok();
    let url = crate::startup::existing_database_url(&config);
    match store_kind(storage.as_deref(), &url) {
        StoreKind::Memory => {
            anyhow::bail!("STORAGE_BACKEND=memory keeps no schema; there is nothing to migrate")
        }
        StoreKind::Postgres if check => {
            let state = jamjet_state::postgres::schema_state_at(&url)
                .await
                .map_err(anyhow::Error::msg)?;
            Ok(check_answer(&state))
        }
        StoreKind::Postgres => match jamjet_state::postgres::migrate(&url).await {
            Ok(report) => Ok((
                0,
                format!(
                    "schema at version {} ({} applied)",
                    report.version, report.applied
                ),
            )),
            Err(MigrateError::Refused(state)) => {
                let (code, why) = check_answer(&state);
                Ok((code, why))
            }
            Err(MigrateError::Failed(why)) => anyhow::bail!(why),
        },
        StoreKind::Sqlite if check => anyhow::bail!(
            "`migrate --check` reads a Postgres schema; SQLite migrates itself when the \
             server opens it"
        ),
        StoreKind::Sqlite => {
            // As the server does at startup: the state schema, then the
            // audit log's, in the file the server would open.
            let url = crate::startup::database_url(&config);
            let backend = jamjet_state::SqliteBackend::open(&url)
                .await
                .map_err(|e| anyhow::anyhow!("failed to open {url}: {e}"))?;
            jamjet_audit::SqliteAuditBackend::new(backend.pool())
                .migrate()
                .await
                .map_err(|e| anyhow::anyhow!("failed to migrate the audit log in {url}: {e}"))?;
            Ok((0, format!("SQLite schema in {url} is current")))
        }
    }
}

/// The exit code and message for a Postgres schema in `state`, as
/// `migrate --check` answers it.
fn check_answer(state: &SchemaState) -> (i32, String) {
    let code = match state {
        SchemaState::Current { .. } => 0,
        SchemaState::Empty | SchemaState::Behind { .. } => MIGRATE_BEHIND,
        SchemaState::Ahead { .. } => MIGRATE_AHEAD,
        SchemaState::Dirty { .. } | SchemaState::Changed { .. } => MIGRATE_DIRTY,
    };
    let said = match (state, state.refusal()) {
        (SchemaState::Current { version }, _) => format!("schema at version {version} (current)"),
        (_, Some(why)) => why,
        (_, None) => format!("{state:?}"),
    };
    (code, said)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_takes_only_check() {
        assert_eq!(
            parse_args(&args("migrate")),
            Ok(Command::Migrate { check: false })
        );
        assert_eq!(
            parse_args(&args("migrate --check")),
            Ok(Command::Migrate { check: true })
        );
        for bad in ["migrate --force", "migrate --check --check", "migrate up"] {
            assert!(parse_args(&args(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn check_codes_follow_the_schema_state() {
        let code = |s: SchemaState| check_answer(&s).0;
        assert_eq!(code(SchemaState::Current { version: 2 }), 0);
        assert_eq!(code(SchemaState::Empty), MIGRATE_BEHIND);
        assert_eq!(
            code(SchemaState::Behind { at: 1, needs: 2 }),
            MIGRATE_BEHIND
        );
        assert_eq!(code(SchemaState::Ahead { at: 9, knows: 2 }), MIGRATE_AHEAD);
        assert_eq!(code(SchemaState::Dirty { version: 2 }), MIGRATE_DIRTY);
        assert_eq!(code(SchemaState::Changed { version: 1 }), MIGRATE_DIRTY);
        assert_eq!(
            (MIGRATE_BEHIND, MIGRATE_AHEAD, MIGRATE_DIRTY),
            (2, 3, 4),
            "the codes the guide documents"
        );
    }

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn no_arguments_serves() {
        assert_eq!(parse_args(&[]), Ok(Command::Serve));
    }

    #[test]
    fn token_create_reads_role_tenant_and_name() {
        assert_eq!(
            parse_args(&args(
                "token create --role reviewer --tenant acme --name ci"
            )),
            Ok(Command::TokenCreate(TokenRequest {
                role: "reviewer".into(),
                tenant: "acme".into(),
                name: "ci".into(),
            }))
        );
        assert_eq!(
            parse_args(&args("token create --role developer")),
            Ok(Command::TokenCreate(TokenRequest {
                role: "developer".into(),
                tenant: "default".into(),
                name: "developer-cli".into(),
            }))
        );
    }

    /// `Role::parse_role` reads an unknown role as `viewer`; the command must
    /// refuse it instead of storing a role nothing grants.
    #[test]
    fn an_unknown_role_is_refused() {
        assert!(parse_args(&args("token create --role admin")).is_err());
        assert!(parse_args(&args("token create")).is_err());
    }

    /// As for every identifier the engine records: the name reaches the
    /// audit log, and an invisible character makes one name read as another.
    #[test]
    fn a_name_or_tenant_with_a_control_or_format_character_is_refused() {
        for bad in ["ci\u{1f}x", "ci\u{202e}gnp", "ci\u{200b}", "a\nb"] {
            let name = vec![
                "token".to_string(),
                "create".into(),
                "--role".into(),
                "viewer".into(),
                "--name".into(),
                bad.to_string(),
            ];
            let err = parse_args(&name).unwrap_err();
            assert!(err.contains("--name"), "{bad:?}: {err}");
            let mut tenant = name.clone();
            tenant[4] = "--tenant".into();
            let err = parse_args(&tenant).unwrap_err();
            assert!(err.contains("--tenant"), "{bad:?}: {err}");
        }
        assert!(parse_args(&args("token create --role viewer --name café-ci")).is_ok());
    }

    #[test]
    fn a_mistyped_command_does_not_serve() {
        for bad in [
            "--dev",
            "serve",
            "token",
            "token make --role viewer",
            "token revoke",
            "token revoke a b",
            "token revoke --role",
            "token create --role",
            "token create --role viewer --role operator",
            "token create --role viewer --scope x",
        ] {
            assert!(parse_args(&args(bad)).is_err(), "{bad}");
        }
    }
}
