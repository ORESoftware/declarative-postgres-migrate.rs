//! Execute a generated migration script against a target database.
//!
//! Scripts are split into individual statements (dollar-quote, string, and
//! comment aware) and executed one at a time over the simple protocol. This
//! matters for two reasons:
//! - `ALTER TYPE ... ADD VALUE` must run outside any transaction; sending the
//!   whole script as one batch would wrap it in an implicit transaction.
//! - Per-statement errors can point at the exact failing SQL.
//!
//! The script's own `BEGIN;` / `COMMIT;` statements provide the transaction
//! boundary for everything that belongs inside one.

use anyhow::{Context, Result};
use sqlx::{Connection, PgConnection};

/// Split SQL text into executable statements. Handles:
/// - single-quoted strings (with `''` escapes)
/// - double-quoted identifiers
/// - dollar-quoted bodies (`$$ ... $$`, `$tag$ ... $tag$`)
/// - line comments (`-- ...`) and block comments (`/* ... */`, nested)
pub fn split_statements(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut statements = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let n = bytes.len();

    while i < n {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < n {
                    if bytes[i] == b'\'' {
                        if i + 1 < n && bytes[i + 1] == b'\'' {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'"' => {
                i += 1;
                while i < n && bytes[i] != b'"' {
                    i += 1;
                }
                i += 1;
            }
            b'-' if i + 1 < n && bytes[i + 1] == b'-' => {
                while i < n && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < n && bytes[i + 1] == b'*' => {
                let mut depth = 1;
                i += 2;
                while i < n && depth > 0 {
                    if i + 1 < n && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                    } else if i + 1 < n && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            b'$' => {
                // Possible dollar-quote opener: $tag$ where tag is
                // [A-Za-z_][A-Za-z0-9_]* or empty.
                let tag_start = i + 1;
                let mut j = tag_start;
                while j < n && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                if j < n && bytes[j] == b'$' {
                    let tag = &sql[i..=j];
                    if let Some(close) = sql[j + 1..].find(tag) {
                        i = j + 1 + close + tag.len();
                    } else {
                        i = n;
                    }
                } else {
                    i += 1;
                }
            }
            b';' => {
                let stmt = sql[start..i].trim();
                if !only_comments(stmt) {
                    statements.push(stmt.to_string());
                }
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    let tail = sql[start..].trim();
    if !only_comments(tail) {
        statements.push(tail.to_string());
    }
    statements
}

/// True when the fragment contains no executable tokens (only whitespace and
/// comments) and therefore should not be sent to the server.
fn only_comments(fragment: &str) -> bool {
    let mut rest = fragment.trim_start();
    loop {
        if rest.is_empty() {
            return true;
        }
        if let Some(after) = rest.strip_prefix("--") {
            rest = match after.find('\n') {
                Some(pos) => after[pos + 1..].trim_start(),
                None => "",
            };
            continue;
        }
        if let Some(after) = rest.strip_prefix("/*") {
            rest = match after.find("*/") {
                Some(pos) => after[pos + 2..].trim_start(),
                None => "",
            };
            continue;
        }
        return false;
    }
}

/// Remove psql meta-command lines (`\restrict`, `\unrestrict`, `\connect`,
/// `\.`, ...) that appear in `pg_dump` output. Only lines *outside* quoted
/// regions are touched, so a backslash-leading line inside a dollar-quoted
/// function body survives. Recent pg_dump versions (2025 security releases)
/// emit `\restrict`/`\unrestrict` unconditionally, so this is required for
/// "diff two dumps" workflows.
pub fn strip_psql_meta_commands(sql: &str) -> String {
    // Mark which byte offsets are inside a quoted region using the same
    // scanner rules as split_statements.
    let bytes = sql.as_bytes();
    let n = bytes.len();
    let mut in_quote = vec![false; n];
    let mut i = 0usize;
    while i < n {
        match bytes[i] {
            b'\'' => {
                let start = i;
                i += 1;
                while i < n {
                    if bytes[i] == b'\'' {
                        if i + 1 < n && bytes[i + 1] == b'\'' {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i = (i + 1).min(n);
                in_quote[start..i.min(n)].fill(true);
            }
            b'"' => {
                let start = i;
                i += 1;
                while i < n && bytes[i] != b'"' {
                    i += 1;
                }
                i = (i + 1).min(n);
                in_quote[start..i.min(n)].fill(true);
            }
            b'$' => {
                let tag_start = i + 1;
                let mut j = tag_start;
                while j < n && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                if j < n && bytes[j] == b'$' {
                    let tag = &sql[i..=j];
                    let start = i;
                    if let Some(close) = sql[j + 1..].find(tag) {
                        i = j + 1 + close + tag.len();
                    } else {
                        i = n;
                    }
                    in_quote[start..i.min(n)].fill(true);
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }

    let mut out = String::with_capacity(sql.len());
    let mut offset = 0usize;
    for line in sql.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let trimmed = line.trim_start();
        let is_meta = trimmed.starts_with('\\') && !in_quote.get(line_start).copied().unwrap_or(false);
        if !is_meta {
            out.push_str(line);
        }
    }
    out
}

/// True for statements that depend on roles/ownership and are skipped when
/// materializing a dump into a shadow database (dpm does not diff ownership
/// or grants; a fresh shadow db lacks the production roles).
pub fn is_role_dependent_statement(stmt: &str) -> bool {
    let upper = stmt.trim_start().to_ascii_uppercase();
    upper.starts_with("GRANT ")
        || upper.starts_with("REVOKE ")
        || upper.starts_with("SET SESSION AUTHORIZATION")
        || upper.starts_with("SET ROLE")
        || (upper.starts_with("ALTER ") && upper.contains(" OWNER TO "))
}

pub fn truncate_sql(stmt: &str) -> String {
    const MAX: usize = 500;
    if stmt.len() <= MAX {
        stmt.to_string()
    } else {
        format!("{}… [{} bytes]", &stmt[..MAX], stmt.len())
    }
}

#[derive(Debug)]
pub struct ApplyReport {
    pub executed: usize,
}

/// Execute a script statement-by-statement. On error the connection has
/// whatever transaction state the script left; we attempt a ROLLBACK so the
/// error surfaces cleanly.
pub async fn apply_script(url: &str, sql: &str) -> Result<ApplyReport> {
    let mut conn = PgConnection::connect(url)
        .await
        .with_context(|| format!("connecting to {}", crate::introspect::redact_url(url)))?;
    let statements = split_statements(sql);
    let mut executed = 0usize;
    for (i, stmt) in statements.iter().enumerate() {
        if let Err(err) = sqlx::raw_sql(stmt).execute(&mut conn).await {
            let _ = sqlx::raw_sql("ROLLBACK").execute(&mut conn).await;
            let _ = conn.close().await;
            return Err(anyhow::anyhow!(err)).with_context(|| {
                format!(
                    "statement {}/{} failed:\n{}",
                    i + 1,
                    statements.len(),
                    truncate_sql(stmt)
                )
            });
        }
        executed += 1;
    }
    let _ = conn.close().await;
    Ok(ApplyReport { executed })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_simple_statements() {
        let stmts = split_statements("CREATE TABLE a (id int);\nDROP TABLE b;\n");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[1], "DROP TABLE b");
    }

    #[test]
    fn semicolons_in_strings_and_dollar_quotes_do_not_split() {
        let sql = r#"
INSERT INTO t VALUES ('a;b');
CREATE FUNCTION f() RETURNS trigger AS $fn$
BEGIN
  PERFORM 1; PERFORM 2;
  RETURN NEW;
END;
$fn$ LANGUAGE plpgsql;
SELECT 1;
"#;
        let stmts = split_statements(sql);
        assert_eq!(stmts.len(), 3, "got: {stmts:#?}");
        assert!(stmts[1].contains("PERFORM 2;"));
    }

    #[test]
    fn comments_with_semicolons_are_ignored() {
        let sql = "-- gated: DROP TABLE x;\n/* also; not this */\nSELECT 1;\n-- trailing comment\n";
        let stmts = split_statements(sql);
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].contains("SELECT 1"));
    }

    #[test]
    fn nested_block_comments() {
        let sql = "/* outer /* inner; */ still; */ SELECT 2;";
        let stmts = split_statements(sql);
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].ends_with("SELECT 2"));
    }

    #[test]
    fn dollar_tag_mismatch_does_not_close_early() {
        let sql = "CREATE FUNCTION g() RETURNS text AS $a$ x $b$ y $a$ LANGUAGE sql;";
        let stmts = split_statements(sql);
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn strips_psql_meta_commands_outside_quotes_only() {
        let sql = "\\restrict abc123\nSET client_encoding = 'UTF8';\n\\unrestrict abc123\nCREATE FUNCTION f() RETURNS text AS $b$\n\\not-a-meta-command\n$b$ LANGUAGE sql;\n\\.\n";
        let cleaned = strip_psql_meta_commands(sql);
        assert!(!cleaned.contains("\\restrict"));
        assert!(!cleaned.contains("\\unrestrict"));
        assert!(!cleaned.contains("\\.\n"));
        assert!(cleaned.contains("\\not-a-meta-command"), "backslash line inside dollar quotes must survive");
        assert!(cleaned.contains("SET client_encoding"));
    }

    #[test]
    fn role_dependent_statements_are_recognized() {
        assert!(is_role_dependent_statement("GRANT ALL ON TABLE public.t TO app_user"));
        assert!(is_role_dependent_statement("REVOKE ALL ON SCHEMA public FROM PUBLIC"));
        assert!(is_role_dependent_statement("ALTER TABLE public.users OWNER TO produser"));
        assert!(is_role_dependent_statement("  alter function public.f() owner to produser"));
        assert!(is_role_dependent_statement("SET SESSION AUTHORIZATION 'x'"));
        assert!(!is_role_dependent_statement("CREATE TABLE t (id int)"));
        assert!(!is_role_dependent_statement("ALTER TABLE t ADD COLUMN owner_to text"));
    }
}

#[cfg(test)]
mod splitter_edge_tests {
    use super::*;

    #[test]
    fn empty_and_comment_only_inputs_yield_no_statements() {
        assert!(split_statements("").is_empty());
        assert!(split_statements("   \n\t\n").is_empty());
        assert!(split_statements("-- just a comment\n/* and a block */\n").is_empty());
    }

    #[test]
    fn trailing_statement_without_semicolon_is_kept() {
        let stmts = split_statements("SELECT 1;\nSELECT 2");
        assert_eq!(stmts, vec!["SELECT 1".to_string(), "SELECT 2".to_string()]);
    }

    #[test]
    fn unterminated_dollar_quote_does_not_panic_or_split() {
        let stmts = split_statements("CREATE FUNCTION f() AS $x$ BEGIN; never closed");
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn quoted_identifiers_with_semicolons_do_not_split() {
        let stmts = split_statements("CREATE TABLE \"we;ird\" (id int);SELECT 1;");
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("we;ird"));
    }

    #[test]
    fn meta_strip_preserves_backslash_lines_inside_strings() {
        let sql = "INSERT INTO t VALUES ('line1\n\\. not a meta terminator\nline3');\n\\.\n";
        let cleaned = strip_psql_meta_commands(sql);
        assert!(cleaned.contains("\\. not a meta terminator"), "inside string must survive");
        assert!(!cleaned.trim_end().ends_with("\\."), "top-level \\. removed");
    }

    #[test]
    fn role_statement_detection_is_not_overeager() {
        assert!(!is_role_dependent_statement("CREATE TABLE grants (id int)"));
        assert!(!is_role_dependent_statement("COMMENT ON TABLE t IS 'GRANT nothing'"));
        assert!(is_role_dependent_statement("\n  GRANT SELECT ON t TO r"));
    }
}
