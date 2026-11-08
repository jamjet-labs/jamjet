//! `jamjet-server`'s command line: no arguments serves; `token create` issues
//! an API token and `token revoke` revokes one.
//!
//! Before this there was no way to issue a token: `create_token` had no
//! caller outside tests, so a server with authentication on refused every
//! request until someone wrote a row into `api_tokens` by hand.

use crate::config::ApiConfig;
use jamjet_state::{ApiToken, SqliteBackend, StateBackend, TenantId, DEFAULT_TENANT};

pub const USAGE: &str = "\
Usage:
  jamjet-server                      start the runtime (configured by environment variables)
  jamjet-server token create --role <role> [--tenant <id>] [--name <name>]
                                     issue an API token and print it once
  jamjet-server token revoke <token id>
                                     revoke a token (its id is printed when it is issued)
  jamjet-server help                 show this message

Roles: operator, developer, reviewer, viewer.

The token commands open the database the server uses: DATABASE_URL, or
.jamjet/runtime.db in the working directory. It must exist, so start the
server against it once first. Only the token's SHA-256 hash is stored; the
token itself is printed once, on stdout.";

/// The roles a token may have, as `api_tokens.role` stores them.
pub const ROLES: &[&str] = &["operator", "developer", "reviewer", "viewer"];

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Serve,
    Help,
    TokenCreate(TokenRequest),
    TokenRevoke(String),
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
    Ok(Command::TokenCreate(TokenRequest { role, tenant, name }))
}

/// Issue a token into the database at `database_url`. Returns the token text
/// (shown once, never stored) and what was stored.
pub async fn create_token(
    database_url: &str,
    req: &TokenRequest,
) -> anyhow::Result<(String, ApiToken)> {
    let backend = SqliteBackend::connect_existing(database_url)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    if req.tenant == DEFAULT_TENANT {
        return backend
            .create_token(&req.name, &req.role)
            .await
            .map_err(|e| anyhow::anyhow!("failed to create the token: {e}"));
    }
    let tenant = TenantId::from(req.tenant.clone());
    if backend
        .get_tenant(&tenant)
        .await
        .map_err(|e| anyhow::anyhow!("failed to read tenant {:?}: {e}", req.tenant))?
        .is_none()
    {
        anyhow::bail!(
            "no tenant {:?}; create it first (POST /tenants, operator role)",
            req.tenant
        );
    }
    backend
        .for_tenant(tenant)
        .create_token(&req.name, &req.role)
        .await
        .map_err(|e| anyhow::anyhow!("failed to create the token: {e}"))
}

/// `jamjet-server token create`: issue the token and print it on stdout, and
/// everything else on stderr, so `TOKEN=$(jamjet-server token create ...)`
/// captures only the token.
pub async fn run_token_create(req: &TokenRequest) -> anyhow::Result<()> {
    let database_url = token_database()?;
    let (token, info) = create_token(&database_url, req).await?;
    // The id first: if stdout is closed, the operator still learns which
    // token to revoke.
    eprintln!(
        "Created token {} in {database_url} (role {}, tenant {}, name {:?}). It is \
         shown once; only its SHA-256 hash is stored.",
        info.id, info.role, info.tenant_id, info.name
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
             other process can add one. Use SQLite (unset STORAGE_BACKEND) to use tokens."
        );
    }
    Ok(crate::startup::database_url(&ApiConfig::default()))
}

/// `jamjet-server token revoke <id>`.
pub async fn run_token_revoke(id: &str) -> anyhow::Result<()> {
    let database_url = token_database()?;
    let backend = SqliteBackend::connect_existing(&database_url)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    if !backend
        .revoke_token(id)
        .await
        .map_err(|e| anyhow::anyhow!("failed to revoke the token: {e}"))?
    {
        anyhow::bail!("no active token {id:?} in {database_url}");
    }
    eprintln!("Revoked token {id}. It is refused from the next request on.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
