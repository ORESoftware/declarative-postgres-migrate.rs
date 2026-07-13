//! The diff engine: compare two [`Catalog`]s and produce a typed change plan
//! that converges `target` onto `source` (source = desired state, target =
//! what exists). Pure — no I/O — so it is unit-testable and reusable from the
//! library API.
//!
//! Every change knows whether it is destructive (loses data or weakens
//! integrity in a way the script itself does not restore). Emission decides
//! how destructive changes are rendered (live vs commented) — the diff only
//! classifies.

use serde::Serialize;

use crate::model::*;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Change {
    CreateSchema { name: String },
    CreateExtension { name: String },
    DropExtension { name: String },

    CreateEnum { ty: QName, labels: Vec<String> },
    /// `before == None` means append at the end.
    AddEnumValue { ty: QName, value: String, before: Option<String> },
    /// Labels were removed or reordered: converging requires a full type
    /// rebuild with column rewrites, which dpm will not attempt automatically.
    EnumNeedsRebuild { ty: QName, source_labels: Vec<String>, target_labels: Vec<String> },
    DropEnum { ty: QName },

    CreateSequence { seq: QName, def: Sequence },
    AlterSequence { seq: QName, def: Sequence },
    DropSequence { seq: QName },

    CreateTable { table: QName, def: Table },
    DropTable { table: QName },

    AddColumn { table: QName, def: Column },
    DropColumn { table: QName, column: String },
    AlterColumnType { table: QName, column: String, from: String, to: String, collation: Option<String> },
    SetNotNull { table: QName, column: String },
    DropNotNull { table: QName, column: String },
    SetDefault { table: QName, column: String, expr: String },
    DropDefault { table: QName, column: String },
    /// Convert a plain column into a serial-style column: create the owned
    /// sequence, set the nextval default, transfer ownership.
    MakeSerial { table: QName, column: String, type_sql: String },
    /// Remove a serial default (and its now-orphaned sequence — the sequence
    /// drop is the destructive half).
    DropSerial { table: QName, column: String },
    AddIdentity { table: QName, column: String, kind: IdentityKind },
    DropIdentity { table: QName, column: String },
    SetIdentityKind { table: QName, column: String, kind: IdentityKind },
    /// Generation expressions cannot be altered in place; converging means
    /// drop + re-add the column (data in the target column is lost).
    RegenerateColumn { table: QName, def: Column },

    AddConstraint { table: QName, name: String, def: String, kind: ConstraintKind, table_is_new: bool },
    DropConstraint { table: QName, name: String, kind: ConstraintKind, replaced: bool },

    CreateIndex { table: QName, name: String, def: String },
    DropIndex { index: QName, unique: bool, replaced: bool },

    EnableRls { table: QName },
    DisableRls { table: QName },
    ForceRls { table: QName },
    UnforceRls { table: QName },
    CreatePolicy { table: QName, def: Policy },
    DropPolicy { table: QName, name: String, replaced: bool },

    CreateView { view: QName, materialized: bool, def: String },
    DropView { view: QName, materialized: bool, replaced: bool },

    /// `def` is a complete CREATE OR REPLACE FUNCTION/PROCEDURE statement.
    CreateFunction { key: String, def: String, replacing: bool },
    DropFunction { key: String, schema: String, signature: String },

    CreateTrigger { key: String, table: QName, def: String },
    DropTrigger { table: QName, name: String, replaced: bool },
}

impl Change {
    /// Destructive = loses data, or weakens integrity in a way this script
    /// does not itself restore. Replacement drops (`replaced: true`) are not
    /// destructive because the paired create restores the object in the same
    /// script.
    pub fn is_destructive(&self) -> bool {
        match self {
            Change::DropExtension { .. }
            | Change::DropEnum { .. }
            | Change::EnumNeedsRebuild { .. }
            | Change::DropSequence { .. }
            | Change::DropTable { .. }
            | Change::DropColumn { .. }
            | Change::DropSerial { .. }
            | Change::RegenerateColumn { .. }
            | Change::DropFunction { .. } => true,
            Change::DropConstraint { kind, replaced, .. } => !replaced && kind.drop_is_destructive(),
            Change::DropIndex { unique, replaced, .. } => !replaced && *unique,
            Change::DropView { replaced, .. } => !replaced,
            Change::DropTrigger { replaced, .. } => !replaced,
            Change::DropPolicy { replaced, .. } => !replaced,
            _ => false,
        }
    }

