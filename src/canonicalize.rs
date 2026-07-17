//! Canonicalize deparsed definition text through the server itself.
//!
//! `pg_get_constraintdef` / `pg_get_indexdef` output is not always a re-parse
//! fixed point. The canonical example is a varchar IN-list: Postgres stores
//! the original parse as `(col)::text = ANY ((ARRAY['a'::character varying,
//! ...])::text[])`, but feeding that emitted text back through the parser
//! stores per-element casts (`ANY (ARRAY[('a'::character varying)::text,
//! ...])`), which deparses differently. The same shape appears in CHECK
//! constraints and in partial-index WHERE predicates. Raw string comparison
//! therefore reports an eternal diff between a freshly-parsed schema and a
//! database built from dpm's own emitted SQL — the same constraint or index
//! is dropped and re-created forever.
//!
//! The fix stays true to the project's core idea (the server is the only
//! trustworthy normalizer — never regex): every CHECK def and index def is
//! rebuilt once against an empty copy of its table in a throwaway shadow
//! database, and the re-read deparse is substituted into the catalog. One
//! round-trip is the fixed point: an already-canonical def re-canonicalizes
//! to itself, so both sides of any diff land on identical strings regardless
//! of how their databases were built. Defs that fail to rebuild (extension
//! types or functions missing on the shadow, etc.) are left untouched —
//! degraded, never wrong: the worst case is the pre-existing behavior.
//!
//! Not yet round-tripped: column defaults and generated expressions (compared
//! via `Column::semantic_eq`), view bodies, and function bodies. If a
//! non-fixed-point shape surfaces for those, this module is where its
//! canonicalization belongs.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use sqlx::Connection;

use crate::introspect;
use crate::model::{
    quote_ident, quote_literal, Catalog, ConstraintKind, DatabaseFlavor, QName, Table,
};
use crate::source::ShadowDb;

/// Rewrite every CHECK-constraint def and index def in `catalogs` to its
/// server-canonical (re-parse fixed point) form, using a throwaway database
/// on `shadow_server_url`. PostgreSQL only; a no-op when no catalog carries a
/// CHECK constraint or index. Identical (column-signature, def) pairs across
/// catalogs are round-tripped once and shared.
pub async fn canonicalize_defs(
    catalogs: &mut [&mut Catalog],
    shadow_server_url: &str,
    verbose: bool,
) -> Result<()> {
    let has_work = catalogs.iter().any(|c| {
        c.tables.values().any(|t| {
            !t.indexes.is_empty()
                || t.constraints
                    .values()
                    .any(|k| k.kind == ConstraintKind::Check)
        })
    });
    if !has_work {
        return Ok(());
    }
    // CockroachDB catalogs deparse through SHOW CREATE and have their own
    // normalization rules; scratch-table DDL below is PostgreSQL-shaped.
    if catalogs
        .iter()
        .any(|c| c.database_flavor != DatabaseFlavor::Postgres)
    {
        return Ok(());
    }

    let scratch = ShadowDb::create(shadow_server_url, verbose).await?;
    if scratch.database_flavor() != DatabaseFlavor::Postgres {
        scratch.drop_db().await;
        return Ok(());
    }
    let result = canonicalize_on(&scratch, catalogs, verbose).await;
    scratch.drop_db().await;
    result
}

