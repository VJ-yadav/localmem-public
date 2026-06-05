-- T-52: facts carry the closed-core kind taxonomy
-- (fact / preference / decision / constraint / todo / note + extension).
--
-- Stored as TEXT (the canonical string form from
-- `crate::kind::Kind::as_str`); extension kinds preserve their
-- original value verbatim. The read path collapses NULL / `""` /
-- `"note"` to `Kind::Note`, matching the same backward-compat
-- discipline migration 0002 used for the `tags` column.
--
-- Nullable, no default: DuckDB does not support adding a column
-- with a NOT NULL constraint via ALTER TABLE
-- ("Parser Error: Adding columns with constraints not yet
-- supported"). Existing v0.1.x / pre-T-52 rows therefore receive
-- NULL, which the Rust read path normalises to `Kind::Note`.

ALTER TABLE facts ADD COLUMN kind TEXT;