    /// Changes that require manual intervention and are only ever emitted as
    /// commentary.
    pub fn is_manual(&self) -> bool {
        matches!(self, Change::EnumNeedsRebuild { .. })
    }
}

#[derive(Debug, Default, Serialize)]
pub struct Plan {
    pub changes: Vec<Change>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn destructive_count(&self) -> usize {
        self.changes.iter().filter(|c| c.is_destructive()).count()
    }
}

/// Compare `target` (live state) against `source` (desired state).
pub fn diff(source: &Catalog, target: &Catalog) -> Plan {
    let mut plan = Plan::default();

    diff_schemas(source, target, &mut plan);
    diff_extensions(source, target, &mut plan);
    diff_enums(source, target, &mut plan);
    diff_sequences(source, target, &mut plan);
    diff_tables(source, target, &mut plan);
    diff_views(source, target, &mut plan);
    diff_functions(source, target, &mut plan);
    diff_triggers(source, target, &mut plan);

    plan
}

fn diff_schemas(source: &Catalog, target: &Catalog, plan: &mut Plan) {
    // Schemas that hold desired objects but don't exist on the target are
    // created. Schemas are never dropped: they may hold out-of-scope objects.
    for schema in &source.schemas {
        let target_has = target.schemas.contains(schema);
        let source_uses = source.tables.keys().any(|q| &q.schema == schema)
            || source.enums.keys().any(|q| &q.schema == schema)
            || source.sequences.keys().any(|q| &q.schema == schema)
            || source.views.keys().any(|q| &q.schema == schema)
            || source.functions.keys().any(|k| k.starts_with(&format!("{schema}.")))
            || schema != "public";
        if !target_has && source_uses {
            plan.changes.push(Change::CreateSchema { name: schema.clone() });
        }
    }
}

fn diff_extensions(source: &Catalog, target: &Catalog, plan: &mut Plan) {
    for ext in source.extensions.difference(&target.extensions) {
        plan.changes.push(Change::CreateExtension { name: ext.clone() });
    }
    for ext in target.extensions.difference(&source.extensions) {
        plan.changes.push(Change::DropExtension { name: ext.clone() });
    }
}

fn diff_enums(source: &Catalog, target: &Catalog, plan: &mut Plan) {
    for (ty, labels) in &source.enums {
        match target.enums.get(ty) {
            None => plan.changes.push(Change::CreateEnum { ty: ty.clone(), labels: labels.clone() }),
            Some(t_labels) if t_labels == labels => {}
            Some(t_labels) => {
                // Only pure insertions are automatable: the target's labels
                // must appear in the source in the same relative order.
                if is_subsequence(t_labels, labels) {
                    let mut anchor_iter = t_labels.iter().peekable();
                    for label in labels {
                        if anchor_iter.peek().copied() == Some(label) {
                            anchor_iter.next();
                            continue;
                        }
                        plan.changes.push(Change::AddEnumValue {
                            ty: ty.clone(),
                            value: label.clone(),
                            before: anchor_iter.peek().map(|l| (*l).clone()),
                        });
                    }
                } else {
                    plan.changes.push(Change::EnumNeedsRebuild {
                        ty: ty.clone(),
                        source_labels: labels.clone(),
                        target_labels: t_labels.clone(),
                    });
                }
            }
        }
    }
    for ty in target.enums.keys() {
        if !source.enums.contains_key(ty) {
            plan.changes.push(Change::DropEnum { ty: ty.clone() });
        }
    }
}

fn is_subsequence(needle: &[String], haystack: &[String]) -> bool {
    let mut it = haystack.iter();
    needle.iter().all(|n| it.any(|h| h == n))
}

