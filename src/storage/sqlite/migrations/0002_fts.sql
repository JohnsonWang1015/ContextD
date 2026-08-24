-- Unified full-text index over memories, decisions and checkpoints.
--
-- A single index (rather than one per table) keeps cross-entity search and
-- ranking uniform: `contextd search "scheduler"` looks in every kind of record
-- with one query and one bm25 scale.
--
-- The indexed columns hold text that ContextD normalised before insertion (see
-- util::text::tokenize), which is what makes CJK content searchable — SQLite's
-- unicode61 tokenizer would otherwise treat a whole Chinese phrase as one
-- token. Rows are maintained by the storage layer inside the same transaction
-- as the base-table write; there are deliberately no triggers, so the schema
-- does not depend on application-defined SQL functions.
CREATE VIRTUAL TABLE search_index USING fts5(
    kind UNINDEXED,
    record_id UNINDEXED,
    project_id UNINDEXED,
    title,
    body,
    tags,
    tokenize = 'unicode61 remove_diacritics 2'
);
