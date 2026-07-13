//! Verification: prove a generated migration actually converges, without
//! touching the real target.
//!
//! 1. Introspect source and target.
//! 2. Create a throwaway database on the shadow server and replay the
//!    *target's* schema into it (bootstrap script = diff(empty → target)).
//! 3. Generate the migration (diff(target → source)) and apply it to the
//!    replica.
//! 4. Re-introspect the replica and re-diff against the source: an empty
//!    plan proves convergence.
//! 5. Optionally run an external cross-checker (migra, pgdiff, an AI review
//!    script, ...) via a command template: `{source}` / `{target}` expand to
//!    the source URL and the migrated-replica URL. Empty stdout + exit 0 is
//!    agreement.
//!
//! The real target is only ever read.

use anyhow::{bail, Context, Result};

use crate::diff::diff;
use crate::emit::{emit, EmitOptions};
use crate::introspect::{self, IntrospectOptions};
use crate::model::Catalog;
use crate::source::ShadowDb;

pub struct VerifyOutcome {
    pub migration_sql: String,
    pub converged: bool,
    /// Residual change count after applying the migration to the replica.
    pub residual_changes: usize,
    pub residual_sql: Option<String>,
    /// (command, agreed, stdout) for the external cross-check, when run.
    pub external: Option<(String, bool, String)>,
}

pub struct VerifyParams<'a> {
    pub source: &'a Catalog,
    pub target: &'a Catalog,
    pub shadow_server_url: &'a str,
    pub source_url_for_external: Option<&'a str>,
    pub allow_destructive: bool,
    pub external_check: Option<&'a str>,
    pub keep_shadow: bool,
    pub verbose: bool,
    pub introspect: &'a IntrospectOptions,
}

pub async fn verify(p: VerifyParams<'_>) -> Result<VerifyOutcome> {
    // The migration under test.
    let plan = diff(p.source, p.target);
    let script = emit(
        &plan,
        &EmitOptions { allow_destructive: p.allow_destructive, source_desc: None, target_desc: None },
    );

    // Replica of the target on the shadow server.
    let replica = ShadowDb::create(p.shadow_server_url, p.verbose).await?;
    let outcome = run_on_replica(&p, &script.sql, &replica).await;
    if p.keep_shadow {
        eprintln!(
            "dpm: keeping verify replica {}",
            introspect::redact_url(&replica.url)
        );
        replica.into_kept();
    } else {
        replica.drop_db().await;
    }
    outcome
}

async fn run_on_replica(p: &VerifyParams<'_>, migration_sql: &str, replica: &ShadowDb) -> Result<VerifyOutcome> {
    // Bootstrap the replica to match the target: diff(target → truly empty)
    // with destructive allowed (there is nothing to destroy in an empty db).
    // The empty catalog has NO schemas so CREATE SCHEMA statements are
    // emitted for everything the target uses.
    let empty = Catalog::default();
    let bootstrap_plan = diff(p.target, &empty);
    let bootstrap = emit(
        &bootstrap_plan,
        &EmitOptions { allow_destructive: true, source_desc: None, target_desc: None },
    );
    crate::apply::apply_script(&replica.url, &bootstrap.sql)
        .await
        .context("bootstrapping the target replica on the shadow server failed")?;

    // Sanity: the replica must introspect identical to the target, otherwise
    // dpm's own bootstrap emission is lossy for this schema and the verify
    // result would be meaningless.
    let replica_cat = introspect::introspect_url(&replica.url, p.introspect).await?;
    let replica_drift = diff(p.target, &replica_cat);
    if !replica_drift.is_empty() {
        let drift = emit(&replica_drift, &EmitOptions::default());
        bail!(
            "shadow replica does not faithfully reproduce the target ({} residual changes). \
             This is a dpm coverage gap — the verify result would be meaningless.\n{}",
            replica_drift.changes.len(),
            drift.sql
        );
    }

    // Apply the migration under test.
    crate::apply::apply_script(&replica.url, migration_sql)
        .await
        .context("applying the generated migration to the replica failed")?;

    // Re-diff.
    let migrated = introspect::introspect_url(&replica.url, p.introspect).await?;
    let residual = diff(p.source, &migrated);
    let converged = residual.is_empty();
    let residual_sql = if converged {
        None
    } else {
        Some(emit(&residual, &EmitOptions::default()).sql)
    };

    // Optional external cross-check.
    let external = match (p.external_check, p.source_url_for_external) {
        (Some(template), Some(source_url)) => {
            let cmd = template.replace("{source}", source_url).replace("{target}", &replica.url);
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .output()
                .with_context(|| format!("running external check: {cmd}"))?;
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let agreed = output.status.success() && stdout.is_empty();
            Some((cmd, agreed, stdout))
        }
        (Some(_), None) => {
            eprintln!("dpm: skipping external check (source is not a live URL)");
            None
        }
        _ => None,
    };

    Ok(VerifyOutcome {
        migration_sql: migration_sql.to_string(),
        converged,
        residual_changes: residual.changes.len(),
        residual_sql,
        external,
    })
}
