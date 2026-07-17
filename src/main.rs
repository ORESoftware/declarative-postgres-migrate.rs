//! dpm — declarative postgres migrate.
//!
//! Commands:
//!   dpm diff       generate the migration SQL (or JSON plan) converging target → source
//!   dpm apply      generate and execute against the target (confirmed, gated)
//!   dpm dump       snapshot a database's catalog to JSON
//!   dpm bootstrap  emit full DDL for a source (diff against empty)
//!   dpm verify     replay the migration on a shadow replica and prove convergence
//!   dpm review     run the AI reviewer over the generated migration
//!   dpm help       flag/env reference
//!
//! All flags follow the flags-2-env contract in `.cli-flags.toml`:
//! every flag maps to an env var; precedence is flag > env > default.

use std::io::Write as _;

use anyhow::{bail, Context, Result};

use dpm::advisor;
use dpm::ai::{self, ReviewOutcome, ReviewRequest};
use dpm::diff::{diff, Plan};
use dpm::emit::{emit, EmitOptions, Script};
use dpm::flagenv::{self, Resolved};
use dpm::introspect::IntrospectOptions;
use dpm::model::{Catalog, DatabaseFlavor};
use dpm::source::{resolve, ResolveContext, SideSpec};
use dpm::verify::{verify, VerifyParams};

const USAGE: &str = "\
dpm — declarative postgres migrate

USAGE
  dpm <command> [flags]

COMMANDS
  diff        Generate SQL that converges the target onto the source (stdout or --out).
  apply       Generate and execute the migration against the target database.
  dump        Snapshot a database catalog to JSON (feed to later diffs, CI, or AI review).
  bootstrap   Emit the full DDL for a source (equivalent to diffing against an empty database).
  verify      Rehearse the migration on a shadow replica of the target and prove convergence.
  review      Generate the migration and have an AI tool review it (claude/codex/chatgpt/gemini).
  help        Show this help and the flag/env reference.

SIDES (any combination works for diff/review/verify; apply needs a live URL target)
  --source / --target each accept:
    postgres:// URL        live database (introspected)
    path/to/catalog.json   saved catalog dump (from `dpm dump`)
    path/to/schema.sql     schema file or pg_dump --schema-only dump (materialized on --shadow)
  Explicit-kind flags override the generic ones:
    --source-sql/--target-sql (SOURCE_SQL_FILE/TARGET_SQL_FILE)
    --source-json/--target-json (SOURCE_CATALOG_JSON/TARGET_CATALOG_JSON)

DESTRUCTIVE CHANGES (two separate consents)
  --allow-destructive-sql   generate destructive statements live (otherwise commented out)
  --allow-destructive-ops   actually execute destructive statements during `dpm apply`
  --allow-destructive       legacy shorthand for both

CROSS-CHECKS (independent diff engines validate dpm's result; verify + apply)
  --cross-check-with-migra    run migra after migrating; agreement = no remaining diff
  --cross-check-with-pgdiff   run pgdiff (joncrlsn) across all schema aspects
  --external-check 'cmd {target} {source}'   any custom checker (empty stdout = agreement)
  Install the tools: scripts/install-crosscheckers.sh

EXIT CODES
  0 success · 1 error · 2 differences found (--fail-on-diff)
  3 verify/apply not converged or a cross-check disagreed
  4 AI reviewer rejected the migration (with --ai-strict, the default)
";

fn main() {
    let code = match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("dpm: error: {err:#}");
            1
        }
    };
    std::process::exit(code);
}

fn run() -> Result<i32> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (command, rest) = match argv.split_first() {
        Some((c, rest)) if !c.starts_with('-') => (c.clone(), rest.to_vec()),
        _ => ("help".to_string(), argv.clone()),
    };

    let config = flagenv::load_config()?;
    if command == "version" || rest.iter().any(|a| a == "--version") {
        println!("dpm {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    if command == "help" || rest.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        println!("FLAGS (flags-2-env contract; flag > env > default)");
        print!("{}", flagenv::help_table(&config));
        return Ok(0);
    }

    let (overrides, positionals) = flagenv::parse(&config, &rest)?;
    if !positionals.is_empty() {
        bail!("unexpected positional arguments: {positionals:?}");
    }
    let resolved = Resolved::new(&config, overrides);

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(dispatch(&command, &resolved))
}

