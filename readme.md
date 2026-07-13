# declarative-postgres-migrate (`dpm`)

Declarative, **ORM-agnostic** Postgres schema migration, in Rust — a library with a CLI on top.

The core idea: **the Postgres system catalogs are the neutral interchange format.** It doesn't matter whether a schema was authored by Prisma, Drizzle, SeaORM, ent, peewee, or raw SQL — once it's in a database, `pg_catalog` describes it canonically. `dpm` introspects two states, diffs the catalogs, and emits ordered, reviewable SQL that converges the target onto the source:

```
dpm diff --source postgres://…/desired --target postgres://…/live
```

Because both sides are deparsed *by the server itself* (`pg_get_constraintdef`, `pg_get_indexdef`, `pg_get_viewdef`, `pg_get_functiondef`, `format_type`, all with `search_path = ''`), comparison is exact string equality — no regex normalization of hand-written SQL against Postgres' re-serialized forms (the failure mode that makes schema-file-vs-catalog differs so hairy).

## The three source/target kinds — every combination works

`--source` and `--target` each accept any of:

| form | meaning |
|---|---|
| `postgres://…` | live database, introspected directly |
| `catalog.json` | a saved snapshot from `dpm dump` (offline diffs, CI artifacts, AI review) |
| `schema.sql` | a declarative schema file **or a `pg_dump --schema-only` dump**, materialized into a throwaway database on `--shadow` and introspected there (the drizzle-kit shadow-database technique, with real Postgres semantics) |

All nine pairings (db↔db, sql↔db, db↔sql, sql↔sql, json↔db, …) are supported and produce identical migrations for identical underlying schemas — the integration suite asserts byte-equality across every combination. Explicit-kind flags/env vars exist alongside the generic ones:

```
-s/--source, --from            SOURCE_DATABASE_URL      (url | .json | .sql, sniffed)
-t/--target, --to              TARGET_DATABASE_URL      (falls back to DATABASE_URL)
--source-sql / --target-sql    SOURCE_SQL_FILE / TARGET_SQL_FILE
--source-json / --target-json  SOURCE_CATALOG_JSON / TARGET_CATALOG_JSON
```

pg_dump compatibility: psql meta-commands (`\restrict`, `\unrestrict`, `\connect`, `\.` — including the 2025 security-release `\restrict` headers) are stripped during materialization, and role-dependent statements (`GRANT`/`REVOKE`/`ALTER … OWNER TO`/`SET ROLE`) are skipped, since dpm does not diff ownership/grants and a fresh shadow database lacks production roles. Dumps made with `--no-owner --no-privileges` are cleanest, but ordinary schema-only dumps work.

## Commands

```
dpm diff        # generate the migration SQL (or --format json for a machine-readable plan)
dpm apply       # generate + execute against the target (interactive confirm unless --yes)
dpm dump        # snapshot a database catalog to JSON
dpm bootstrap   # full DDL for a source (diff against an empty database)
dpm verify      # rehearse the migration on a shadow replica and PROVE convergence
dpm review      # generate the migration and have an AI tool review it
dpm help        # flag/env reference (generated from .cli-flags.toml)
```

### `dpm verify` — the confidence loop

`verify` never touches the real target. It:

1. replays the **target's** schema onto a throwaway database (proves dpm's own bootstrap emission is faithful — if the replica drifts from the target, dpm refuses and tells you it has a coverage gap),
2. applies the generated migration to the replica,
3. re-introspects and re-diffs against the source — an **empty plan is the proof of convergence**,
4. optionally cross-checks with an external tool: `--external-check 'migra {target} {source}'` (any command template works — migra, pgdiff, a custom script, an AI reviewer; empty stdout + exit 0 = agreement).

Exit codes: `0` verified · `3` not converged (CI-friendly), and `dpm diff --fail-on-diff` exits `2` on drift like `git diff --exit-code`.

### AI review — claude / codex / chatgpt / gemini

`dpm review` (or `--ai-review` on `diff`, `apply`, and `verify`) sends a self-contained payload — reviewer instructions, the destructive-consent policy in force, the JSON change plan, and the full SQL — to a coding-agent CLI in non-interactive mode and parses a machine verdict:

```
dpm review --source schema.sql --target "$DATABASE_URL" --shadow "$SHADOW_DATABASE_URL"   # claude by default
dpm apply  --ai-review --ai-tool gemini --yes ...    # review gates the apply, before any DB write
```

