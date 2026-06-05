-- T-51b: facts inherit their source capture's container tags so that
-- `localmem recall` and `localmem profile` can filter by tag.
--
-- Stored as a JSON-encoded `BTreeMap<String, String>` in a TEXT column
-- (DuckDB's JSON type aliases to VARCHAR; TEXT keeps the duckdb-rs
-- binding straightforward and matches how the lexical index stores
-- tags for capture-side filtering in T-51).
--
-- Nullable, no default: DuckDB does not currently support adding a
-- column with a NOT NULL constraint via ALTER TABLE
-- ("Parser Error: Adding columns with constraints not yet supported").
-- Existing v0.1.x rows therefore receive NULL, which the read path
-- in `fact_from_row` collapses to an empty `BTreeMap` so behavior
-- matches "no tags" exactly. New INSERTs always provide a JSON
-- string (`{}` for the empty map), so NULL only appears on legacy
-- rows.

ALTER TABLE facts ADD COLUMN tags TEXT;