fn diff_sequences(source: &Catalog, target: &Catalog, plan: &mut Plan) {
    for (q, def) in &source.sequences {
        match target.sequences.get(q) {
            None => plan.changes.push(Change::CreateSequence { seq: q.clone(), def: def.clone() }),
            Some(t) if t == def => {}
            Some(_) => plan.changes.push(Change::AlterSequence { seq: q.clone(), def: def.clone() }),
        }
    }
    for q in target.sequences.keys() {
        if !source.sequences.contains_key(q) {
            plan.changes.push(Change::DropSequence { seq: q.clone() });
        }
    }
}

fn diff_tables(source: &Catalog, target: &Catalog, plan: &mut Plan) {
    for (q, s_table) in &source.tables {
        match target.tables.get(q) {
            None => plan.changes.push(Change::CreateTable { table: q.clone(), def: s_table.clone() }),
            Some(t_table) => diff_one_table(q, s_table, t_table, plan),
        }
    }
    for q in target.tables.keys() {
        if !source.tables.contains_key(q) {
            plan.changes.push(Change::DropTable { table: q.clone() });
        }
    }
}

fn diff_one_table(q: &QName, s: &Table, t: &Table, plan: &mut Plan) {
    if s.partition_by != t.partition_by {
        // Partitioning strategy cannot be altered in place: converging is a
        // full (destructive, gated) table rebuild — nothing finer-grained
        // applies, so short-circuit.
        plan.changes.push(Change::DropTable { table: q.clone() });
        plan.changes.push(Change::CreateTable { table: q.clone(), def: s.clone() });
        return;
    }

    // Columns (compared as a set by name; ordinal drift is not a difference).
    for s_col in &s.columns {
        match t.column(&s_col.name) {
            None => plan.changes.push(Change::AddColumn { table: q.clone(), def: s_col.clone() }),
            Some(t_col) if s_col.semantic_eq(t_col) => {}
            Some(t_col) => diff_one_column(q, s_col, t_col, plan),
        }
    }
    for t_col in &t.columns {
        if s.column(&t_col.name).is_none() {
            plan.changes.push(Change::DropColumn { table: q.clone(), column: t_col.name.clone() });
        }
    }

    // Constraints, compared by (name, deparsed definition).
    for (name, s_con) in &s.constraints {
        match t.constraints.get(name) {
            None => plan.changes.push(Change::AddConstraint {
                table: q.clone(),
                name: name.clone(),
                def: s_con.def.clone(),
                kind: s_con.kind,
                table_is_new: false,
            }),
            Some(t_con) if t_con.def == s_con.def => {}
            Some(t_con) => {
                plan.changes.push(Change::DropConstraint {
                    table: q.clone(),
                    name: name.clone(),
                    kind: t_con.kind,
                    replaced: true,
                });
                plan.changes.push(Change::AddConstraint {
                    table: q.clone(),
                    name: name.clone(),
                    def: s_con.def.clone(),
                    kind: s_con.kind,
                    table_is_new: false,
                });
            }
        }
    }
    for (name, t_con) in &t.constraints {
        if !s.constraints.contains_key(name) {
            plan.changes.push(Change::DropConstraint {
                table: q.clone(),
                name: name.clone(),
                kind: t_con.kind,
                replaced: false,
            });
        }
    }

    // Free-standing indexes, compared by (name, full indexdef).
    for (name, s_idx) in &s.indexes {
        match t.indexes.get(name) {
            None => plan.changes.push(Change::CreateIndex {
                table: q.clone(),
                name: name.clone(),
                def: s_idx.def.clone(),
            }),
            Some(t_idx) if t_idx.def == s_idx.def => {}
            Some(t_idx) => {
                plan.changes.push(Change::DropIndex {
                    index: QName::new(q.schema.clone(), name.clone()),
                    unique: t_idx.unique,
                    replaced: true,
                });
                plan.changes.push(Change::CreateIndex {
                    table: q.clone(),
                    name: name.clone(),
                    def: s_idx.def.clone(),
                });
            }
        }
    }
    for (name, t_idx) in &t.indexes {
        if !s.indexes.contains_key(name) {
            plan.changes.push(Change::DropIndex {
                index: QName::new(q.schema.clone(), name.clone()),
                unique: t_idx.unique,
                replaced: false,
            });
        }
    }

    // Row-level security.
    match (s.rls_enabled, t.rls_enabled) {
        (true, false) => plan.changes.push(Change::EnableRls { table: q.clone() }),
        (false, true) => plan.changes.push(Change::DisableRls { table: q.clone() }),
        _ => {}
    }
    match (s.rls_forced, t.rls_forced) {
        (true, false) => plan.changes.push(Change::ForceRls { table: q.clone() }),
        (false, true) => plan.changes.push(Change::UnforceRls { table: q.clone() }),
        _ => {}
    }

    // Policies, compared structurally.
    for (name, s_pol) in &s.policies {
        match t.policies.get(name) {
            None => plan.changes.push(Change::CreatePolicy { table: q.clone(), def: s_pol.clone() }),
            Some(t_pol) if t_pol == s_pol => {}
            Some(_) => {
                plan.changes.push(Change::DropPolicy { table: q.clone(), name: name.clone(), replaced: true });
                plan.changes.push(Change::CreatePolicy { table: q.clone(), def: s_pol.clone() });
            }
        }
    }
    for name in t.policies.keys() {
        if !s.policies.contains_key(name) {
            plan.changes.push(Change::DropPolicy { table: q.clone(), name: name.clone(), replaced: false });
        }
    }

    if s.partition_by != t.partition_by {
        // Partitioning strategy cannot be altered in place; surfaced as a
        // rebuild-class manual item via EnumNeedsRebuild-style commentary is
        // overkill — reuse RegenerateColumn? No: encode as manual note through
        // the emit layer using a dedicated change would grow the enum for a
        // rare case. Emit as DropTable+CreateTable is far too destructive to
        // automate silently, so we log it as a destructive table rebuild.
        plan.changes.push(Change::DropTable { table: q.clone() });
        plan.changes.push(Change::CreateTable {
            table: q.clone(),
            def: Table { partition_by: s.partition_by.clone(), ..s_clone_for_rebuild(s) },
        });
    }
}