async fn dispatch(command: &str, r: &Resolved) -> Result<i32> {
    match command {
        "diff" => cmd_diff(r, false).await,
        "apply" => cmd_apply(r).await,
        "dump" => cmd_dump(r).await,
        "bootstrap" => cmd_diff(r, true).await,
        "verify" => cmd_verify(r).await,
        "review" => cmd_review(r).await,
        other => bail!("unknown command {other:?} — run `dpm help`"),
    }
}

// ---------------------------------------------------------------------------
// shared plumbing
// ---------------------------------------------------------------------------

fn introspect_options(r: &Resolved) -> IntrospectOptions {
    let split = |s: String| {
        s.split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
    };
    IntrospectOptions {
        schemas: r.get("DPM_SCHEMAS").map(split),
        extra_excluded: r.get("DPM_EXCLUDE_SCHEMAS").map(split).unwrap_or_default(),
    }
}

/// Two independent consents around destructive changes. The legacy
/// DPM_ALLOW_DESTRUCTIVE implies both.
#[derive(Clone, Copy, Debug)]
struct DestructivePolicy {
    sql: bool,
    ops: bool,
}

fn destructive_policy(r: &Resolved) -> DestructivePolicy {
    let legacy = r.get_bool("DPM_ALLOW_DESTRUCTIVE");
    DestructivePolicy {
        sql: legacy || r.get_bool("DPM_ALLOW_DESTRUCTIVE_SQL"),
        ops: legacy || r.get_bool("DPM_ALLOW_DESTRUCTIVE_OPS"),
    }
}

/// Resolve one side from its explicit-kind flags first, then the generic one.
fn side_spec(
    r: &Resolved,
    sql_key: &str,
    json_key: &str,
    generic_keys: &[&str],
    side: &str,
) -> Result<SideSpec> {
    let sql = r.get(sql_key);
    let json = r.get(json_key);
    if sql.is_some() && json.is_some() {
        bail!("both {sql_key} and {json_key} are set — pick one for the {side}");
    }
    if let Some(path) = sql {
        if !path.to_ascii_lowercase().ends_with(".sql") {
            bail!("{sql_key} must point at a .sql file, got {path:?}");
        }
        return Ok(SideSpec::SqlPath(path));
    }
    if let Some(path) = json {
        if !path.to_ascii_lowercase().ends_with(".json") {
            bail!("{json_key} must point at a .json catalog dump, got {path:?}");
        }
        return Ok(SideSpec::JsonPath(path));
    }
    let raw = r
        .get_first(generic_keys)
        .with_context(|| format!("no {side}: pass --{side} (or the matching env var; run `dpm help`)"))?;
    SideSpec::parse(&raw)
}

fn source_spec(r: &Resolved) -> Result<SideSpec> {
    side_spec(r, "SOURCE_SQL_FILE", "SOURCE_CATALOG_JSON", &["SOURCE_DATABASE_URL"], "source")
}

fn target_spec(r: &Resolved) -> Result<SideSpec> {
    side_spec(
        r,
        "TARGET_SQL_FILE",
        "TARGET_CATALOG_JSON",
        &["TARGET_DATABASE_URL", "DATABASE_URL"],
        "target",
    )
}

fn write_output(r: &Resolved, content: &str) -> Result<()> {
    match r.get("DPM_OUT") {
        Some(path) => {
            std::fs::write(&path, content).with_context(|| format!("writing {path}"))?;
            eprintln!("dpm: wrote {path}");
        }
        None => {
            print!("{content}");
            std::io::stdout().flush()?;
        }
    }
    Ok(())
}

fn summarize(script: &Script) {
    eprintln!(
        "dpm: {} change(s), {} destructive ({} gated), {} manual-review",
        script.change_count, script.destructive_count, script.gated_count, script.manual_count
    );
}

struct DiffInputs {
    source_cat: Catalog,
    target_cat: Catalog,
    source_desc: String,
    target_desc: String,
}

