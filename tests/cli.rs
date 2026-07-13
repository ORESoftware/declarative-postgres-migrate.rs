//! End-to-end tests of the dpm BINARY (not the library): flag parsing,
//! exit-code contract, output formats, the destructive two-consent gate, and
//! the AI review verdict paths — everything a shell user or CI touches.
//!
//! Database-backed cases are gated on DPM_TEST_DATABASE_URL like the rest of
//! the integration suite; pure CLI cases always run.

use std::process::{Command, Output};

fn dpm() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dpm"))
}

fn admin_url() -> Option<String> {
    match std::env::var("DPM_TEST_DATABASE_URL") {
        Ok(v) if !v.is_empty() => Some(v),
        _ => {
            eprintln!("skipping: DPM_TEST_DATABASE_URL not set (run scripts/test.sh)");
            None
        }
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}
fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

fn scratch(name: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("dpm-cli-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

// ---------------------------------------------------------------------------
// pure CLI (no database)
// ---------------------------------------------------------------------------

#[test]
fn help_shows_commands_and_flag_table() {
    let out = dpm().arg("help").output().unwrap();
    assert!(out.status.success());
    let text = stdout(&out);
    for needle in ["diff", "apply", "verify", "review", "bootstrap", "dump",
                   "--cross-check-all", "SOURCE_DATABASE_URL", "DPM_AI_TRANSPORT"] {
        assert!(text.contains(needle), "help missing {needle:?}");
    }
}

#[test]
fn version_prints_cargo_version() {
    let out = dpm().arg("version").output().unwrap();
    assert!(out.status.success());
    assert!(stdout(&out).contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn unknown_command_and_unknown_flag_error_cleanly() {
    let out = dpm().arg("frobnicate").output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("unknown command"));

    let out = dpm().args(["diff", "--definitely-not-a-flag"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("unknown option"));
}

#[test]
fn diff_without_source_is_a_clear_error() {
    let out = dpm()
        .arg("diff")
        .env_remove("SOURCE_DATABASE_URL")
        .env_remove("TARGET_DATABASE_URL")
        .env_remove("DATABASE_URL")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("no source"));
}

#[test]
fn conflicting_kind_flags_error() {
    let out = dpm()
        .args(["diff", "--source-sql", "a.sql", "--source-json", "b.json", "--target", "postgres://x@y/z"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("pick one"));
}

// ---------------------------------------------------------------------------
// database-backed CLI flows
// ---------------------------------------------------------------------------

const CLI_SOURCE: &str = "CREATE TABLE widgets (id bigserial PRIMARY KEY, name text NOT NULL, price numeric(10,2) NOT NULL DEFAULT 0);\nCREATE INDEX widgets_name_idx ON widgets (name);";
const CLI_TARGET: &str = "CREATE TABLE widgets (id bigserial PRIMARY KEY, name text NOT NULL, obsolete boolean);";

#[test]
fn full_cli_lifecycle_sql_to_live() {
    let Some(admin) = admin_url() else { return };

    // Prepare a live target database via the library helpers.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let target = rt.block_on(async {
        let db = dpm::source::ShadowDb::create(&admin, false).await.unwrap();
        db.apply_sql(CLI_TARGET).await.unwrap();
        db
    });
    let source_sql = scratch("desired.sql");
    std::fs::write(&source_sql, CLI_SOURCE).unwrap();

    // 1. diff (sql -> live): plan present, exit 0 without --fail-on-diff.
    let out = dpm()
        .args(["diff", "--source"])
        .arg(&source_sql)
        .args(["--target", &target.url, "--shadow", &admin])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let sql = stdout(&out);
    assert!(sql.contains("ADD COLUMN IF NOT EXISTS \"price\""), "{sql}");
    assert!(sql.contains("-- ALTER TABLE") && sql.contains("DROP COLUMN"), "destructive gated: {sql}");

    // 2. --fail-on-diff exits 2 while drift exists.
    let out = dpm()
        .args(["diff", "--fail-on-diff", "--source"])
        .arg(&source_sql)
        .args(["--target", &target.url, "--shadow", &admin])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));

    // 3. json format carries plan + summary.
    let out = dpm()
        .args(["diff", "--format", "json", "--source"])
        .arg(&source_sql)
        .args(["--target", &target.url, "--shadow", &admin])
        .output()
        .unwrap();
    let doc: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert!(doc["summary"]["total"].as_u64().unwrap() >= 2);
    assert!(doc["changes"].as_array().unwrap().iter().any(|c| c["op"] == "add_column"));

    // 4. two-consent gate: sql-consent without ops-consent refuses pre-write.
    let out = dpm()
        .args(["apply", "--yes", "--allow-destructive-sql", "--source"])
        .arg(&source_sql)
        .args(["--target", &target.url, "--shadow", &admin])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("--allow-destructive-ops"));

    // 5. review with a fake AI: reject => exit 4, approve => exit 0.
    let out = dpm()
        .args(["review", "--ai-tool", "custom", "--ai-cmd", "echo 'DPM_VERDICT: REJECT nope'", "--source"])
        .arg(&source_sql)
        .args(["--target", &target.url, "--shadow", &admin])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    let out = dpm()
        .args(["review", "--ai-tool", "custom", "--ai-cmd", "echo 'DPM_VERDICT: APPROVE'", "--source"])
        .arg(&source_sql)
        .args(["--target", &target.url, "--shadow", &admin])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    // 6. apply with both consents converges; then fail-on-diff exits 0.
    let out = dpm()
        .args(["apply", "--yes", "--allow-destructive", "--source"])
        .arg(&source_sql)
        .args(["--target", &target.url, "--shadow", &admin])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("converged"));
    let out = dpm()
        .args(["diff", "--fail-on-diff", "--source"])
        .arg(&source_sql)
        .args(["--target", &target.url, "--shadow", &admin])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "post-apply drift: {}", stdout(&out));

    // 7. dump -> catalog.json usable as a side; json↔live now identical.
    let dump_path = scratch("target.json");
    let out = dpm()
        .args(["dump", "--source", &target.url, "-o"])
        .arg(&dump_path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let out = dpm()
        .args(["diff", "--fail-on-diff", "--source"])
        .arg(&dump_path)
        .args(["--target", &target.url])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));

    // 8. verify end-to-end through the CLI (with a stubbed agreeing checker).
    let out = dpm()
        .args(["verify", "--source"])
        .arg(&source_sql)
        .args(["--target", &target.url, "--shadow", &admin, "--allow-destructive-sql"])
        .args(["--external-check", "true"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stderr(&out).contains("VERIFIED"));

    rt.block_on(target.drop_db());
}

#[test]
fn bootstrap_emits_full_ddl_without_target() {
    let Some(admin) = admin_url() else { return };
    let source_sql = scratch("boot.sql");
    std::fs::write(&source_sql, CLI_SOURCE).unwrap();
    let out = dpm()
        .args(["bootstrap", "--source"])
        .arg(&source_sql)
        .args(["--shadow", &admin])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let sql = stdout(&out);
    assert!(sql.contains("CREATE TABLE IF NOT EXISTS \"public\".\"widgets\""));
    assert!(sql.contains("widgets_name_idx"));
}