fn s_clone_for_rebuild(s: &Table) -> Table {
    s.clone()
}

fn diff_one_column(q: &QName, s: &Column, t: &Column, plan: &mut Plan) {
    // Generated expression change ⇒ column rebuild (destructive), and no
    // other per-facet changes make sense on top.
    if s.generated != t.generated {
        plan.changes.push(Change::RegenerateColumn { table: q.clone(), def: s.clone() });
        return;
    }

    let col = s.name.clone();

    if s.type_sql != t.type_sql || s.collation != t.collation {
        plan.changes.push(Change::AlterColumnType {
            table: q.clone(),
            column: col.clone(),
            from: t.type_sql.clone(),
            to: s.type_sql.clone(),
            collation: s.collation.clone(),
        });
    }

    // Identity transitions.
    match (t.identity, s.identity) {
        (None, Some(kind)) => plan.changes.push(Change::AddIdentity { table: q.clone(), column: col.clone(), kind }),
        (Some(_), None) => plan.changes.push(Change::DropIdentity { table: q.clone(), column: col.clone() }),
        (Some(a), Some(b)) if a != b => {
            plan.changes.push(Change::SetIdentityKind { table: q.clone(), column: col.clone(), kind: b })
        }
        _ => {}
    }

    // Serial / default transitions (skipped entirely for identity columns —
    // identity manages its own sequence).
    if s.identity.is_none() && t.identity.is_none() {
        match (t.is_serial, s.is_serial) {
            (false, true) => plan.changes.push(Change::MakeSerial {
                table: q.clone(),
                column: col.clone(),
                type_sql: s.type_sql.clone(),
            }),
            (true, false) => {
                plan.changes.push(Change::DropSerial { table: q.clone(), column: col.clone() });
                if let Some(expr) = &s.default {
                    plan.changes.push(Change::SetDefault { table: q.clone(), column: col.clone(), expr: expr.clone() });
                }
            }
            (false, false) => {
                if s.default != t.default {
                    match &s.default {
                        Some(expr) => plan.changes.push(Change::SetDefault {
                            table: q.clone(),
                            column: col.clone(),
                            expr: expr.clone(),
                        }),
                        None => plan.changes.push(Change::DropDefault { table: q.clone(), column: col.clone() }),
                    }
                }
            }
            (true, true) => {}
        }
    }

    match (t.not_null, s.not_null) {
        (false, true) => plan.changes.push(Change::SetNotNull { table: q.clone(), column: col }),
        (true, false) => plan.changes.push(Change::DropNotNull { table: q.clone(), column: col }),
        _ => {}
    }
}