async fn canonicalize_on(
    scratch: &ShadowDb,
    catalogs: &mut [&mut Catalog],
    verbose: bool,
) -> Result<()> {
    let mut conn = sqlx::postgres::PgConnection::connect(&scratch.url)
        .await
        .with_context(|| {
            format!(
                "connecting to canonicalization scratch database {}",
                introspect::redact_url(&scratch.url)
            )
        })?;

    // Best-effort environment: user schemas, extensions, and enum types give
    // column definitions something to resolve against. Failures are fine —
    // any def they break simply stays un-canonicalized.
    for catalog in catalogs.iter() {
        for schema in &catalog.schemas {
            let _ = sqlx::raw_sql(&format!(
                "CREATE SCHEMA IF NOT EXISTS {}",
                quote_ident(schema)
            ))
            .execute(&mut conn)
            .await;
        }
        for ext in &catalog.extensions {
            let _ = sqlx::raw_sql(&format!(
                "CREATE EXTENSION IF NOT EXISTS {}",
                quote_ident(ext)
            ))
            .execute(&mut conn)
            .await;
        }
        for (qname, labels) in &catalog.enums {
            let labels_sql = labels
                .iter()
                .map(|l| quote_literal(l))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = sqlx::raw_sql(&format!(
                "CREATE TYPE {}.{} AS ENUM ({labels_sql})",
                quote_ident(&qname.schema),
                quote_ident(&qname.name)
            ))
            .execute(&mut conn)
            .await;
        }
    }

    // (column-signature, def) → canonical def. The signature keys the column
    // environment an expression was parsed against; the same def under the
    // same columns canonicalizes identically across catalogs.
    let mut canonical: BTreeMap<(String, String), String> = BTreeMap::new();

    for catalog in catalogs.iter_mut() {
        for (qname, table) in catalog.tables.iter_mut() {
            let sig = column_signature(table);
            let check_defs: Vec<String> = table
                .constraints
                .values()
                .filter(|c| c.kind == ConstraintKind::Check)
                .map(|c| c.def.clone())
                .filter(|d| !canonical.contains_key(&(sig.clone(), d.clone())))
                .collect();
            let index_defs: Vec<String> = table
                .indexes
                .values()
                .map(|i| i.def.clone())
                .filter(|d| !canonical.contains_key(&(sig.clone(), d.clone())))
                .collect();

            if !check_defs.is_empty() || !index_defs.is_empty() {
                // An empty copy of the table under its real name, so index
                // defs (full CREATE INDEX statements naming the table) and
                // CHECK defs run verbatim. Sequential processing: a same-name
                // table from another catalog is simply rebuilt.
                match create_scratch_table(&mut conn, qname, table).await {
                    Ok(()) => {
                        for def in check_defs {
                            let canon = round_trip_check(&mut conn, qname, &def)
                                .await
                                .unwrap_or_else(|e| {
                                    if verbose {
                                        eprintln!(
                                            "dpm: canonicalize: CHECK def did not round-trip \
                                             ({e:#}); comparing it verbatim: {def}"
                                        );
                                    }
                                    def.clone()
                                });
                            canonical.insert((sig.clone(), def), canon);
                        }
                        for def in index_defs {
                            let canon = round_trip_index(&mut conn, qname, &def)
                                .await
                                .unwrap_or_else(|e| {
                                    if verbose {
                                        eprintln!(
                                            "dpm: canonicalize: index def did not round-trip \
                                             ({e:#}); comparing it verbatim: {def}"
                                        );
                                    }
                                    def.clone()
                                });
                            canonical.insert((sig.clone(), def), canon);
                        }
                    }
                    Err(e) => {
                        if verbose {
                            eprintln!(
                                "dpm: canonicalize: scratch copy of {}.{} failed ({e:#}); \
                                 leaving its defs as-is",
                                qname.schema, qname.name
                            );
                        }
                        for d in check_defs.into_iter().chain(index_defs) {
                            canonical.insert((sig.clone(), d.clone()), d);
                        }
                    }
                }
            }

            for con in table.constraints.values_mut() {
                if con.kind == ConstraintKind::Check {
                    if let Some(canon) = canonical.get(&(sig.clone(), con.def.clone())) {
                        con.def = canon.clone();
                    }
                }
            }
            for idx in table.indexes.values_mut() {
                if let Some(canon) = canonical.get(&(sig.clone(), idx.def.clone())) {
                    idx.def = canon.clone();
                }
            }
        }
    }
    let _ = conn.close().await;
    Ok(())
}

fn qualified(qname: &QName) -> String {
    format!(
        "{}.{}",
        quote_ident(&qname.schema),
        quote_ident(&qname.name)
    )
}

/// Create an empty, constraint-free copy of the table under its real name
/// (types and collations only — enough to parse expressions against).
async fn create_scratch_table(
    conn: &mut sqlx::postgres::PgConnection,
    qname: &QName,
    table: &Table,
) -> Result<()> {
    let target = qualified(qname);
    sqlx::raw_sql(&format!("DROP TABLE IF EXISTS {target} CASCADE"))
        .execute(&mut *conn)
        .await
        .context("dropping previous scratch copy")?;
    let cols = table
        .columns
        .iter()
        .map(|c| {
            let mut d = format!("{} {}", quote_ident(&c.name), c.type_sql);
            if let Some(coll) = &c.collation {
                d.push_str(&format!(" COLLATE {coll}"));
            }
            d
        })
        .collect::<Vec<_>>()
        .join(", ");
    sqlx::raw_sql(&format!("CREATE TABLE {target} ({cols})"))
        .execute(&mut *conn)
        .await
        .context("creating scratch copy")?;
    Ok(())
}

