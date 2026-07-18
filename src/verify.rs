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
//! 5. Optionally cross-check with independent diff engines:
//!    - migra / pgdiff first-party drivers (`--cross-check-with-migra`,
//!      `--cross-check-with-pgdiff`),
//!    - any custom command template (`--external-check 'cmd {target} {source}'`,
//!      empty stdout + exit 0 = agreement). When the source is a .sql file or
//!      catalog dump (no live URL), a second throwaway "source replica" is
//!      materialized so the external tools still have two live databases.
//!
//! The real target is only ever read.

use anyhow::{bail, Context, Result};

use crate::crosscheck::{self, CheckReport};
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
    /// External / cross-check tool reports (migra, pgdiff, custom).
    pub checks: Vec<CheckReport>,
}

impl VerifyOutcome {
    pub fn all_checks_agreed(&self) -> bool {
        self.checks.iter().all(|c| c.agreed)
    }
}

pub struct VerifyParams<'a> {
    pub source: &'a Catalog,
    pub target: &'a Catalog,
    pub shadow_server_url: &'a str,
    /// Live URL of the source when it is a database (used directly by
    /// external tools); when None a source replica is materialized on demand.
    pub source_url_for_external: Option<&'a str>,
    pub allow_destructive: bool,
    /// Custom cross-check command template ({source}/{target} placeholders).
    pub external_check: Option<&'a str>,
    /// Which of the seven first-party cross-checkers to run.
    pub checks: crosscheck::CheckSelection,
    pub bins: crosscheck::Bins,
    pub keep_shadow: bool,
    pub verbose: bool,
    pub introspect: &'a IntrospectOptions,
}

pub async fn verify(p: VerifyParams<'_>) -> Result<VerifyOutcome> {
    if p.source.database_flavor != p.target.database_flavor {
        bail!(
            "cannot verify a {} source against a {} target",
            p.source.database_flavor.label(),
            p.target.database_flavor.label()
        );
    }
    // Canonicalize both reference catalogs so every comparison below is
    // symmetric with the replicas (which are re-introspected from dpm's own
    // emitted SQL and canonicalized before diffing). This makes verify()
    // correct regardless of whether the caller pre-canonicalized — the CLI
    // path does via load_sides; other callers may not — and is a no-op on
    // already-canonical input (one round-trip is the fixed point).
    let mut source = p.source.clone();
    let mut target = p.target.clone();
    crate::canonicalize::canonicalize_defs(
        &mut [&mut source, &mut target],
        p.shadow_server_url,
        p.verbose,
    )
    .await?;
    let p = VerifyParams { source: &source, target: &target, ..p };

    // The migration under test.
    let plan = diff(p.source, p.target);
    let script = emit(
        &plan,
        &EmitOptions {
            allow_destructive: p.allow_destructive,
            database_flavor: p.source.database_flavor,
            source_desc: None,
            target_desc: None,
        },
    );

    // Replica of the target on the shadow server.
    let replica = ShadowDb::create(p.shadow_server_url, p.verbose).await?;
    if replica.database_flavor() != p.source.database_flavor {
        let actual = replica.database_flavor();
        replica.drop_db().await;
        bail!(
            "cannot verify a {} migration on a {} shadow server",
            p.source.database_flavor.label(),
            actual.label()
        );
    }
    let outcome = run_on_replica(&p, &script.sql, &replica).await;
    if p.keep_shadow {
        eprintln!("dpm: keeping verify replica {}", introspect::redact_url(&replica.url));
        replica.into_kept();
    } else {
        replica.drop_db().await;
    }
    outcome
}

/// Materialize a catalog into a fresh shadow database (bootstrap DDL) and
/// sanity-check the result reproduces the catalog exactly.
pub async fn materialize_catalog(
    label: &str,
    cat: &Catalog,
    shadow_server_url: &str,
    opts: &IntrospectOptions,
    verbose: bool,
) -> Result<ShadowDb> {
    let db = ShadowDb::create(shadow_server_url, verbose).await?;
    if db.database_flavor() != cat.database_flavor {
        let actual = db.database_flavor();
        db.drop_db().await;
        bail!(
            "cannot materialize a {} catalog on a {} shadow server",
            cat.database_flavor.label(),
            actual.label()
        );
    }
    let bootstrap_plan = diff(cat, &Catalog::default());
    let bootstrap = emit(
        &bootstrap_plan,
        &EmitOptions {
            allow_destructive: true,
            database_flavor: cat.database_flavor,
            source_desc: None,
            target_desc: None,
        },
    );
    let applied = crate::apply::apply_script(&db.url, &bootstrap.sql).await;
    if let Err(e) = applied {
        db.drop_db().await;
        return Err(e).with_context(|| format!("bootstrapping the {label} replica on the shadow server failed"));
    }
    let mut replica_cat = match introspect::introspect_url(&db.url, opts).await {
        Ok(c) => c,
        Err(e) => {
            db.drop_db().await;
            return Err(e);
        }
    };
    // The replica was built from dpm's own emitted deparse, so its CHECK and
    // index defs are the re-parse fixed point; canonicalize before comparing
    // against the (already canonicalized) catalog.
    if let Err(e) =
        crate::canonicalize::canonicalize_defs(&mut [&mut replica_cat], shadow_server_url, verbose).await
    {
        db.drop_db().await;
        return Err(e);
    }
    let drift = diff(cat, &replica_cat);
    if !drift.is_empty() {
        let drift_sql = emit(
            &drift,
            &EmitOptions { database_flavor: cat.database_flavor, ..Default::default() },
        )
        .sql;
        db.drop_db().await;
        bail!(
            "shadow replica does not faithfully reproduce the {label} ({} residual changes). \
             This is a dpm coverage gap — the verify result would be meaningless.\n{}",
            drift.changes.len(),
            drift_sql
        );
    }
    Ok(db)
}

