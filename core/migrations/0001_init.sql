-- Initial facts schema. See ARCHITECTURE.md (Derived stores -> facts.duckdb).
--
-- Bitemporal layout:
--   valid_from / valid_to     = when the fact was true in the real world
--   recorded_at / retired_at  = when we recorded / superseded it
--
-- "Deletion" never mutates; emit a `forget` event in events.jsonl and set
-- retired_at + (optionally) valid_to here at replay time.

CREATE TABLE IF NOT EXISTS facts (
    id            TEXT PRIMARY KEY,
    subject       TEXT NOT NULL,
    predicate     TEXT NOT NULL,
    object        TEXT NOT NULL,
    confidence    DOUBLE NOT NULL,
    valid_from    TIMESTAMPTZ NOT NULL,
    valid_to      TIMESTAMPTZ,
    recorded_at   TIMESTAMPTZ NOT NULL,
    retired_at    TIMESTAMPTZ,
    source_events TEXT[],
    policy_id     TEXT
);

CREATE INDEX IF NOT EXISTS idx_facts_subject ON facts(subject);
CREATE INDEX IF NOT EXISTS idx_facts_valid   ON facts(valid_from, valid_to);
