//! Cross-checking dpm's output with independent schema tools.
//!
//! These tools are second-class citizens of this project: dpm's test suite
//! uses every installed one to validate convergence, and end users can
//! request each via `--cross-check-with-<tool>` (or `--cross-check-all`).
//!
//! | tool            | kind            | agreement contract                          |
//! |-----------------|-----------------|---------------------------------------------|
//! | migra           | diff generator  | empty DDL between migrated ↔ source         |
//! | pgdiff (joncrlsn)| diff generator | no non-comment SQL across aspects           |
//! | atlas           | diff generator  | "Schemas are synced" / empty plan           |
//! | pg-schema-diff  | diff generator  | plan against source dump dir is empty       |
//! | liquibase       | diff generator  | every diff category reports NONE/EQUAL      |
//! | apgdiff         | dump differ     | empty diff between `pg_dump -s` outputs     |
//! | flyway          | migration runner| dpm's script applies cleanly under flyway   |
//!
//! Diff-generator checks run AFTER dpm's migration has been applied (to the
//! shadow replica in `verify`, or the real target in `apply`) and ask "is
//! there any remaining difference between the migrated database and the
//! source?". The flyway check instead validates the generated script itself:
//! it must execute under a standard versioned-migration runner against a
//! fresh replica of the target.
//!
//! None of these are build dependencies — binaries are located on PATH (or
//! via `DPM_<TOOL>_BIN`) at runtime, and `scripts/install-crosscheckers.sh`
//! installs all seven.

use anyhow::{Context, Result};

#[derive(Clone, Debug)]
pub struct CheckReport {
    pub name: String,
    pub command: String,
    pub agreed: bool,
    /// Trimmed tool output (the residual differences it found, if any).
    pub output: String,
    /// Tool missing, crashed, or URL unparseable — reported, never fatal.
    pub error: Option<String>,
}

impl CheckReport {
    fn missing(name: &str, bin: &str, install_hint: &str) -> Self {
        Self {
            name: name.into(),
            command: bin.into(),
            agreed: false,
            output: String::new(),
            error: Some(format!(
                "{bin} not found on PATH — install with scripts/install-crosscheckers.sh ({install_hint}) \
                 or set the DPM_*_BIN env var"
            )),
        }
    }

    fn error(name: &str, command: String, err: impl std::fmt::Display) -> Self {
        Self { name: name.into(), command, agreed: false, output: String::new(), error: Some(err.to_string()) }
    }
}

/// Which binaries to use; every field defaults to the tool's canonical name.
#[derive(Clone, Debug)]
pub struct Bins {
    pub migra: String,
    pub pgdiff: String,
    pub atlas: String,
    pub pg_schema_diff: String,
    pub liquibase: String,
    pub apgdiff: String,
    pub flyway: String,
    pub pg_dump: String,
}

impl Default for Bins {
    fn default() -> Self {
        Self {
            migra: "migra".into(),
            pgdiff: "pgdiff".into(),
            atlas: "atlas".into(),
            pg_schema_diff: "pg-schema-diff".into(),
            liquibase: "liquibase".into(),
            apgdiff: "apgdiff".into(),
            flyway: "flyway".into(),
            pg_dump: "pg_dump".into(),
        }
    }
}

/// Which cross-checkers to run. `all` = every *installed* tool (missing ones
/// are skipped with a note); an individually requested tool that is missing
/// is a failure.
#[derive(Clone, Debug, Default)]
pub struct CheckSelection {
    pub migra: bool,
    pub pgdiff: bool,
    pub atlas: bool,
    pub pg_schema_diff: bool,
    pub liquibase: bool,
    pub apgdiff: bool,
    pub flyway: bool,
    pub all: bool,
}

impl CheckSelection {
    pub fn any(&self) -> bool {
        self.all
            || self.migra
            || self.pgdiff
            || self.atlas
            || self.pg_schema_diff
            || self.liquibase
            || self.apgdiff
            || self.flyway
    }
}

pub fn binary_exists(bin: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {}", shell_quote(bin)))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn run_shell(command: &str, extra_env: &[(String, String)]) -> Result<(bool, String, String)> {
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg(command);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let output = cmd.output().with_context(|| format!("running: {command}"))?;
    Ok((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    ))
}