async fn run_on_replica(p: &VerifyParams<'_>, migration_sql: &str, replica: &ShadowDb) -> Result<VerifyOutcome> {
    // Bootstrap the replica to match the target (destructive allowed: there
    // is nothing to destroy in an empty db), with fidelity sanity-check.
    {
        let bootstrap_plan = diff(p.target, &Catalog::default());
        let bootstrap = emit(
            &bootstrap_plan,
            &EmitOptions {
                allow_destructive: true,
                database_flavor: p.target.database_flavor,
                source_desc: None,
                target_desc: None,
            },
        );
        crate::apply::apply_script(&replica.url, &bootstrap.sql)
            .await
            .context("bootstrapping the target replica on the shadow server failed")?;
        let mut replica_cat = introspect::introspect_url(&replica.url, p.introspect).await?;
        crate::canonicalize::canonicalize_defs(&mut [&mut replica_cat], p.shadow_server_url, p.verbose).await?;
        let drift = diff(p.target, &replica_cat);
        if !drift.is_empty() {
            let drift_sql = emit(
                &drift,
                &EmitOptions { database_flavor: p.target.database_flavor, ..Default::default() },
            )
            .sql;
            bail!(
                "shadow replica does not faithfully reproduce the target ({} residual changes). \
                 This is a dpm coverage gap — the verify result would be meaningless.\n{}",
                drift.changes.len(),
                drift_sql
            );
        }
    }

    // Apply the migration under test.
    crate::apply::apply_script(&replica.url, migration_sql)
        .await
        .context("applying the generated migration to the replica failed")?;

    // Re-diff.
    let mut migrated = introspect::introspect_url(&replica.url, p.introspect).await?;
    crate::canonicalize::canonicalize_defs(&mut [&mut migrated], p.shadow_server_url, p.verbose).await?;
    let residual = diff(p.source, &migrated);
    let converged = residual.is_empty();
    let residual_sql = if converged {
        None
    } else {
        Some(
            emit(
                &residual,
                &EmitOptions { database_flavor: p.source.database_flavor, ..Default::default() },
            )
            .sql,
        )
    };

    // External / cross-checks: need a live URL for the source side.
    let mut checks: Vec<CheckReport> = Vec::new();
    let wants_external = p.external_check.is_some() || p.checks.any();
    if wants_external {
        // Own the materialized replica (if any) so it outlives the URL.
        let mut source_replica: Option<ShadowDb> = None;
        let source_url: Option<String> = match p.source_url_for_external {
            Some(u) => Some(u.to_string()),
            None => {
                match materialize_catalog("source", p.source, p.shadow_server_url, p.introspect, p.verbose).await {
                    Ok(db) => {
                        let url = db.url.clone();
                        source_replica = Some(db);
                        Some(url)
                    }
                    Err(e) => {
                        checks.push(CheckReport {
                            name: "source-replica".into(),
                            command: String::new(),
                            agreed: false,
                            output: String::new(),
                            error: Some(format!("{e:#}")),
                        });
                        None
                    }
                }
            }
        };

        if let Some(source_url) = &source_url {
            // Diff-agreement checkers compare the migrated replica to the source.
            checks.extend(crosscheck::run_diff_checks(&p.checks, &p.bins, &replica.url, source_url));

            // flyway validates the SCRIPT under a standard runner against a
            // fresh replica of the ORIGINAL target.
            let want_flyway = p.checks.flyway
                || (p.checks.all && crosscheck::binary_exists(&p.bins.flyway));
            if want_flyway {
                match materialize_catalog("flyway-target", p.target, p.shadow_server_url, p.introspect, p.verbose)
                    .await
                {
                    Ok(db) => {
                        checks.push(crosscheck::run_flyway(&p.bins.flyway, &db.url, migration_sql));
                        db.drop_db().await;
                    }
                    Err(e) => checks.push(CheckReport {
                        name: "flyway".into(),
                        command: String::new(),
                        agreed: false,
                        output: String::new(),
                        error: Some(format!("flyway replica setup failed: {e:#}")),
                    }),
                }
            }

            if let Some(template) = p.external_check {
                let cmd = template.replace("{source}", source_url).replace("{target}", &replica.url);
                let output = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&cmd)
                    .output()
                    .with_context(|| format!("running external check: {cmd}"))?;
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                checks.push(CheckReport {
                    name: "external".into(),
                    command: cmd,
                    agreed: output.status.success() && stdout.is_empty(),
                    output: stdout,
                    error: None,
                });
            }
        }

        if let Some(db) = source_replica {
            if p.keep_shadow {
                eprintln!("dpm: keeping source replica {}", introspect::redact_url(&db.url));
                db.into_kept();
            } else {
                db.drop_db().await;
            }
        }
    }

    Ok(VerifyOutcome {
        migration_sql: migration_sql.to_string(),
        converged,
        residual_changes: residual.changes.len(),
        residual_sql,
        checks,
    })
}