async fn load_sides(r: &Resolved, bootstrap: bool) -> Result<DiffInputs> {
    let opts = introspect_options(r);
    let ctx = ResolveContext {
        introspect: &opts,
        shadow_url: r.get("SHADOW_DATABASE_URL"),
        keep_shadow: r.get_bool("DPM_KEEP_SHADOW"),
        verbose: r.get_bool("DPM_VERBOSE"),
    };
    let source = source_spec(r)?;
    let source_cat = resolve(&source, &ctx).await.context("loading source")?;
    let (target_cat, target_desc) = if bootstrap {
        // Truly empty (no schemas) so CREATE SCHEMA statements are included.
        (Catalog::default(), "(empty database)".to_string())
    } else {
        let target = target_spec(r)?;
        let cat = resolve(&target, &ctx).await.context("loading target")?;
        (cat, target.describe())
    };
    // Catalog syntax is only comparable within one server family.  A JSON
    // dump records the family too, so this also prevents a stale PostgreSQL
    // snapshot from being applied to CockroachDB (or the reverse).
    if source_cat.server_version_num != 0
        && target_cat.server_version_num != 0
        && source_cat.database_flavor != target_cat.database_flavor
    {
        bail!(
            "source is {} but target is {}; dpm does not generate cross-dialect migrations",
            source_cat.database_flavor.label(),
            target_cat.database_flavor.label()
        );
    }
    let mut inputs = DiffInputs { source_cat, target_cat, source_desc: source.describe(), target_desc };
    // With a shadow server available, normalize CHECK and index defs to their
    // re-parse fixed point so string comparison is exact regardless of whether
    // a side was built from original SQL or from dpm's own emitted deparse.
    if let Some(shadow) = &ctx.shadow_url {
        dpm::canonicalize::canonicalize_defs(&mut [&mut inputs.source_cat, &mut inputs.target_cat], shadow, ctx.verbose)
            .await
            .context("canonicalizing deparsed definitions on the shadow server")?;
    }
    Ok(inputs)
}

/// Migration script + optional FK-index advisory block.
fn render(r: &Resolved, inputs: &DiffInputs, allow_destructive_sql: bool) -> (Plan, Script, String) {
    let plan = diff(&inputs.source_cat, &inputs.target_cat);
    let script = emit(
        &plan,
        &EmitOptions {
            allow_destructive: allow_destructive_sql,
            database_flavor: inputs.source_cat.database_flavor,
            source_desc: Some(inputs.source_desc.clone()),
            target_desc: Some(inputs.target_desc.clone()),
        },
    );
    let mut text = script.sql.clone();
    if r.get_bool("DPM_ADVISE_FK_INDEXES") {
        let advice = advisor::advise_fk_indexes(&inputs.source_cat);
        let block = advisor::advisory_comment_block(&advice);
        if !block.is_empty() {
            text.push('\n');
            text.push_str(&block);
        }
    }
    (plan, script, text)
}

/// Run the configured AI reviewer over a generated migration. Returns None
/// when AI review is not enabled.
async fn maybe_ai_review(
    r: &Resolved,
    plan: &Plan,
    script: &Script,
    inputs: &DiffInputs,
    policy: DestructivePolicy,
    force: bool,
) -> Result<Option<ReviewOutcome>> {
    if !(force || r.get_bool("DPM_AI_REVIEW")) {
        return Ok(None);
    }
    let tool = r.get("DPM_AI_TOOL").unwrap_or_else(|| "claude".to_string());
    let custom = r.get("DPM_AI_CMD");
    let transport = ai::Transport::parse(&r.get("DPM_AI_TRANSPORT").unwrap_or_else(|| "auto".into()))?;
    let model = r.get("DPM_AI_MODEL");
    let req = ReviewRequest {
        sql: script.sql.clone(),
        plan_json: serde_json::to_string_pretty(&plan.changes)?,
        source_desc: inputs.source_desc.clone(),
        target_desc: inputs.target_desc.clone(),
        allow_destructive_sql: policy.sql,
        allow_destructive_ops: policy.ops,
        total_changes: script.change_count,
        destructive_changes: script.destructive_count,
        gated_changes: script.gated_count,
        manual_changes: script.manual_count,
    };
    eprintln!("dpm: ai review via {tool} ...");
    let outcome = ai::run_review(
        &tool,
        custom.as_deref(),
        transport,
        model.as_deref(),
        &req,
        r.get_bool("DPM_VERBOSE"),
    )
    .await?;
    match (&outcome.approved, &outcome.verdict) {
        (true, Some(v)) => eprintln!("dpm: ai review: {v}"),
        (_, Some(v)) => eprintln!("dpm: ai review REJECTED: {v}"),
        (_, None) => eprintln!("dpm: ai review returned no parseable verdict (treated as rejection)"),
    }
    Ok(Some(outcome))
}