fn diff_views(source: &Catalog, target: &Catalog, plan: &mut Plan) {
    for (q, s_view) in &source.views {
        match target.views.get(q) {
            None => plan.changes.push(Change::CreateView {
                view: q.clone(),
                materialized: s_view.materialized,
                def: s_view.def.clone(),
            }),
            Some(t_view) if t_view == s_view => {}
            Some(t_view) => {
                plan.changes.push(Change::DropView {
                    view: q.clone(),
                    materialized: t_view.materialized,
                    replaced: true,
                });
                plan.changes.push(Change::CreateView {
                    view: q.clone(),
                    materialized: s_view.materialized,
                    def: s_view.def.clone(),
                });
            }
        }
    }
    for (q, t_view) in &target.views {
        if !source.views.contains_key(q) {
            plan.changes.push(Change::DropView {
                view: q.clone(),
                materialized: t_view.materialized,
                replaced: false,
            });
        }
    }
}

fn diff_functions(source: &Catalog, target: &Catalog, plan: &mut Plan) {
    for (key, s_fn) in &source.functions {
        match target.functions.get(key) {
            None => plan.changes.push(Change::CreateFunction {
                key: key.clone(),
                def: s_fn.def.clone(),
                replacing: false,
            }),
            Some(t_fn) if t_fn.def == s_fn.def => {}
            Some(_) => plan.changes.push(Change::CreateFunction {
                key: key.clone(),
                def: s_fn.def.clone(),
                replacing: true,
            }),
        }
    }
    for (key, t_fn) in &target.functions {
        if !source.functions.contains_key(key) {
            let schema = key.split('.').next().unwrap_or("public").to_string();
            plan.changes.push(Change::DropFunction {
                key: key.clone(),
                schema,
                signature: t_fn.signature.clone(),
            });
        }
    }
}