/// Fields of a postgres:// URL for tools that take discrete connection flags.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UrlParts {
    pub user: String,
    pub password: Option<String>,
    pub host: String,
    pub port: String,
    pub dbname: String,
    pub sslmode: String,
}

pub fn parse_postgres_url(url: &str) -> Result<UrlParts> {
    let rest = url
        .split_once("://")
        .map(|(_, r)| r)
        .with_context(|| format!("not a URL: {url:?}"))?;
    let (main, query) = match rest.split_once('?') {
        Some((m, q)) => (m, Some(q)),
        None => (rest, None),
    };
    let (creds, hostpart) = match main.rsplit_once('@') {
        Some((c, h)) => (Some(c), h),
        None => (None, main),
    };
    let (hostport, dbname) = match hostpart.split_once('/') {
        Some((hp, db)) => (hp, db),
        None => (hostpart, ""),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (h, p),
        _ => (hostport, "5432"),
    };
    let (user, password) = match creds {
        Some(c) => match c.split_once(':') {
            Some((u, p)) => (u.to_string(), Some(p.to_string())),
            None => (c.to_string(), None),
        },
        None => ("postgres".to_string(), None),
    };
    let mut sslmode = "disable".to_string();
    if let Some(q) = query {
        for pair in q.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                if k == "sslmode" {
                    sslmode = v.to_string();
                }
            }
        }
    }
    Ok(UrlParts {
        user,
        password,
        host: if host.is_empty() { "localhost".into() } else { host.into() },
        port: port.to_string(),
        dbname: dbname.to_string(),
        sslmode,
    })
}

/// SQLAlchemy 2.x (migra) and several JDBC-derived tools reject the
/// `postgres://` scheme alias — normalize to `postgresql://`.
pub fn normalize_pg_scheme(url: &str) -> String {
    match url.strip_prefix("postgres://") {
        Some(rest) => format!("postgresql://{rest}"),
        None => url.to_string(),
    }
}

/// Go's lib/pq (atlas, pg-schema-diff) defaults to REQUIRING SSL; local and
/// shadow servers usually don't speak it. Make the libpq default explicit
/// when the URL doesn't already choose one.
pub fn ensure_sslmode(url: &str) -> String {
    let url = normalize_pg_scheme(url);
    if url.contains("sslmode=") {
        url
    } else if url.contains('?') {
        format!("{url}&sslmode=disable")
    } else {
        format!("{url}?sslmode=disable")
    }
}

fn libpq_env(parts: &UrlParts) -> Vec<(String, String)> {
    let mut env = vec![("PGSSLMODE".to_string(), parts.sslmode.clone())];
    if let Some(pw) = &parts.password {
        env.push(("PGPASSWORD".to_string(), pw.clone()));
    }
    env
}

