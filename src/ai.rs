//! AI-tool review of generated migrations.
//!
//! dpm builds a self-contained review payload (instructions + flag context +
//! plan summary + the full SQL script), writes it to a temp file, and drives
//! a coding-agent CLI non-interactively:
//!
//!   claude   →  `claude -p < {file}`          (Claude Code print mode)
//!   codex    →  `codex exec - < {file}`       (Codex CLI, prompt from stdin)
//!   chatgpt  →  alias of codex
//!   gemini   →  `gemini < {file}`             (Gemini CLI, stdin prompt)
//!   custom   →  the `--ai-cmd` template, `{file}` expands to the payload path
//!
//! The payload instructs the reviewer to end with a machine-parseable verdict
//! line:
//!
//!   DPM_VERDICT: APPROVE
//!   DPM_VERDICT: REJECT <one-line reason>
//!
//! dpm parses the LAST verdict line from stdout. No verdict line counts as a
//! rejection (fail closed) — a reviewer that crashed or rambled must not gate
//! a migration open.

use anyhow::{bail, Context, Result};

#[derive(Clone, Debug)]
pub struct ReviewRequest {
    /// The migration script under review.
    pub sql: String,
    /// JSON plan (typed change list) for structured cross-checking.
    pub plan_json: String,
    pub source_desc: String,
    pub target_desc: String,
    /// Flag context so the reviewer can flag policy violations (e.g. live
    /// destructive SQL when the operator did not allow it).
    pub allow_destructive_sql: bool,
    pub allow_destructive_ops: bool,
    /// Summary counts from emission.
    pub total_changes: usize,
    pub destructive_changes: usize,
    pub gated_changes: usize,
    pub manual_changes: usize,
}

#[derive(Clone, Debug)]
pub struct ReviewOutcome {
    pub approved: bool,
    /// The parsed verdict line, if any.
    pub verdict: Option<String>,
    /// Full reviewer stdout (the reasoning transcript).
    pub transcript: String,
    /// The command that was run.
    pub command: String,
}

/// Resolve a tool name to its shell command template. `{file}` is replaced
/// with the payload path.
pub fn tool_command_template(tool: &str, custom_cmd: Option<&str>) -> Result<String> {
    if let Some(cmd) = custom_cmd {
        if !cmd.trim().is_empty() {
            return Ok(cmd.to_string());
        }
    }
    Ok(match tool.to_ascii_lowercase().as_str() {
        "claude" => "claude -p < {file}".to_string(),
        "codex" | "chatgpt" | "openai" => "codex exec - < {file}".to_string(),
        "gemini" => "gemini < {file}".to_string(),
        "custom" => bail!("--ai-tool custom requires --ai-cmd (DPM_AI_CMD)"),
        other => bail!("unknown --ai-tool {other:?}: expected claude | codex | chatgpt | gemini | custom"),
    })
}

pub fn build_payload(req: &ReviewRequest) -> String {
    let destructive_policy = match (req.allow_destructive_sql, req.allow_destructive_ops) {
        (false, _) => {
            "Destructive SQL generation is NOT allowed: every destructive statement must appear \
             commented out (gated). Any LIVE destructive statement is a policy violation — REJECT."
        }
        (true, false) => {
            "Destructive SQL generation IS allowed (statements may appear live), but executing \
             them is NOT yet approved; the operator will need --allow-destructive-ops at apply \
             time. Judge the SQL on correctness and safety."
        }
        (true, true) => {
            "Destructive SQL generation AND execution are both operator-approved. Still verify \
             each destructive statement is intentional given the plan, and flag anything that \
             looks like collateral damage."
        }
    };

    format!(
        r#"You are reviewing an auto-generated PostgreSQL schema migration produced by
declarative-postgres-migrate (dpm). The script converges a target database onto a
desired source state. Review it for CORRECTNESS, CONSISTENCY, and SAFETY. Do not
suggest stylistic rewrites; judge only whether this script is safe and correct to run.

CONTEXT
- desired (source): {source}
- current (target): {target}
- plan summary: {total} change(s), {destructive} destructive ({gated} gated/commented), {manual} manual-review item(s)
- destructive policy: {destructive_policy}

CHECKLIST
1. Statement ordering: types/tables/columns exist before anything references them;
   FKs added after referenced tables and their PK/unique constraints; drops run
   dependents-first; enum ADD VALUE statements appear before BEGIN (outside the
   transaction).
2. Destructive audit: list every statement that can lose data or weaken integrity
   (DROP TABLE/COLUMN/TYPE/SEQUENCE/FUNCTION, integrity-weakening constraint/index
   drops, column type changes that can truncate). Check each against the destructive
   policy above.
3. Consistency: the SQL must match the JSON plan — no statement without a plan entry,
   no plan entry without a statement (gated entries appear as comments).
4. Safety: no statement may target objects outside the declared plan; no data-modifying
   DML (INSERT/UPDATE/DELETE) except sequence setval for serial adoption; nothing that
   looks like injection or an unrelated side effect.

OUTPUT FORMAT (mandatory)
- Brief findings, most severe first. If everything is fine say so in one line.
- Then, as the FINAL line, exactly one verdict:
  DPM_VERDICT: APPROVE
  or
  DPM_VERDICT: REJECT <one-line reason>

=== JSON PLAN ===
{plan_json}

=== MIGRATION SQL ===
{sql}
"#,
        source = req.source_desc,
        target = req.target_desc,
        total = req.total_changes,
        destructive = req.destructive_changes,
        gated = req.gated_changes,
        manual = req.manual_changes,
        destructive_policy = destructive_policy,
        plan_json = req.plan_json,
        sql = req.sql,
    )
}

