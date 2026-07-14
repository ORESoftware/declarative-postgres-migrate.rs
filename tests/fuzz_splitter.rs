//! Property-based tests for the SQL statement splitter and the psql
//! meta-command stripper — the two hand-rolled scanners everything else
//! trusts. Invariants over arbitrary inputs, plus round-trips over
//! generated well-formed scripts.

use dpm::apply::{split_statements, strip_psql_meta_commands};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// The splitter must never panic, whatever bytes arrive.
    #[test]
    fn splitter_never_panics(input in "\\PC*") {
        let _ = split_statements(&input);
    }

    /// Statement content is preserved: concatenating the split output
    /// contains every non-quote character run of the input statements.
    #[test]
    fn simple_statement_lists_round_trip(
        stmts in proptest::collection::vec("[a-zA-Z0-9_ ]{1,40}", 1..12)
    ) {
        let script: String = stmts.iter().map(|s| format!("{s};")).collect();
        let out = split_statements(&script);
        let expected: Vec<String> = stmts.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        prop_assert_eq!(out.iter().map(|s| s.trim().to_string()).collect::<Vec<_>>(), expected);
    }

    /// Semicolons inside single-quoted strings never split statements.
    #[test]
    fn quoted_semicolons_never_split(payload in "[a-z;]{0,30}") {
        let escaped = payload.replace('\'', "''");
        let script = format!("INSERT INTO t VALUES ('{escaped}');SELECT 1;");
        let out = split_statements(&script);
        prop_assert_eq!(out.len(), 2, "script: {}", script);
        prop_assert!(out[0].contains("INSERT"));
    }

    /// Dollar-quoted bodies are opaque: whatever is inside (semicolons,
    /// quotes, fake meta-commands) stays in one statement.
    #[test]
    fn dollar_quoted_bodies_are_opaque(body in "[a-zA-Z0-9;'\" \\\\.\n]{0,80}") {
        // Choose a tag that cannot appear in the body.
        let script = format!("CREATE FUNCTION f() AS $dpmtag${body}$dpmtag$ LANGUAGE sql;SELECT 2;");
        let out = split_statements(&script);
        prop_assert_eq!(out.len(), 2, "script: {}", script);
        prop_assert!(out[0].contains(&body), "body lost");
    }

    /// The meta stripper never panics and is idempotent.
    #[test]
    fn meta_strip_never_panics_and_is_idempotent(input in "\\PC*") {
        let once = strip_psql_meta_commands(&input);
        let twice = strip_psql_meta_commands(&once);
        prop_assert_eq!(once, twice);
    }

    /// Stripping only ever removes whole lines that start with a backslash;
    /// every other line survives byte-identically.
    #[test]
    fn meta_strip_preserves_non_meta_lines(lines in proptest::collection::vec("[a-zA-Z0-9 ;']{0,30}", 0..10)) {
        let input: String = lines.iter().map(|l| format!("{l}\n")).collect();
        // No line starts with '\' by construction, and generated quotes are
        // balanced per line only by chance — quote state can make later
        // lines "inside a string", which is fine: nothing is a meta line.
        let out = strip_psql_meta_commands(&input);
        prop_assert_eq!(out, input);
    }

    /// Splitting is stable: split(join(split(x))) == split(x) for scripts
    /// built from safe statements.
    #[test]
    fn splitting_is_idempotent(stmts in proptest::collection::vec("[a-zA-Z0-9_, ()=<>]{1,60}", 1..8)) {
        let script: String = stmts.iter().map(|s| format!("{s};\n")).collect();
        let first = split_statements(&script);
        let rejoined: String = first.iter().map(|s| format!("{s};\n")).collect();
        let second = split_statements(&rejoined);
        prop_assert_eq!(first, second);
    }
}
