-- Intelligence v2 P2: the typed-knowledge-graph NODE layer.
--
-- Understanding events already name typed entities ({name, kind}); the facts
-- table already holds the EDGES (subject -predicate-> object). What was missing
-- is a resolved-NODE layer, so the graph renders typed, DEDUPLICATED nodes
-- (one "localmem" node, not 50 spellings of it) instead of raw co-occurrence.
--
-- Append-only by design, mirroring the event log: each row is ONE mention of an
-- entity by ONE understanding. The resolved node (dominant kind, mention count,
-- most-recent display form, first/last seen) is a GROUP BY over `canonical`, so
-- nothing here is ever mutated and the whole table is recomputable by replay
-- from the Understanding events (invariant #2). `canonical` is the resolution
-- key (lowercased / whitespace-collapsed surface form); `display_name` keeps the
-- human spelling for rendering.

CREATE TABLE IF NOT EXISTS entity_mentions (
    canonical     TEXT NOT NULL,
    display_name  TEXT NOT NULL,
    kind          TEXT NOT NULL,
    valid_from    TIMESTAMPTZ NOT NULL,
    source_event  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_entity_canonical ON entity_mentions(canonical);