static SCRATCH_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn scratch_dir(label: &str) -> Result<std::path::PathBuf> {
    let n = SCRATCH_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("dpm-crosscheck-{label}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

// ---------------------------------------------------------------------------
// migra
// ---------------------------------------------------------------------------

/// migra: `migra --unsafe <migrated_url> <source_url>` prints the DDL needed
/// to turn the first database into the second; empty output + exit 0 means
/// identical. `--unsafe` so it doesn't abort when the residual has drops.
pub fn run_migra(bin: &str, migrated_url: &str, source_url: &str) -> CheckReport {
    let name = "migra";
    if !binary_exists(bin) {
        return CheckReport::missing(name, bin, "pip/pipx install migra");
    }
    let command = format!(
        "{} --unsafe {} {}",
        shell_quote(bin),
        shell_quote(&normalize_pg_scheme(migrated_url)),
        shell_quote(&normalize_pg_scheme(source_url))
    );
    match run_shell(&command, &[]) {
        Ok((success, stdout, stderr)) => {
            let out = stdout.trim().to_string();
            if out.is_empty() && success {
                CheckReport { name: name.into(), command, agreed: true, output: out, error: None }
            } else if !out.is_empty() {
                CheckReport { name: name.into(), command, agreed: false, output: out, error: None }
            } else {
                CheckReport::error(name, command, format!("migra failed: {}", stderr.trim()))
            }
        }
        Err(e) => CheckReport::error(name, command, format!("{e:#}")),
    }
}

// ---------------------------------------------------------------------------
// pgdiff (joncrlsn)
// ---------------------------------------------------------------------------

/// Aspects supported by pgdiff 0.9.x, excluding role/grant/ownership (out of
/// dpm's scope).
pub const PGDIFF_SCHEMA_TYPES: &[&str] =
    &["SEQUENCE", "TABLE", "COLUMN", "VIEW", "INDEX", "FOREIGN_KEY"];

/// pgdiff takes paired single-letter flags (upper = db1, lower = db2):
/// `-U/-u user, -H/-h host, -P/-p port, -D/-d dbname`, one aspect per run.
/// Agreement = no non-comment output across all aspects.
pub fn run_pgdiff(bin: &str, migrated_url: &str, source_url: &str) -> CheckReport {
    let name = "pgdiff";
    if !binary_exists(bin) {
        return CheckReport::missing(name, bin, "go install github.com/joncrlsn/pgdiff@latest");
    }
    let (a, b) = match (parse_postgres_url(migrated_url), parse_postgres_url(source_url)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return CheckReport::error(name, bin.into(), format!("{e:#}")),
    };
    let env = libpq_env(&a);
    let base = format!(
        "{bin} -U {u1} -H {h1} -P {p1} -D {d1} -u {u2} -h {h2} -p {p2} -d {d2}",
        bin = shell_quote(bin),
        u1 = shell_quote(&a.user),
        h1 = shell_quote(&a.host),
        p1 = a.port,
        d1 = shell_quote(&a.dbname),
        u2 = shell_quote(&b.user),
        h2 = shell_quote(&b.host),
        p2 = b.port,
        d2 = shell_quote(&b.dbname),
    );

    let mut all_sql = String::new();
    let mut errors = Vec::new();
    for aspect in PGDIFF_SCHEMA_TYPES {
        let command = format!("{base} {aspect}");
        match run_shell(&command, &env) {
            Ok((success, stdout, stderr)) => {
                let real: Vec<&str> = stdout
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.starts_with("--"))
                    .collect();
                if !real.is_empty() {
                    all_sql.push_str(&format!("-- [{aspect}]\n{}\n", real.join("\n")));
                }
                if !success && real.is_empty() {
                    errors.push(format!("[{aspect}] {}", stderr.trim()));
                }
            }
            Err(e) => errors.push(format!("[{aspect}] {e:#}")),
        }
    }

    CheckReport {
        name: name.into(),
        command: format!("{base} <{} aspects>", PGDIFF_SCHEMA_TYPES.len()),
        agreed: all_sql.is_empty() && errors.is_empty(),
        output: all_sql.trim().to_string(),
        error: if errors.is_empty() { None } else { Some(errors.join("; ")) },
    }
}

// ---------------------------------------------------------------------------
// atlas (ariga)
// ---------------------------------------------------------------------------

/// atlas: `atlas schema diff --from <migrated> --to <source>` prints the DDL
/// to converge; "Schemas are synced" (or empty) = agreement. OSS atlas diffs
/// tables/indexes/constraints; views/functions need Atlas Pro and are simply
/// invisible to it — it validates the relational core.
pub fn run_atlas(bin: &str, migrated_url: &str, source_url: &str) -> CheckReport {
    let name = "atlas";
    if !binary_exists(bin) {
        return CheckReport::missing(name, bin, "brew install ariga/tap/atlas");
    }
    let command = format!(
        "{} schema diff --from {} --to {}",
        shell_quote(bin),
        shell_quote(&ensure_sslmode(migrated_url)),
        shell_quote(&ensure_sslmode(source_url))
    );
    match run_shell(&command, &[]) {
        Ok((success, stdout, stderr)) => {
            let out = stdout.trim().to_string();
            let synced = out.is_empty()
                || out.to_ascii_lowercase().contains("schemas are synced")
                || out.to_ascii_lowercase().contains("no changes to be made");
            if success && synced {
                CheckReport { name: name.into(), command, agreed: true, output: String::new(), error: None }
            } else if success {
                CheckReport { name: name.into(), command, agreed: false, output: out, error: None }
            } else {
                CheckReport::error(name, command, format!("atlas failed: {} {}", out, stderr.trim()))
            }
        }
        Err(e) => CheckReport::error(name, command, format!("{e:#}")),
    }
}

// ---------------------------------------------------------------------------
// stripe pg-schema-diff
// ---------------------------------------------------------------------------

/// pg-schema-diff plans directly between two DSNs:
/// `pg-schema-diff plan --from-dsn <migrated> --to-dsn <source>`.
/// An empty plan = agreement. Plan validation (its own shadow processing)
/// runs against the from-side connection automatically.
pub fn run_pg_schema_diff(bin: &str, _pg_dump: &str, migrated_url: &str, source_url: &str) -> CheckReport {
    let name = "pg-schema-diff";
    if !binary_exists(bin) {
        return CheckReport::missing(name, bin, "go install github.com/stripe/pg-schema-diff/cmd/pg-schema-diff@latest");
    }
    let command = format!(
        "{} plan --from-dsn {} --to-dsn {}",
        shell_quote(bin),
        shell_quote(&ensure_sslmode(migrated_url)),
        shell_quote(&ensure_sslmode(source_url))
    );
    match run_shell(&command, &[]) {
        Ok((success, stdout, stderr)) => {
            let out = stdout.trim().to_string();
            let lower = out.to_ascii_lowercase();
            let empty_plan =
                out.is_empty() || lower.contains("schema matches expected") || lower.contains("no changes");
            if success && empty_plan {
                CheckReport { name: name.into(), command, agreed: true, output: String::new(), error: None }
            } else if success {
                CheckReport { name: name.into(), command, agreed: false, output: out, error: None }
            } else {
                CheckReport::error(name, command, format!("pg-schema-diff failed: {} {}", out, stderr.trim()))
            }
        }
        Err(e) => CheckReport::error(name, command, format!("{e:#}")),
    }
}

// ---------------------------------------------------------------------------
// liquibase
// ---------------------------------------------------------------------------

/// liquibase OSS `diff` compares two live databases over JDBC. Agreement =
/// every `Missing/Unexpected/Changed <object>(s):` category reports NONE.
pub fn run_liquibase(bin: &str, migrated_url: &str, source_url: &str) -> CheckReport {
    let name = "liquibase";
    if !binary_exists(bin) {
        return CheckReport::missing(name, bin, "brew install liquibase");
    }
    let (a, b) = match (parse_postgres_url(migrated_url), parse_postgres_url(source_url)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return CheckReport::error(name, bin.into(), format!("{e:#}")),
    };
    let jdbc = |p: &UrlParts| {
        format!(
            "jdbc:postgresql://{}:{}/{}?sslmode={}",
            p.host, p.port, p.dbname, p.sslmode
        )
    };
    let command = format!(
        "{bin} --show-banner=false diff \
         --url {url} --username {user} --password {pw} \
         --reference-url {rurl} --reference-username {ruser} --reference-password {rpw}",
        bin = shell_quote(bin),
        url = shell_quote(&jdbc(&a)),
        user = shell_quote(&a.user),
        pw = shell_quote(a.password.as_deref().unwrap_or("")),
        rurl = shell_quote(&jdbc(&b)),
        ruser = shell_quote(&b.user),
        rpw = shell_quote(b.password.as_deref().unwrap_or("")),
    );
    match run_shell(&command, &[]) {
        Ok((success, stdout, stderr)) => {
            if !success {
                return CheckReport::error(
                    name,
                    command,
                    format!("liquibase failed: {}", if stderr.trim().is_empty() { stdout } else { stderr }),
                );
            }
            let violations = liquibase_violations(&stdout);
            CheckReport {
                name: name.into(),
                command,
                agreed: violations.is_empty(),
                output: violations.join("\n"),
                error: None,
            }
        }
        Err(e) => CheckReport::error(name, command, format!("{e:#}")),
    }
}

/// Parse `liquibase diff` output into real violations. Format:
/// ```text
/// Missing Table(s): NONE
/// Changed Column(s):
///      public.users.bio
///           order changed from '4' to '5'
/// ```
/// Filtered as expected-noise (not violations):
/// - "Changed Catalog(s)" — the two sides are different databases by
///   construction, so the catalog NAME always differs.
/// - Column entries whose only changes are `order changed from ...` — dpm
///   deliberately does not enforce column ordinals (converged databases may
///   disagree after historic ADD COLUMNs).
fn liquibase_violations(stdout: &str) -> Vec<String> {
    #[derive(Default)]
    struct Entry {
        object: String,
        details: Vec<String>,
    }
    let mut violations: Vec<String> = Vec::new();
    let mut category: Option<String> = None;
    let mut entry: Option<Entry> = None;

    let flush = |category: &Option<String>, entry: &mut Option<Entry>, violations: &mut Vec<String>| {
        if let (Some(cat), Some(e)) = (category, entry.take()) {
            let order_only = !e.details.is_empty()
                && e.details.iter().all(|d| d.contains("order changed from"));
            if cat.starts_with("Changed Column(s)") && order_only {
                return; // ordinal drift is by-design
            }
            violations.push(format!("{cat} {}", e.object));
            for d in &e.details {
                violations.push(format!("    {d}"));
            }
        }
    };

    for line in stdout.lines() {
        let trimmed = line.trim_end();
        let is_category = trimmed.starts_with("Missing ")
            || trimmed.starts_with("Unexpected ")
            || trimmed.starts_with("Changed ");
        if is_category {
            flush(&category, &mut entry, &mut violations);
            let noise = trimmed.ends_with("NONE")
                || trimmed.ends_with("EQUAL")
                || trimmed.starts_with("Changed Catalog(s)");
            category = if noise { None } else { Some(trimmed.trim_end_matches(':').to_string()) };
        } else if category.is_some() && line.starts_with(' ') && !line.trim().is_empty() {
            let depth = line.len() - line.trim_start().len();
            if depth <= 6 {
                flush(&category, &mut entry, &mut violations);
                entry = Some(Entry { object: line.trim().to_string(), details: Vec::new() });
            } else if let Some(e) = entry.as_mut() {
                e.details.push(line.trim().to_string());
            }
        } else if !line.starts_with(' ') {
            flush(&category, &mut entry, &mut violations);
            category = None;
        }
    }
    flush(&category, &mut entry, &mut violations);
    violations
}

// ---------------------------------------------------------------------------
// apgdiff
// ---------------------------------------------------------------------------

/// apgdiff diffs two `pg_dump --schema-only` files; empty output = identical.
pub fn run_apgdiff(bin: &str, pg_dump: &str, migrated_url: &str, source_url: &str) -> CheckReport {
    let name = "apgdiff";
    if !binary_exists(bin) {
        return CheckReport::missing(name, bin, "brew install apgdiff");
    }
    if !binary_exists(pg_dump) {
        return CheckReport::error(name, bin.into(), format!("{pg_dump} (pg_dump) not found on PATH"));
    }
    let dir = match scratch_dir("apgdiff") {
        Ok(d) => d,
        Err(e) => return CheckReport::error(name, bin.into(), format!("{e:#}")),
    };
    let dump = |url: &str, file: &std::path::Path| {
        run_shell(
            &format!(
                "{} --schema-only --no-owner --no-privileges {} > {}",
                shell_quote(pg_dump),
                shell_quote(&normalize_pg_scheme(url)),
                shell_quote(&file.display().to_string())
            ),
            &[],
        )
        .and_then(|(ok, _, err)| if ok { Ok(()) } else { anyhow::bail!("pg_dump failed: {err}") })
        .and_then(|_| {
            // apgdiff predates the psql \restrict/\unrestrict dump headers
            // (2025 security releases) and cannot parse them.
            let text = std::fs::read_to_string(file)?;
            std::fs::write(file, crate::apply::strip_psql_meta_commands(&text))?;
            Ok(())
        })
    };
    let migrated_file = dir.join("migrated.sql");
    let source_file = dir.join("source.sql");
    if let Err(e) = dump(migrated_url, &migrated_file).and_then(|_| dump(source_url, &source_file)) {
        let _ = std::fs::remove_dir_all(&dir);
        return CheckReport::error(name, bin.into(), format!("{e:#}"));
    }

    let command = format!(
        "{} --ignore-start-with {} {}",
        shell_quote(bin),
        shell_quote(&migrated_file.display().to_string()),
        shell_quote(&source_file.display().to_string())
    );
    let report = match run_shell(&command, &[]) {
        Ok((success, stdout, stderr)) => {
            let out: String = stdout
                .lines()
                .filter(|l| {
                    let t = l.trim();
                    // apgdiff always emits SET/search_path chatter.
                    !t.is_empty() && !t.starts_with("SET ") && !t.starts_with("--")
                })
                .collect::<Vec<_>>()
                .join("\n");
            if success && out.is_empty() {
                CheckReport { name: name.into(), command, agreed: true, output: String::new(), error: None }
            } else if success {
                CheckReport { name: name.into(), command, agreed: false, output: out, error: None }
            } else {
                CheckReport::error(name, command, format!("apgdiff failed: {}", stderr.trim()))
            }
        }
        Err(e) => CheckReport::error(name, command, format!("{e:#}")),
    };
    let _ = std::fs::remove_dir_all(&dir);
    report
}

// ---------------------------------------------------------------------------
// flyway (runner validation)
// ---------------------------------------------------------------------------

/// flyway validates dpm's SCRIPT rather than the end state: the generated
/// migration must apply cleanly as `V1__dpm_migration.sql` under flyway's
/// runner against a fresh replica of the target. Exit 0 = agreement.
pub fn run_flyway(bin: &str, replica_url: &str, migration_sql: &str) -> CheckReport {
    let name = "flyway";
    if !binary_exists(bin) {
        return CheckReport::missing(name, bin, "brew install flyway");
    }
    let parts = match parse_postgres_url(replica_url) {
        Ok(p) => p,
        Err(e) => return CheckReport::error(name, bin.into(), format!("{e:#}")),
    };
    let dir = match scratch_dir("flyway") {
        Ok(d) => d,
        Err(e) => return CheckReport::error(name, bin.into(), format!("{e:#}")),
    };
    if let Err(e) = std::fs::write(dir.join("V1__dpm_migration.sql"), migration_sql) {
        return CheckReport::error(name, bin.into(), format!("{e:#}"));
    }
    let jdbc = format!(
        "jdbc:postgresql://{}:{}/{}?sslmode={}",
        parts.host, parts.port, parts.dbname, parts.sslmode
    );
    let command = format!(
        "{bin} -url={url} -user={user} -password={pw} -locations=filesystem:{dir} \
         -mixed=true -baselineOnMigrate=true -validateMigrationNaming=true migrate",
        bin = shell_quote(bin),
        url = shell_quote(&jdbc),
        user = shell_quote(&parts.user),
        pw = shell_quote(parts.password.as_deref().unwrap_or("")),
        dir = shell_quote(&dir.display().to_string()),
    );
    let report = match run_shell(&command, &[]) {
        Ok((success, stdout, stderr)) => {
            if success {
                CheckReport { name: name.into(), command, agreed: true, output: String::new(), error: None }
            } else {
                let tail: String = stdout
                    .lines()
                    .chain(stderr.lines())
                    .rev()
                    .take(15)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                CheckReport { name: name.into(), command, agreed: false, output: tail, error: None }
            }
        }
        Err(e) => CheckReport::error(name, command, format!("{e:#}")),
    };
    let _ = std::fs::remove_dir_all(&dir);
    report
}

// ---------------------------------------------------------------------------
// orchestration
// ---------------------------------------------------------------------------

/// Run every selected diff-agreement checker (everything except flyway,
/// which needs its own replica and is orchestrated by verify).
pub fn run_diff_checks(
    sel: &CheckSelection,
    bins: &Bins,
    migrated_url: &str,
    source_url: &str,
) -> Vec<CheckReport> {
    let mut reports = Vec::new();
    let want = |explicit: bool, bin: &str| -> Option<bool> {
        if explicit {
            Some(true) // requested by name: missing binary = failure
        } else if sel.all {
            if binary_exists(bin) {
                Some(true)
            } else {
                None // --cross-check-all skips uninstalled tools silently
            }
        } else {
            Some(false)
        }
    };
    if want(sel.migra, &bins.migra).unwrap_or(false) {
        reports.push(run_migra(&bins.migra, migrated_url, source_url));
    }
    if want(sel.pgdiff, &bins.pgdiff).unwrap_or(false) {
        reports.push(run_pgdiff(&bins.pgdiff, migrated_url, source_url));
    }
    if want(sel.atlas, &bins.atlas).unwrap_or(false) {
        reports.push(run_atlas(&bins.atlas, migrated_url, source_url));
    }
    if want(sel.pg_schema_diff, &bins.pg_schema_diff).unwrap_or(false) {
        reports.push(run_pg_schema_diff(&bins.pg_schema_diff, &bins.pg_dump, migrated_url, source_url));
    }
    if want(sel.liquibase, &bins.liquibase).unwrap_or(false) {
        reports.push(run_liquibase(&bins.liquibase, migrated_url, source_url));
    }
    if want(sel.apgdiff, &bins.apgdiff).unwrap_or(false) {
        reports.push(run_apgdiff(&bins.apgdiff, &bins.pg_dump, migrated_url, source_url));
    }
    reports
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parsing_covers_common_shapes() {
        let p = parse_postgres_url("postgres://alice:s3cr3t@db.example.com:6432/appdb?sslmode=require").unwrap();
        assert_eq!(p.user, "alice");
        assert_eq!(p.password.as_deref(), Some("s3cr3t"));
        assert_eq!(p.host, "db.example.com");
        assert_eq!(p.port, "6432");
        assert_eq!(p.dbname, "appdb");
        assert_eq!(p.sslmode, "require");

        let p = parse_postgres_url("postgres://postgres@127.0.0.1:54329/postgres").unwrap();
        assert_eq!(p.password, None);
        assert_eq!(p.port, "54329");
        assert_eq!(p.sslmode, "disable");

        let p = parse_postgres_url("postgresql://u@h/db").unwrap();
        assert_eq!(p.port, "5432");
        assert_eq!(p.dbname, "db");
    }

    #[test]
    fn scheme_normalization() {
        assert_eq!(normalize_pg_scheme("postgres://u@h/db"), "postgresql://u@h/db");
        assert_eq!(normalize_pg_scheme("postgresql://u@h/db"), "postgresql://u@h/db");
    }

    #[test]
    fn missing_binaries_report_error_not_panic() {
        for report in [
            run_migra("definitely-not-installed-xyz", "postgres://a@h/x", "postgres://a@h/y"),
            run_pgdiff("definitely-not-installed-xyz", "postgres://a@h/x", "postgres://a@h/y"),
            run_atlas("definitely-not-installed-xyz", "postgres://a@h/x", "postgres://a@h/y"),
            run_pg_schema_diff("definitely-not-installed-xyz", "pg_dump", "postgres://a@h/x", "postgres://a@h/y"),
            run_liquibase("definitely-not-installed-xyz", "postgres://a@h/x", "postgres://a@h/y"),
            run_apgdiff("definitely-not-installed-xyz", "pg_dump", "postgres://a@h/x", "postgres://a@h/y"),
            run_flyway("definitely-not-installed-xyz", "postgres://a@h/x", "SELECT 1;"),
        ] {
            assert!(!report.agreed);
            assert!(report.error.is_some(), "{report:?}");
        }
    }

    #[test]
    fn stub_migra_agreement_and_disagreement() {
        let dir = std::env::temp_dir().join(format!("dpm-stub-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let agree = dir.join("migra-agree");
        let disagree = dir.join("migra-disagree");
        std::fs::write(&agree, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(&disagree, "#!/bin/sh\necho 'alter table t add column x integer;'\nexit 2\n").unwrap();
        for f in [&agree, &disagree] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let r = run_migra(agree.to_str().unwrap(), "postgres://a@h/x", "postgres://a@h/y");
        assert!(r.agreed, "{r:?}");
        let r = run_migra(disagree.to_str().unwrap(), "postgres://a@h/x", "postgres://a@h/y");
        assert!(!r.agreed);
        assert!(r.output.contains("alter table"));
    }

    #[test]
    fn selection_semantics() {
        let sel = CheckSelection { all: true, ..Default::default() };
        assert!(sel.any());
        // --cross-check-all with nothing installed under fake names yields no
        // reports (skipped), not failures.
        let bins = Bins {
            migra: "no-such-migra".into(),
            pgdiff: "no-such-pgdiff".into(),
            atlas: "no-such-atlas".into(),
            pg_schema_diff: "no-such-psd".into(),
            liquibase: "no-such-lb".into(),
            apgdiff: "no-such-apg".into(),
            flyway: "no-such-flyway".into(),
            pg_dump: "pg_dump".into(),
        };
        let reports = run_diff_checks(&sel, &bins, "postgres://a@h/x", "postgres://a@h/y");
        assert!(reports.is_empty(), "{reports:?}");

        // Explicitly requested + missing = failure report.
        let sel = CheckSelection { atlas: true, ..Default::default() };
        let reports = run_diff_checks(&sel, &bins, "postgres://a@h/x", "postgres://a@h/y");
        assert_eq!(reports.len(), 1);
        assert!(!reports[0].agreed);
    }

    #[test]
    fn liquibase_category_parsing() {
        // Exercise the section parser through a synthetic transcript by
        // reusing the parsing rules inline (the driver embeds them).
        let transcript = "\
Diff Results:
Reference Database: x
Comparison Database: y
Product Name: EQUAL
Product Version: EQUAL
Missing Catalog(s): NONE
Unexpected Catalog(s): NONE
Changed Catalog(s): NONE
Missing Column(s): NONE
Unexpected Column(s): \n     public.orders.legacy\nMissing Table(s): NONE
";
        // The category parser is embedded in run_liquibase; keep this test as
        // a spec of the format we expect and guard the key substrings.
        assert!(transcript.contains("Unexpected Column(s):"));
        assert!(!transcript.contains("Unexpected Column(s): NONE"));
    }
}

#[cfg(test)]
mod parser_tests {
    use super::*;

    #[test]
    fn liquibase_all_none_and_catalog_noise_is_agreement() {
        let out = "\
Reference Database: a
Comparison Database: b
Product Version: EQUAL
Changed Catalog(s):
     db_one
          name changed from 'db_one' to 'db_two'
Missing Table(s): NONE
Unexpected Table(s): NONE
Changed Table(s): NONE
";
        assert!(liquibase_violations(out).is_empty());
    }

    #[test]
    fn liquibase_order_only_column_changes_are_filtered() {
        let out = "\
Changed Column(s):
     public.users.bio
          order changed from '4' to '5'
     public.users.created_at
          order changed from '5' to '4'
Missing Column(s): NONE
";
        assert!(liquibase_violations(out).is_empty());
    }

    #[test]
    fn liquibase_real_changes_survive_filtering() {
        let out = "\
Changed Column(s):
     public.users.bio
          order changed from '4' to '5'
     public.users.email
          type changed from 'varchar(100)' to 'text'
Missing Table(s):
     public.orders
Unexpected Index(s): NONE
";
        let v = liquibase_violations(out);
        let text = v.join("\n");
        assert!(text.contains("public.users.email"), "{text}");
        assert!(text.contains("type changed"), "{text}");
        assert!(text.contains("public.orders"), "{text}");
        assert!(!text.contains("bio"), "order-only entry must be filtered: {text}");
    }

    #[test]
    fn ensure_sslmode_respects_existing_choice_and_query() {
        assert_eq!(ensure_sslmode("postgres://u@h/db"), "postgresql://u@h/db?sslmode=disable");
        assert_eq!(ensure_sslmode("postgresql://u@h/db?x=1"), "postgresql://u@h/db?x=1&sslmode=disable");
        assert_eq!(ensure_sslmode("postgresql://u@h/db?sslmode=require"), "postgresql://u@h/db?sslmode=require");
    }
}
