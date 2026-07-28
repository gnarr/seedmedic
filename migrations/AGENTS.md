# AGENTS.md — `migrations`

Supplements the root `AGENTS.md`.

## Rules

**Forward-only, numbered, immutable.** `NNNN_name.sql`, applied by
`sqlx::migrate!` at startup. Once a migration has shipped it is never edited —
somebody's database has already run it. Fix a mistake with a new migration.

**Additive by default.** Adding a nullable column or a new table is safe.
Dropping or renaming one means every deployed instance loses data at upgrade
time; if you genuinely need to, write the migration in two steps across two
releases and say so in the file.

**`CHECK` constraints mirror the Rust enums.** `repair_jobs.state`,
`review_from_state`, `materialization`, and `repair_job_files.match_confidence`
all enumerate their values. Adding an enum variant without adding a migration
means the insert fails at runtime, in production, on the one job that needed it.

Two tests guard the seams and will fail if you forget:

- `repair::adapters::sqlite::tests::the_actionable_state_list_matches_the_lifecycle`
- `repair::adapters::sqlite::tests::the_job_column_list_matches_the_schema`

**Widening an existing `CHECK` needs a table rebuild.** SQLite's `ALTER TABLE`
cannot modify a constraint in place. `0006_operator_match_confidence.sql` is
the pattern: create the table under a new name with the widened `CHECK`, copy
every row across, drop the old table, rename the new one into place — all
inside the migration's own transaction. Safe without touching
`PRAGMA foreign_keys` as long as the table being rebuilt is never a parent
another table references; if it is, follow SQLite's full 12-step procedure
instead.

**Timestamps are RFC 3339 text, UTC.** Written with
`to_rfc3339_opts(SecondsFormat::Micros, true)`, read with
`DateTime::parse_from_rfc3339`. SQLite has no date type and lexical ordering of
this format matches chronological ordering, which is what the claim query
depends on.

**Integers are `i64`.** SQLite has no unsigned type. Sizes cross the boundary
through `as_i64`/`as_u64` in the store adapter.

## No `sqlx::query!` macros

Use the runtime `sqlx::query()` API. The compile-time macros would require a
`DATABASE_URL` at build time or a checked-in `.sqlx` directory to keep in sync —
ongoing cost, for a schema this small, in exchange for type checks the row
mapping already performs. sqlx 0.9 additionally refuses non-literal SQL, so
anything that needs to be assembled is a `macro_rules!` producing a literal,
with a test asserting it matches the schema.

## Adding a migration

1. New file, next number.
2. If it adds an enum value, update the `CHECK` constraint *and* the Rust enum in
   the same change.
3. If it adds a column the store reads, add it to `job_columns!()` — deliberately
   not `SELECT *`, so the `.torrent` blob stays out of list queries.
4. Run `rtk cargo test`. The in-memory `database::test_pool()` applies every
   migration, so a broken one fails the whole suite immediately.