/// Parse the last `DPM_VERDICT:` line from a reviewer transcript.
pub fn parse_verdict(transcript: &str) -> Option<(bool, String)> {
    transcript
        .lines()
        .rev()
        .map(str::trim)
        .find_map(|line| {
            let rest = line.strip_prefix("DPM_VERDICT:")?.trim();
            if rest.eq_ignore_ascii_case("APPROVE") || rest.to_ascii_uppercase().starts_with("APPROVE") {
                Some((true, line.to_string()))
            } else if rest.to_ascii_uppercase().starts_with("REJECT") {
                Some((false, line.to_string()))
            } else {
                None
            }
        })
}

pub fn run_review(tool: &str, custom_cmd: Option<&str>, req: &ReviewRequest, verbose: bool) -> Result<ReviewOutcome> {
    let template = tool_command_template(tool, custom_cmd)?;
    let payload = build_payload(req);

    let dir = std::env::temp_dir().join("dpm-ai-review");
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("payload-{}-{}.md", std::process::id(), req.total_changes));
    std::fs::write(&file, &payload).with_context(|| format!("writing {}", file.display()))?;

    let command = template.replace("{file}", &file.display().to_string());
    if verbose {
        eprintln!("dpm: ai review: {command}");
    }
    // The reviewer is an independent non-interactive call; strip Claude
    // Code's nesting guard so `dpm review` works when dpm itself is being
    // driven from inside a Claude Code session (the guard exists for
    // interactive sessions sharing runtime resources, and its own error
    // message documents this bypass).
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(&command)
        .env_remove("CLAUDECODE")
        .output()
        .with_context(|| format!("running AI reviewer: {command}"))?;
    let _ = std::fs::remove_file(&file);

    let transcript = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() {
        return Ok(ReviewOutcome {
            approved: false,
            verdict: Some(format!("reviewer exited {}", output.status)),
            transcript,
            command,
        });
    }

    // Fail closed: a transcript without a parseable verdict is not approval.
    let parsed = parse_verdict(&transcript);
    Ok(ReviewOutcome {
        approved: parsed.as_ref().map(|(ok, _)| *ok).unwrap_or(false),
        verdict: parsed.map(|(_, line)| line),
        transcript,
        command,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> ReviewRequest {
        ReviewRequest {
            sql: "BEGIN;\nSELECT 1;\nCOMMIT;".into(),
            plan_json: "[]".into(),
            source_desc: "a".into(),
            target_desc: "b".into(),
            allow_destructive_sql: false,
            allow_destructive_ops: false,
            total_changes: 1,
            destructive_changes: 0,
            gated_changes: 0,
            manual_changes: 0,
        }
    }

    #[test]
    fn verdict_parsing_takes_last_line_and_fails_closed() {
        assert_eq!(parse_verdict("blah\nDPM_VERDICT: APPROVE\n"), Some((true, "DPM_VERDICT: APPROVE".into())));
        let (ok, line) = parse_verdict("DPM_VERDICT: APPROVE\nlater...\nDPM_VERDICT: REJECT drops users table").unwrap();
        assert!(!ok);
        assert!(line.contains("REJECT"));
        assert_eq!(parse_verdict("no verdict here"), None);
        // Indented verdict still parses.
        assert!(parse_verdict("  DPM_VERDICT: APPROVE  ").unwrap().0);
    }

    #[test]
    fn tool_templates() {
        assert!(tool_command_template("claude", None).unwrap().starts_with("claude -p"));
        assert!(tool_command_template("chatgpt", None).unwrap().starts_with("codex exec"));
        assert!(tool_command_template("gemini", None).unwrap().starts_with("gemini"));
        assert!(tool_command_template("custom", None).is_err());
        assert_eq!(tool_command_template("custom", Some("x {file}")).unwrap(), "x {file}");
        // --ai-cmd overrides even a named tool.
        assert_eq!(tool_command_template("claude", Some("y {file}")).unwrap(), "y {file}");
        assert!(tool_command_template("skynet", None).is_err());
    }

    #[test]
    fn payload_contains_policy_and_sections() {
        let payload = build_payload(&req());
        assert!(payload.contains("policy violation — REJECT"));
        assert!(payload.contains("=== MIGRATION SQL ==="));
        assert!(payload.contains("DPM_VERDICT: APPROVE"));
    }

    #[test]
    fn fake_reviewer_end_to_end() {
        // A stand-in "AI" that approves; exercises payload write + shell + parse.
        let outcome = run_review(
            "custom",
            Some("cat {file} > /dev/null && echo 'looks good' && echo 'DPM_VERDICT: APPROVE'"),
            &req(),
            false,
        )
        .unwrap();
        assert!(outcome.approved, "transcript: {}", outcome.transcript);

        let outcome = run_review(
            "custom",
            Some("echo 'DPM_VERDICT: REJECT live destructive statement found'"),
            &req(),
            false,
        )
        .unwrap();
        assert!(!outcome.approved);

        // Reviewer that says nothing useful → fail closed.
        let outcome = run_review("custom", Some("echo hello"), &req(), false).unwrap();
        assert!(!outcome.approved);
        assert!(outcome.verdict.is_none());
    }
}