| flag | env | notes |
|---|---|---|
| `--ai-review` | `DPM_AI_REVIEW` | enable review inside diff/apply/verify (`dpm review` implies it) |
| `--ai-tool` | `DPM_AI_TOOL` | `claude` (default) \| `codex` \| `chatgpt` (codex alias) \| `gemini` \| `custom` |
| `--ai-cmd` | `DPM_AI_CMD` | custom command template; `{file}` = payload path (also overrides a named tool) |
| `--ai-strict` | `DPM_AI_STRICT` | default `true`: a REJECT (or missing verdict) blocks — apply aborts, others exit `4` |

Built-in templates: `claude -p < {file}`, `codex exec - < {file}`, `gemini < {file}`. The reviewer must end with `DPM_VERDICT: APPROVE` or `DPM_VERDICT: REJECT <reason>`; dpm **fails closed** — no parseable verdict, a crashed reviewer, or a nonzero exit all count as rejection. The payload tells the reviewer the exact destructive-consent flags in force, so "a live `DROP TABLE` appeared without `--allow-destructive-sql`" is a policy violation it is instructed to reject. In `dpm apply`, the review runs *before* anything touches the database. Reviewers run via `sh -c`, so the tool must be on `PATH` (or use an absolute path in `--ai-cmd`).

## Safety model (house rules)

- **Reviewable SQL only.** `dpm diff` prints SQL; it never executes. `dpm apply` requires an interactive `yes` or an explicit `--yes`.
- **Destructive changes need two separate consents.** Drops of tables, columns, enums, sequences, functions, standalone views/triggers/policies, and integrity-weakening drops (PK/unique/exclusion constraints, unique indexes) are:
  1. emitted **commented out** unless `--allow-destructive-sql` (`DPM_ALLOW_DESTRUCTIVE_SQL`) — the consent to *generate* destructive SQL;
  2. refused at execution time by `dpm apply` unless `--allow-destructive-ops` (`DPM_ALLOW_DESTRUCTIVE_OPS`) — the consent to *run* it. A script containing live destructive statements without ops-consent aborts before any statement executes.
  `--allow-destructive` remains as legacy shorthand for both. Replacement drops (drop + recreate in the same script) are not considered destructive.
- **Constraint adds on existing tables use `NOT VALID` + `VALIDATE`** (short lock window, full validation).
- **Enum value additions are emitted before `BEGIN`** — a value added inside a transaction can't be referenced until it commits.
- **FKs are added last**, after every referenced table/PK exists; drops run dependents-first (triggers → policies → views → FKs → other constraints → indexes).
- **Manual-review items** (enum label removal/reorder, partition-strategy changes) are surfaced as comments, never guessed at.

## What's covered

Schemas, extensions (as units — extension-owned objects are excluded via `pg_depend`), enums (create/append/positioned insert via `ADD VALUE BEFORE`), tables (incl. partitioned parents), columns (types, defaults, `NOT NULL`, collations, `serial`/`bigserial` detection, `GENERATED … AS IDENTITY` both kinds, `GENERATED … STORED`), PK/unique/check/FK/exclusion constraints, free-standing indexes (partial, expression, `DESC`, any access method — full `pg_get_indexdef` fidelity), standalone sequences, views, materialized views, functions & procedures, triggers, **row-level security + policies** (Supabase-critical).

Known limitations (deliberate v1 scope): no `COMMENT ON`, no grants/ownership/roles (policies reference roles by name; the role must exist), no domains, no partition child management, no aggregate/window functions, identity sequence options aren't diffed, column *order* isn't enforced, cross-view dependency ordering is name-order (a changed view stack with inter-dependencies may need manual ordering), and type changes that require an FK drop/re-add on *other* tables aren't cascaded automatically. **Data migrations are out of scope for now** — the JSON plan format is the seam where a data-migration phase will slot in later.

## Supabase notes

- Managed schemas (`auth`, `storage`, `realtime`, `graphql*`, `vault`, `pgsodium*`, `extensions`, …) are excluded by default — you diff *your* schema, not the platform's. Override with `--schemas` / `--exclude-schemas`.
- Connect introspection through the **direct connection or session pooler (port 5432)**, not the transaction pooler (6543): dpm sets `search_path = ''` for canonical deparsing, verifies it stuck, and refuses with a clear error if a transaction-mode pooler dropped it.
- RLS + policies are first-class diffed objects.