/// Add the CHECK def to the scratch copy, read back the server's deparse,
/// drop it again.
async fn round_trip_check(
    conn: &mut sqlx::postgres::PgConnection,
    qname: &QName,
    def: &str,
) -> Result<String> {
    let target = qualified(qname);
    sqlx::raw_sql(&format!(
        "ALTER TABLE {target} ADD CONSTRAINT _dpm_canon_check {def}"
    ))
    .execute(&mut *conn)
    .await
    .context("ADD CONSTRAINT failed")?;
    let row: (String,) = sqlx::query_as(
        "SELECT pg_catalog.pg_get_constraintdef(oid) FROM pg_catalog.pg_constraint \
         WHERE conname = '_dpm_canon_check' AND conrelid = ($1::text)::regclass",
    )
    .bind(&target)
    .fetch_one(&mut *conn)
    .await
    .context("reading back canonical constraint def")?;
    sqlx::raw_sql(&format!(
        "ALTER TABLE {target} DROP CONSTRAINT _dpm_canon_check"
    ))
    .execute(&mut *conn)
    .await
    .context("DROP CONSTRAINT failed")?;
    Ok(row.0)
}

/// Execute the index def (a complete CREATE INDEX statement) against the
/// scratch copy, read back `pg_get_indexdef`, drop the index again.
async fn round_trip_index(
    conn: &mut sqlx::postgres::PgConnection,
    qname: &QName,
    def: &str,
) -> Result<String> {
    sqlx::raw_sql(def)
        .execute(&mut *conn)
        .await
        .context("CREATE INDEX failed")?;
    let row: (String, String) = sqlx::query_as(
        "SELECT quote_ident(n.nspname) || '.' || quote_ident(c.relname), \
                pg_catalog.pg_get_indexdef(i.indexrelid) \
         FROM pg_catalog.pg_index i \
         JOIN pg_catalog.pg_class c ON c.oid = i.indexrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE i.indrelid = ($1::text)::regclass \
         ORDER BY i.indexrelid DESC LIMIT 1",
    )
    .bind(qualified(qname))
    .fetch_one(&mut *conn)
    .await
    .context("reading back canonical index def")?;
    sqlx::raw_sql(&format!("DROP INDEX {}", row.0))
        .execute(&mut *conn)
        .await
        .context("DROP INDEX failed")?;
    Ok(row.1)
}

/// Identity of the column environment an expression parses against.
fn column_signature(table: &Table) -> String {
    let mut parts: Vec<String> = table
        .columns
        .iter()
        .map(|c| {
            format!(
                "{}\u{1}{}\u{1}{}",
                c.name,
                c.type_sql,
                c.collation.as_deref().unwrap_or("")
            )
        })
        .collect();
    parts.sort();
    parts.join("\u{2}")
}

#[cfg(test)]
mod tests {
    use super::column_signature;
    use crate::model::{Column, Table};

    fn col(name: &str, type_sql: &str) -> Column {
        Column {
            name: name.into(),
            type_sql: type_sql.into(),
            not_null: false,
            default: None,
            identity: None,
            generated: None,
            is_serial: false,
            collation: None,
            hidden: false,
        }
    }

    #[test]
    fn signature_ignores_column_order_but_not_types() {
        let a = Table {
            columns: vec![col("x", "text"), col("y", "integer")],
            constraints: Default::default(),
            indexes: Default::default(),
            partition_by: None,
            rls_enabled: false,
            rls_forced: false,
            policies: Default::default(),
        };
        let mut b = a.clone();
        b.columns.reverse();
        assert_eq!(column_signature(&a), column_signature(&b));
        let mut c = a.clone();
        c.columns[0].type_sql = "character varying(32)".into();
        assert_ne!(column_signature(&a), column_signature(&c));
    }
}
