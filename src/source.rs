//! Resolve a "side" of the diff into a [`Catalog`].
//!
//! A side can be:
//! - a live database URL (`postgres://` / `postgresql://`),
//! - a saved catalog dump (`*.json`, produced by `dpm dump`),
//! - a declarative schema file (`*.sql`), materialized into a throwaway
//!   database on the shadow server and introspected there (the drizzle-kit
//!   "shadow database" technique, done with real Postgres semantics).

use anyhow::{bail, Context, Result};
use sqlx::Connection;

use crate::introspect::{self, IntrospectOptions};
use crate::model::Catalog;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SideSpec {
    Url(String),
    JsonPath(String),
    SqlPath(String),
}

impl SideSpec {
    pub fn parse(raw: &str) -> Result<Self> {
        let lower = raw.to_ascii_lowercase();
        if lower.starts_with("postgres://") || lower.starts_with("postgresql://") {
            Ok(Self::Url(raw.to_string()))
        } else if lower.ends_with(".json") {
            Ok(Self::JsonPath(raw.to_string()))
        } else if lower.ends_with(".sql") {
            Ok(Self::SqlPath(raw.to_string()))
        } else {
            bail!(
                "cannot interpret {raw:?}: expected a postgres:// URL, a .json catalog dump, \
                 or a .sql schema file"
            )
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Url(u) => introspect::redact_url(u),
            Self::JsonPath(p) => format!("catalog dump {p}"),
            Self::SqlPath(p) => format!("schema file {p}"),
        }
    }

    pub fn is_url(&self) -> bool {
        matches!(self, Self::Url(_))
    }
}

pub struct ResolveContext<'a> {
    pub introspect: &'a IntrospectOptions,
    /// Server URL where throwaway databases may be created (for .sql sides).
    pub shadow_url: Option<String>,
    pub keep_shadow: bool,
    pub verbose: bool,
}

pub async fn resolve(spec: &SideSpec, ctx: &ResolveContext<'_>) -> Result<Catalog> {
    match spec {
        SideSpec::Url(url) => introspect::introspect_url(url, ctx.introspect).await,
        SideSpec::JsonPath(path) => {
            let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
            let cat: Catalog = serde_json::from_str(&text).with_context(|| format!("parsing {path}"))?;
            Ok(cat)
        }
        SideSpec::SqlPath(path) => {
            let sql = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
            let Some(shadow) = &ctx.shadow_url else {
                bail!(
                    "a .sql side needs a shadow server: pass --shadow postgres://... \
                     (or set SHADOW_DATABASE_URL) pointing at a server where dpm may \
                     create and drop throwaway databases"
                );
            };
            let shadow_db = ShadowDb::create(shadow, ctx.verbose).await?;
            let result = async {
                shadow_db.apply_sql(&sql).await.with_context(|| format!("applying {path} to shadow database"))?;
                introspect::introspect_url(&shadow_db.url, ctx.introspect).await
            }
            .await;
            if ctx.keep_shadow {
                eprintln!("dpm: keeping shadow database {}", introspect::redact_url(&shadow_db.url));
                shadow_db.into_kept();
            } else {
                shadow_db.drop_db().await;
            }
            result
        }
    }
}

/// A throwaway database created on the shadow server. Named uniquely per
/// process + counter; dropped explicitly (no Drop impl — async).
pub struct ShadowDb {
    pub url: String,
    admin_url: String,
    pub db_name: String,
    verbose: bool,
}