## CLI contract: flags-2-env

Flags follow the [flags-2-env](https://github.com/ORESoftware/flags-2-env) convention, declared in `.cli-flags.toml`: **every flag maps to an environment variable**, with precedence `flag > env > default`.

| flag | env |
|---|---|
| `-s, --source, --from, --desired` | `SOURCE_DATABASE_URL` |
| `-t, --target, --to, --current` | `TARGET_DATABASE_URL` (falls back to `DATABASE_URL`) |
| `--shadow, --scratch` | `SHADOW_DATABASE_URL` |
| `--schemas` / `--exclude-schemas` | `DPM_SCHEMAS` / `DPM_EXCLUDE_SCHEMAS` |
| `--allow-destructive-sql` / `--allow-destructive-ops` | `DPM_ALLOW_DESTRUCTIVE_SQL` / `DPM_ALLOW_DESTRUCTIVE_OPS` |
| `--allow-destructive` (legacy: implies both) | `DPM_ALLOW_DESTRUCTIVE` |
| `--ai-review`, `--ai-tool`, `--ai-cmd`, `--ai-strict` | `DPM_AI_REVIEW`, `DPM_AI_TOOL`, `DPM_AI_CMD`, `DPM_AI_STRICT` |
| `--format`, `-o/--out`, `--yes`, `--fail-on-diff`, `--keep-shadow`, `--verbose` | `DPM_FORMAT`, `DPM_OUT`, `DPM_YES`, `DPM_FAIL_ON_DIFF`, `DPM_KEEP_SHADOW`, `DPM_VERBOSE` |
| `--advise-fk-indexes` | `DPM_ADVISE_FK_INDEXES` |
| `--external-check` | `DPM_EXTERNAL_CHECK` |

When the native flags2env core is available (`FLAGS2ENV_LIB=/path/to/libflags2env.dylib`), dpm loads it via dlopen; otherwise a built-in parser of the same `.cli-flags.toml` contract is used, so the binary is self-contained.

A fully env-driven invocation (CI-friendly):

```sh
export SOURCE_DATABASE_URL=postgres://…/desired
export TARGET_DATABASE_URL=postgres://…/live
export SHADOW_DATABASE_URL=postgres://…/postgres   # role needs CREATEDB
dpm verify && dpm apply --yes
```

## Advisory: FK supporting indexes

Every foreign key should have an index leading with its referencing column (without one, cascading deletes scan the child table). `dpm` appends advisory *comments* (never DDL — they'd make the next diff non-convergent) for FKs in the desired schema that lack one. Disable with `--advise-fk-indexes=false`.

## Library use

The CLI is a thin shell over the `dpm` library crate:

```rust
let source = dpm::introspect_url(&source_url, &Default::default()).await?;
let target = dpm::introspect_url(&target_url, &Default::default()).await?;
let plan = dpm::diff(&source, &target);              // typed Vec<Change>, serde-serializable
let script = dpm::emit(&plan, &dpm::EmitOptions::default());
println!("{}", script.sql);
```

Layers: `model` (serializable `Catalog`) → `introspect` → `diff` (pure) → `emit` → `apply` (dollar-quote-aware statement splitter) → `verify`, plus `advisor`, `source`, `flagenv`.

## Development

```sh
cargo test                # unit tests (no database needed)
scripts/test.sh           # boots an ephemeral Postgres cluster (initdb/pg_ctl,
                          # no system services), runs unit + convergence tests,
                          # tears everything down
```

The integration suite's core invariant is **convergence**: for schema pairs covering every supported object class (bootstrap-from-empty, divergent evolution, teardown-to-empty, enum insertion, identical-databases-diff-clean, verify-rehearsal), applying the generated migration and re-diffing must yield zero changes.

## Prior art & lineage

- **migra / pgdiff** — the db↔db catalog-diff lineage this follows; `--external-check` lets them cross-validate dpm's output.
- **drizzle-kit / prisma migrate** — the shadow-database materialization technique for `.sql` sources.
- The in-house `pg-defs` diff tooling — source of the safety rules (reviewable SQL only, gated destructive changes, `NOT VALID`+`VALIDATE`) and the FK-index advisor.

MIT © Alex Mills