fn diff_triggers(source: &Catalog, target: &Catalog, plan: &mut Plan) {
    for (key, s_trg) in &source.triggers {
        match target.triggers.get(key) {
            None => plan.changes.push(Change::CreateTrigger {
                key: key.clone(),
                table: s_trg.table.clone(),
                def: s_trg.def.clone(),
            }),
            Some(t_trg) if t_trg.def == s_trg.def => {}
            Some(t_trg) => {
                plan.changes.push(Change::DropTrigger {
                    table: t_trg.table.clone(),
                    name: t_trg.name.clone(),
                    replaced: true,
                });
                plan.changes.push(Change::CreateTrigger {
                    key: key.clone(),
                    table: s_trg.table.clone(),
                    def: s_trg.def.clone(),
                });
            }
        }
    }
    for (key, t_trg) in &target.triggers {
        if !source.triggers.contains_key(key) {
            plan.changes.push(Change::DropTrigger {
                table: t_trg.table.clone(),
                name: t_trg.name.clone(),
                replaced: false,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> Column {
        Column {
            name: name.into(),
            type_sql: ty.into(),
            not_null: false,
            default: None,
            identity: None,
            generated: None,
            is_serial: false,
            collation: None,
        }
    }

    fn table_with(columns: Vec<Column>) -> Table {
        Table {
            columns,
            constraints: Default::default(),
            indexes: Default::default(),
            partition_by: None,
            rls_enabled: false,
            rls_forced: false,
            policies: Default::default(),
        }
    }

    #[test]
    fn identical_catalogs_produce_empty_plan() {
        let mut a = Catalog::empty_with_schemas(["public".into()]);
        a.tables.insert(QName::new("public", "t"), table_with(vec![col("id", "integer")]));
        let plan = diff(&a, &a.clone());
        assert!(plan.is_empty(), "plan not empty: {:?}", plan.changes);
    }

    #[test]
    fn enum_append_and_insert_positions() {
        let mut src = Catalog::empty_with_schemas(["public".into()]);
        let mut tgt = src.clone();
        let ty = QName::new("public", "mood");
        src.enums.insert(ty.clone(), vec!["sad".into(), "meh".into(), "ok".into(), "great".into()]);
        tgt.enums.insert(ty.clone(), vec!["sad".into(), "ok".into()]);
        let plan = diff(&src, &tgt);
        let adds: Vec<(String, Option<String>)> = plan
            .changes
            .iter()
            .filter_map(|c| match c {
                Change::AddEnumValue { value, before, .. } => Some((value.clone(), before.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            adds,
            vec![
                ("meh".to_string(), Some("ok".to_string())),
                ("great".to_string(), None),
            ]
        );
    }

    #[test]
    fn enum_reorder_is_manual() {
        let mut src = Catalog::empty_with_schemas(["public".into()]);
        let mut tgt = src.clone();
        let ty = QName::new("public", "mood");
        src.enums.insert(ty.clone(), vec!["b".into(), "a".into()]);
        tgt.enums.insert(ty.clone(), vec!["a".into(), "b".into()]);
        let plan = diff(&src, &tgt);
        assert!(matches!(plan.changes.as_slice(), [Change::EnumNeedsRebuild { .. }]));
    }

    #[test]
    fn changed_constraint_is_replace_not_destructive() {
        let mut src = Catalog::empty_with_schemas(["public".into()]);
        let mut tgt = src.clone();
        let q = QName::new("public", "t");
        let mut s_t = table_with(vec![col("id", "integer")]);
        let mut t_t = s_t.clone();
        s_t.constraints.insert(
            "t_pkey".into(),
            Constraint { name: "t_pkey".into(), kind: ConstraintKind::PrimaryKey, def: "PRIMARY KEY (id)".into() },
        );
        t_t.constraints.insert(
            "t_pkey".into(),
            Constraint { name: "t_pkey".into(), kind: ConstraintKind::PrimaryKey, def: "PRIMARY KEY (id, x)".into() },
        );
        src.tables.insert(q.clone(), s_t);
        tgt.tables.insert(q.clone(), t_t);
        let plan = diff(&src, &tgt);
        assert_eq!(plan.changes.len(), 2);
        assert_eq!(plan.destructive_count(), 0, "replace pair must not be destructive");
    }

    #[test]
    fn removed_unique_constraint_is_destructive_but_removed_check_is_not() {
        let mut src = Catalog::empty_with_schemas(["public".into()]);
        let mut tgt = src.clone();
        let q = QName::new("public", "t");
        let s_t = table_with(vec![col("id", "integer")]);
        let mut t_t = s_t.clone();
        t_t.constraints.insert(
            "t_uniq".into(),
            Constraint { name: "t_uniq".into(), kind: ConstraintKind::Unique, def: "UNIQUE (id)".into() },
        );
        t_t.constraints.insert(
            "t_chk".into(),
            Constraint { name: "t_chk".into(), kind: ConstraintKind::Check, def: "CHECK ((id > 0))".into() },
        );
        src.tables.insert(q.clone(), s_t);
        tgt.tables.insert(q.clone(), t_t);
        let plan = diff(&src, &tgt);
        assert_eq!(plan.changes.len(), 2);
        assert_eq!(plan.destructive_count(), 1);
    }

    #[test]
    fn default_transitions() {
        let mut src = Catalog::empty_with_schemas(["public".into()]);
        let mut tgt = src.clone();
        let q = QName::new("public", "t");
        let mut s_col = col("n", "integer");
        s_col.default = Some("42".into());
        let t_col = col("n", "integer");
        src.tables.insert(q.clone(), table_with(vec![s_col]));
        tgt.tables.insert(q.clone(), table_with(vec![t_col]));
        let plan = diff(&src, &tgt);
        assert!(matches!(plan.changes.as_slice(), [Change::SetDefault { expr, .. }] if expr == "42"));
    }
}