fn ai_strict(r: &Resolved) -> bool {
    r.get_bool("DPM_AI_STRICT")
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

async fn cmd_diff(r: &Resolved, bootstrap: bool) -> Result<i32> {
    let inputs = load_sides(r, bootstrap).await?;
    let policy = destructive_policy(r);
    // Bootstrap of an empty database has nothing to destroy; always live.
    let allow_sql = policy.sql || bootstrap;
    let (plan, script, text) = render(r, &inputs, allow_sql);

    if r.get("DPM_FORMAT").as_deref() == Some("json") {
        let doc = serde_json::json!({
            "source": inputs.source_desc,
            "target": inputs.target_desc,
            "changes": plan.changes,
            "summary": {
                "total": script.change_count,
                "destructive": script.destructive_count,
                "gated": script.gated_count,
                "manual": script.manual_count,
            },
            "sql": text,
        });
        write_output(r, &format!("{}\n", serde_json::to_string_pretty(&doc)?))?;
    } else {
        write_output(r, &text)?;
    }
    summarize(&script);

    if let Some(outcome) = maybe_ai_review(r, &plan, &script, &inputs, policy, false).await? {
        if !outcome.approved && ai_strict(r) {
            return Ok(4);
        }
    }
    if r.get_bool("DPM_FAIL_ON_DIFF") && !plan.is_empty() {
        return Ok(2);
    }
    Ok(0)
}

async fn cmd_apply(r: &Resolved) -> Result<i32> {
    let target = target_spec(r)?;
    let SideSpec::Url(target_url) = &target else {
        bail!("apply needs a live --target database URL (got {})", target.describe());
    };
    let inputs = load_sides(r, false).await?;
    let policy = destructive_policy(r);
    if inputs.source_cat.database_flavor == DatabaseFlavor::Cockroach && check_selection(r).any() {
        bail!(
            "the bundled cross-checkers are PostgreSQL-only; CockroachDB apply still performs dpm's post-apply convergence check"
        );
    }
    let (plan, script, text) = render(r, &inputs, policy.sql);

    if plan.is_empty() {
        eprintln!("dpm: no differences — nothing to apply");
        return Ok(0);
    }

    // Two-consent destructive model: generating live destructive SQL is one
    // decision (--allow-destructive-sql); executing it is another
    // (--allow-destructive-ops). Fail closed before touching the database.
    let live_destructive = script.destructive_count - script.gated_count;
    if live_destructive > 0 && !policy.ops {
        bail!(
            "the migration contains {live_destructive} live destructive statement(s) but \
             executing destructive operations was not approved — re-run with \
             --allow-destructive-ops (DPM_ALLOW_DESTRUCTIVE_OPS=true) to proceed, or drop \
             --allow-destructive-sql to keep them gated"
        );
    }
    if script.gated_count > 0 {
        eprintln!(
            "dpm: note: {} destructive change(s) are gated (commented out); \
             re-run with --allow-destructive-sql --allow-destructive-ops to include them",
            script.gated_count
        );
    }

    // AI review runs BEFORE anything touches the database.
    if let Some(outcome) = maybe_ai_review(r, &plan, &script, &inputs, policy, false).await? {
        if !outcome.approved {
            if ai_strict(r) {
                eprintln!("dpm: aborting apply (AI reviewer rejected; use --ai-strict=false to override)");
                eprintln!("--- reviewer transcript ---\n{}", outcome.transcript);
                return Ok(4);
            }
            eprintln!("dpm: WARNING: AI reviewer rejected but --ai-strict=false; continuing");
        }
    }

    // House rule: never write to a database without explicit confirmation.
    eprintln!("{text}");
    summarize(&script);
    if !r.get_bool("DPM_YES") {
        eprint!(
            "dpm: apply the SQL above to {}? Type 'yes' to continue: ",
            dpm::introspect::redact_url(target_url)
        );
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if answer.trim() != "yes" {
            eprintln!("dpm: aborted (nothing was applied)");
            return Ok(1);
        }
    }

    let report = dpm::apply::apply_script(target_url, &script.sql).await?;
    eprintln!(
        "dpm: applied {} statement(s) to {}",
        report.executed,
        dpm::introspect::redact_url(target_url)
    );

    // Post-apply convergence check against the freshly migrated target.
    let opts = introspect_options(r);
    let mut migrated = dpm::introspect::introspect_url(target_url, &opts).await?;
    if let Some(shadow) = r.get("SHADOW_DATABASE_URL") {
        dpm::canonicalize::canonicalize_defs(&mut [&mut migrated], &shadow, r.get_bool("DPM_VERBOSE"))
            .await
            .context("canonicalizing deparsed definitions on the shadow server")?;
    }
    let residual = diff(&inputs.source_cat, &migrated);
    let residual_real: Vec<_> = residual.changes.iter().filter(|c| !c.is_manual()).collect();
    if residual_real.is_empty() {
        eprintln!("dpm: converged — target now matches the source");
    } else {
        eprintln!(
            "dpm: warning: {} change(s) remain after apply (gated destructive or manual items)",
            residual_real.len()
        );
    }

    // Optional independent cross-checks of the freshly migrated target.
    // (flyway is verify-only: it validates the script on a replica, and the
    // script has already run here.)
    let selection = check_selection(r);
    if selection.any() {
        if selection.flyway {
            eprintln!("dpm: note: --cross-check-with-flyway applies to `dpm verify` only (script already applied)");
        }
        let source_url = match source_spec(r)? {
            SideSpec::Url(u) => Some(u),
            _ => None,
        };
        // File-based sources get a temporary replica when a shadow server is
        // available, so the external tools have two live databases.
        let mut source_replica = None;
        let compare_url = match source_url {
            Some(u) => Some(u),
            None => match r.get("SHADOW_DATABASE_URL") {
                Some(shadow) => {
                    let db = dpm::verify::materialize_catalog(
                        "source",
                        &inputs.source_cat,
                        &shadow,
                        &opts,
                        r.get_bool("DPM_VERBOSE"),
                    )
                    .await?;
                    let url = db.url.clone();
                    source_replica = Some(db);
                    Some(url)
                }
                None => {
                    eprintln!(
                        "dpm: skipping cross-checks: source is not a live URL and no --shadow \
                         server was given to materialize it"
                    );
                    None
                }
            },
        };
        if let Some(compare_url) = compare_url {
            let checks = dpm::crosscheck::run_diff_checks(&selection, &check_bins(r), target_url, &compare_url);
            report_checks(&checks);
            let scan_ok = maybe_ai_discrepancy_scan(
                r,
                residual_real.is_empty(),
                None,
                &checks,
            )
            .await?
            .unwrap_or(true);
            if let Some(db) = source_replica {
                db.drop_db().await;
            }
            if !checks.iter().all(|c| c.agreed) {
                return Ok(3);
            }
            if !scan_ok && ai_strict(r) {
                return Ok(4);
            }
        }
    }
    Ok(0)
}

async fn cmd_dump(r: &Resolved) -> Result<i32> {
    // dump reads one database: --source, falling back to --target/DATABASE_URL.
    let raw = r
        .get_first(&["SOURCE_DATABASE_URL", "TARGET_DATABASE_URL", "DATABASE_URL"])
        .context("no database: pass --source (or --target / DATABASE_URL)")?;
    let SideSpec::Url(url) = SideSpec::parse(&raw)? else {
        bail!("dump needs a live database URL");
    };
    let opts = introspect_options(r);
    let cat = dpm::introspect::introspect_url(&url, &opts)
        .await
        .context("introspecting database for dump")?;
    eprintln!(
        "dpm: dumped {} object(s) across schemas: {}",
        cat.object_count(),
        cat.schemas.iter().cloned().collect::<Vec<_>>().join(", ")
    );
    write_output(r, &format!("{}\n", serde_json::to_string_pretty(&cat)?))?;
    Ok(0)
}

fn check_selection(r: &Resolved) -> dpm::crosscheck::CheckSelection {
    dpm::crosscheck::CheckSelection {
        migra: r.get_bool("DPM_CROSS_CHECK_MIGRA"),
        pgdiff: r.get_bool("DPM_CROSS_CHECK_PGDIFF"),
        atlas: r.get_bool("DPM_CROSS_CHECK_ATLAS"),
        pg_schema_diff: r.get_bool("DPM_CROSS_CHECK_PG_SCHEMA_DIFF"),
        liquibase: r.get_bool("DPM_CROSS_CHECK_LIQUIBASE"),
        apgdiff: r.get_bool("DPM_CROSS_CHECK_APGDIFF"),
        flyway: r.get_bool("DPM_CROSS_CHECK_FLYWAY"),
        all: r.get_bool("DPM_CROSS_CHECK_ALL"),
    }
}

fn check_bins(r: &Resolved) -> dpm::crosscheck::Bins {
    let get = |key: &str, default: &str| r.get(key).unwrap_or_else(|| default.into());
    dpm::crosscheck::Bins {
        migra: get("DPM_MIGRA_BIN", "migra"),
        pgdiff: get("DPM_PGDIFF_BIN", "pgdiff"),
        atlas: get("DPM_ATLAS_BIN", "atlas"),
        pg_schema_diff: get("DPM_PG_SCHEMA_DIFF_BIN", "pg-schema-diff"),
        liquibase: get("DPM_LIQUIBASE_BIN", "liquibase"),
        apgdiff: get("DPM_APGDIFF_BIN", "apgdiff"),
        flyway: get("DPM_FLYWAY_BIN", "flyway"),
        pg_dump: get("DPM_PG_DUMP_BIN", "pg_dump"),
    }
}

/// AI discrepancy scan over the assembled cross-check reports. Returns
/// Some(approved) when the scan ran.
async fn maybe_ai_discrepancy_scan(
    r: &Resolved,
    converged: bool,
    residual_sql: Option<&str>,
    checks: &[dpm::crosscheck::CheckReport],
) -> Result<Option<bool>> {
    if !r.get_bool("DPM_CROSS_CHECK_AI") {
        return Ok(None);
    }
    let tool = r.get("DPM_AI_TOOL").unwrap_or_else(|| "claude".to_string());
    let custom = r.get("DPM_AI_CMD");
    let transport = ai::Transport::parse(&r.get("DPM_AI_TRANSPORT").unwrap_or_else(|| "auto".into()))?;
    let model = r.get("DPM_AI_MODEL");
    let reports: Vec<(String, bool, String, Option<String>)> = checks
        .iter()
        .map(|c| (c.name.clone(), c.agreed, c.output.clone(), c.error.clone()))
        .collect();
    let payload = ai::build_discrepancy_payload(converged, residual_sql, &reports);
    eprintln!("dpm: ai discrepancy scan via {tool} ...");
    let outcome = ai::run_payload(&tool, custom.as_deref(), transport, model.as_deref(), &payload, r.get_bool("DPM_VERBOSE")).await?;
    match (&outcome.approved, &outcome.verdict) {
        (true, Some(v)) => eprintln!("dpm: ai discrepancy scan: {v}"),
        (_, Some(v)) => eprintln!("dpm: ai discrepancy scan REJECTED: {v}\n{}", outcome.transcript),
        (_, None) => eprintln!("dpm: ai discrepancy scan returned no parseable verdict (treated as rejection)"),
    }
    Ok(Some(outcome.approved))
}

async fn cmd_verify(r: &Resolved) -> Result<i32> {
    let shadow = r
        .get("SHADOW_DATABASE_URL")
        .context("verify needs --shadow (SHADOW_DATABASE_URL): a server where dpm may create throwaway databases")?;
    let inputs = load_sides(r, false).await?;
    let opts = introspect_options(r);
    let policy = destructive_policy(r);
    let checks = check_selection(r);
    if inputs.source_cat.database_flavor == DatabaseFlavor::Cockroach
        && (checks.any() || r.get("DPM_EXTERNAL_CHECK").is_some())
    {
        bail!(
            "external cross-checkers are PostgreSQL-only; CockroachDB verify uses dpm's shadow-replay convergence proof"
        );
    }
    let source_url = match source_spec(r)? {
        SideSpec::Url(u) => Some(u),
        _ => None,
    };
    let external = r.get("DPM_EXTERNAL_CHECK");

    let outcome = verify(VerifyParams {
        source: &inputs.source_cat,
        target: &inputs.target_cat,
        shadow_server_url: &shadow,
        source_url_for_external: source_url.as_deref(),
        allow_destructive: policy.sql,
        external_check: external.as_deref(),
        checks,
        bins: check_bins(r),
        keep_shadow: r.get_bool("DPM_KEEP_SHADOW"),
        verbose: r.get_bool("DPM_VERBOSE"),
        introspect: &opts,
    })
    .await?;

    if let Some(path) = r.get("DPM_OUT") {
        std::fs::write(&path, &outcome.migration_sql)?;
        eprintln!("dpm: wrote verified migration to {path}");
    }

    if outcome.converged {
        eprintln!("dpm: VERIFIED — migration converges the target onto the source");
    } else {
        eprintln!(
            "dpm: NOT CONVERGED — {} residual change(s) after applying the migration to the replica",
            outcome.residual_changes
        );
        if let Some(sql) = &outcome.residual_sql {
            eprintln!("--- residual diff ---\n{sql}");
        }
    }
    report_checks(&outcome.checks);

    // AI discrepancy scan over the cross-check reports.
    let mut ai_ok = true;
    if let Some(approved) =
        maybe_ai_discrepancy_scan(r, outcome.converged, outcome.residual_sql.as_deref(), &outcome.checks).await?
    {
        ai_ok &= approved || !ai_strict(r);
    }

    // AI review of the (now convergence-proven) migration.
    {
        let plan = diff(&inputs.source_cat, &inputs.target_cat);
        let script = emit(
            &plan,
            &EmitOptions {
                allow_destructive: policy.sql,
                database_flavor: inputs.source_cat.database_flavor,
                source_desc: Some(inputs.source_desc.clone()),
                target_desc: Some(inputs.target_desc.clone()),
            },
        );
        if let Some(review) = maybe_ai_review(r, &plan, &script, &inputs, policy, false).await? {
            ai_ok &= review.approved || !ai_strict(r);
        }
    }

    if !ai_ok {
        return Ok(4);
    }
    Ok(if outcome.converged && outcome.all_checks_agreed() { 0 } else { 3 })
}

fn report_checks(checks: &[dpm::crosscheck::CheckReport]) {
    for c in checks {
        match (&c.error, c.agreed) {
            (Some(err), _) => eprintln!("dpm: cross-check {} ERROR: {err}", c.name),
            (None, true) => eprintln!("dpm: cross-check {} agreed (no remaining differences)", c.name),
            (None, false) => eprintln!(
                "dpm: cross-check {} DISAGREED — it still sees differences:\n{}",
                c.name, c.output
            ),
        }
    }
}

async fn cmd_review(r: &Resolved) -> Result<i32> {
    let inputs = load_sides(r, false).await?;
    let policy = destructive_policy(r);
    let (plan, script, text) = render(r, &inputs, policy.sql);

    if let Some(path) = r.get("DPM_OUT") {
        std::fs::write(&path, &text)?;
        eprintln!("dpm: wrote migration to {path}");
    }
    summarize(&script);

    let outcome = maybe_ai_review(r, &plan, &script, &inputs, policy, true).await?
        .expect("review command forces AI review");
    println!("{}", outcome.transcript.trim_end());
    if outcome.approved {
        Ok(0)
    } else {
        Ok(if ai_strict(r) { 4 } else { 0 })
    }
}