static SHADOW_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl ShadowDb {
    pub async fn create(shadow_server_url: &str, verbose: bool) -> Result<Self> {
        let n = SHADOW_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let db_name = format!("dpm_shadow_{}_{}", std::process::id(), n);
        let mut admin = sqlx::postgres::PgConnection::connect(shadow_server_url)
            .await
            .with_context(|| {
                format!(
                    "connecting to shadow server {}",
                    introspect::redact_url(shadow_server_url)
                )
            })?;
        sqlx::raw_sql(&format!("CREATE DATABASE {}", crate::model::quote_ident(&db_name)))
            .execute(&mut admin)
            .await
            .context("CREATE DATABASE on shadow server failed (the role needs CREATEDB)")?;
        let _ = admin.close().await;
        let url = replace_database_in_url(shadow_server_url, &db_name)?;
        if verbose {
            eprintln!("dpm: created shadow database {db_name}");
        }
        Ok(Self { url, admin_url: shadow_server_url.to_string(), db_name, verbose })
    }

    /// Apply schema SQL to the shadow database. Tolerates `pg_dump
    /// --schema-only` output: psql meta-commands (`\restrict`, `\connect`,
    /// ...) are stripped, and role-dependent statements (GRANT/REVOKE/OWNER
    /// TO/SET ROLE) are skipped — dpm does not diff ownership or grants and a
    /// fresh shadow database lacks production roles.
    pub async fn apply_sql(&self, sql: &str) -> Result<()> {
        let cleaned = crate::apply::strip_psql_meta_commands(sql);
        let mut conn = sqlx::postgres::PgConnection::connect(&self.url).await?;
        let statements = crate::apply::split_statements(&cleaned);
        let mut skipped = 0usize;
        for (i, stmt) in statements.iter().enumerate() {
            if crate::apply::is_role_dependent_statement(stmt) {
                skipped += 1;
                continue;
            }
            sqlx::raw_sql(stmt).execute(&mut conn).await.with_context(|| {
                format!("statement {} failed:\n{}", i + 1, crate::apply::truncate_sql(stmt))
            })?;
        }
        if skipped > 0 && self.verbose {
            eprintln!("dpm: shadow materialize: skipped {skipped} role-dependent statement(s) (grants/ownership)");
        }
        let _ = conn.close().await;
        Ok(())
    }

    pub async fn drop_db(self) {
        if let Ok(mut admin) = sqlx::postgres::PgConnection::connect(&self.admin_url).await {
            let stmt = format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                crate::model::quote_ident(&self.db_name)
            );
            if sqlx::raw_sql(&stmt).execute(&mut admin).await.is_err() {
                // FORCE needs PG 13+; retry plain.
                let stmt = format!("DROP DATABASE IF EXISTS {}", crate::model::quote_ident(&self.db_name));
                let _ = sqlx::raw_sql(&stmt).execute(&mut admin).await;
            }
            if self.verbose {
                eprintln!("dpm: dropped shadow database {}", self.db_name);
            }
            let _ = admin.close().await;
        }
    }

    pub fn into_kept(self) {}
}

/// Swap the database path segment of a postgres URL, preserving query params.
pub fn replace_database_in_url(url: &str, db: &str) -> Result<String> {
    let (scheme, rest) = url
        .split_once("://")
        .with_context(|| format!("not a URL: {url:?}"))?;
    let (main, query) = match rest.split_once('?') {
        Some((m, q)) => (m, Some(q)),
        None => (rest, None),
    };
    // main = [user[:pass]@]host[:port][/dbname]
    let (authority, _old_db) = match main.rfind('/') {
        Some(pos) => (&main[..pos], &main[pos + 1..]),
        None => (main, ""),
    };
    let mut out = format!("{scheme}://{authority}/{db}");
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_spec_parsing() {
        assert!(matches!(SideSpec::parse("postgres://u@h/db").unwrap(), SideSpec::Url(_)));
        assert!(matches!(SideSpec::parse("POSTGRESQL://u@h/db").unwrap(), SideSpec::Url(_)));
        assert!(matches!(SideSpec::parse("dump.json").unwrap(), SideSpec::JsonPath(_)));
        assert!(matches!(SideSpec::parse("schema/schema.sql").unwrap(), SideSpec::SqlPath(_)));
        assert!(SideSpec::parse("whatever.txt").is_err());
    }

    #[test]
    fn url_db_replacement_preserves_authority_and_query() {
        assert_eq!(
            replace_database_in_url("postgres://u:p@h:5432/postgres?sslmode=disable", "shadow1").unwrap(),
            "postgres://u:p@h:5432/shadow1?sslmode=disable"
        );
        assert_eq!(
            replace_database_in_url("postgres://u@h", "shadow1").unwrap(),
            "postgres://u@h/shadow1"
        );
    }
}
