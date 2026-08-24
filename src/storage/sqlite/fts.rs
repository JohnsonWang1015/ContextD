//! FTS5 index maintenance and querying.
//!
//! The index is written by [`index_record`] inside the same transaction as the
//! base-table write, so the index can never drift from the data on a crash.
//! Text is normalised through [`crate::util::text::tokenize`] before insertion
//! so that CJK content is searchable — SQLite's `unicode61` tokenizer treats a
//! run of Han characters as a single token, which would make `搜尋排程器`
//! findable only by typing the phrase exactly.

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::core::model::{RecordKind, RecordRef};
use crate::error::Result;
use crate::storage::repository::{FtsHit, FtsQuery, FullTextIndex, IndexableRecord, ProjectScope};
use crate::util::text;

use super::{from_json_list, placeholders, SqliteStore};

/// bm25 weights, one per FTS column: kind, record_id, project_id, title, body, tags.
/// Titles carry the most signal, tags a little, the unindexed columns none.
const BM25_WEIGHTS: &str = "0.0, 0.0, 0.0, 10.0, 1.0, 3.0";

/// Insert or replace one record in the index.
pub(crate) fn index_record(conn: &Connection, record: &IndexableRecord) -> Result<()> {
    delete_record(conn, &record.record)?;
    conn.execute(
        "INSERT INTO search_index (kind, record_id, project_id, title, body, tags)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            record.record.kind.as_str(),
            record.record.id,
            record.project_id,
            normalise(&record.title),
            normalise(&record.body),
            normalise(&record.tags.join(" ")),
        ],
    )?;
    Ok(())
}

/// Remove one record from the index.
pub(crate) fn delete_record(conn: &Connection, record: &RecordRef) -> Result<()> {
    conn.execute(
        "DELETE FROM search_index WHERE kind = ?1 AND record_id = ?2",
        params![record.kind.as_str(), record.id],
    )?;
    Ok(())
}

fn normalise(text: &str) -> String {
    text::tokenize(text).join(" ")
}

impl FullTextIndex for SqliteStore {
    fn fts_search(&self, query: &FtsQuery) -> Result<Vec<FtsHit>> {
        let match_expr = text::fts_query(&query.text);
        if match_expr.is_empty() {
            return Ok(Vec::new());
        }

        let kinds: Vec<RecordKind> =
            if query.kinds.is_empty() { all_kinds() } else { query.kinds.clone() };
        let (scope_sql, scope_param) = query.scope.sql("project_id");

        // All placeholders are positional `?`, so the argument vector is built
        // in exactly the order they appear in the statement.
        let sql = format!(
            "SELECT kind, record_id, project_id, bm25(search_index, {BM25_WEIGHTS}) AS rank
               FROM search_index
              WHERE search_index MATCH ?
                AND kind IN ({})
                AND {scope_sql}
              ORDER BY rank
              LIMIT ?",
            placeholders(kinds.len())
        );

        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;

        let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(match_expr)];
        for kind in &kinds {
            args.push(Box::new(kind.as_str().to_string()));
        }
        if let Some(p) = scope_param {
            args.push(Box::new(p));
        }
        args.push(Box::new(query.limit.max(1) as i64));
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();

        let raw = stmt
            .query_map(refs.as_slice(), |row| {
                let kind: String = row.get(0)?;
                let id: String = row.get(1)?;
                let project_id: Option<String> = row.get(2)?;
                let bm25: f64 = row.get(3)?;
                Ok((kind, id, project_id, bm25))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(normalise_scores(raw))
    }

    fn rebuild_fts(&self) -> Result<usize> {
        let records = self.indexable_records(&ProjectScope::Any, &all_kinds())?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM search_index", [])?;
        for record in &records {
            index_record(&tx, record)?;
        }
        tx.commit()?;
        Ok(records.len())
    }

    fn indexable_records(
        &self,
        scope: &ProjectScope,
        kinds: &[RecordKind],
    ) -> Result<Vec<IndexableRecord>> {
        let kinds = if kinds.is_empty() { all_kinds() } else { kinds.to_vec() };
        let conn = self.conn();
        let mut out = Vec::new();
        for kind in kinds {
            let (sql, param) = select_for(kind, scope, None);
            let mut stmt = conn.prepare(&sql)?;
            let rows = match param {
                Some(p) => stmt
                    .query_map(params![p], |row| map_indexable(kind, row))?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
                None => stmt
                    .query_map([], |row| map_indexable(kind, row))?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            };
            out.extend(rows);
        }
        // Tags live in their own table; fetch them only for memories.
        attach_tags(&conn, &mut out)?;
        Ok(out)
    }

    fn get_indexable(&self, record: &RecordRef) -> Result<Option<IndexableRecord>> {
        let conn = self.conn();
        let (sql, _) = select_for(record.kind, &ProjectScope::Any, Some(&record.id));
        let mut found = conn
            .query_row(&sql, params![record.id], |row| map_indexable(record.kind, row))
            .optional()?
            .into_iter()
            .collect::<Vec<_>>();
        attach_tags(&conn, &mut found)?;
        Ok(found.into_iter().next())
    }
}

fn all_kinds() -> Vec<RecordKind> {
    vec![RecordKind::Memory, RecordKind::Decision, RecordKind::Checkpoint]
}

/// Per-kind projection into (id, project_id, title, body).
fn select_for(
    kind: RecordKind,
    scope: &ProjectScope,
    by_id: Option<&str>,
) -> (String, Option<String>) {
    let (base, table) = match kind {
        RecordKind::Memory => {
            ("SELECT id, project_id, title, content FROM memories".to_string(), "memories")
        }
        RecordKind::Decision => (
            "SELECT id, project_id, title,
                    coalesce(decision, '') || char(10) || coalesce(context, '') || char(10) ||
                    coalesce(consequences, '') || char(10) || coalesce(alternatives, '')
               FROM architecture_decisions"
                .to_string(),
            "architecture_decisions",
        ),
        RecordKind::Checkpoint => (
            "SELECT id, project_id, summary,
                    coalesce(current_goal, '') || char(10) || coalesce(current_state, '') ||
                    char(10) || completed || char(10) || next_steps || char(10) || open_problems
               FROM checkpoints"
                .to_string(),
            "checkpoints",
        ),
    };
    let _ = table;
    if by_id.is_some() {
        return (format!("{base} WHERE id = ?1"), None);
    }
    let (scope_sql, param) = scope.sql("project_id");
    (format!("{base} WHERE {scope_sql}"), param)
}

fn map_indexable(kind: RecordKind, row: &Row<'_>) -> rusqlite::Result<IndexableRecord> {
    Ok(IndexableRecord {
        record: RecordRef::new(kind, row.get::<_, String>(0)?),
        project_id: row.get(1)?,
        title: row.get(2)?,
        body: clean_body(kind, &row.get::<_, String>(3)?),
        tags: Vec::new(),
    })
}

/// Decisions and checkpoints concatenate JSON list columns; render those as
/// plain lines so the index (and any excerpt) reads naturally.
fn clean_body(kind: RecordKind, raw: &str) -> String {
    if kind == RecordKind::Memory {
        return raw.to_string();
    }
    raw.lines()
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                from_json_list(trimmed).join("\n")
            } else {
                trimmed.to_string()
            }
        })
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn attach_tags(conn: &Connection, records: &mut [IndexableRecord]) -> Result<()> {
    let ids: Vec<&String> = records
        .iter()
        .filter(|r| r.record.kind == RecordKind::Memory)
        .map(|r| &r.record.id)
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let sql = format!(
        "SELECT memory_id, tag FROM memory_tags WHERE memory_id IN ({})",
        placeholders(ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(ids), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (memory_id, tag) in rows {
        if let Some(rec) = records.iter_mut().find(|r| r.record.id == memory_id) {
            rec.tags.push(tag);
        }
    }
    Ok(())
}

/// bm25 returns negative values where more negative is better. Flip the sign
/// and scale to 0.0..=1.0 against the best hit so the score can be fused with
/// cosine similarity, which lives on the same scale.
fn normalise_scores(raw: Vec<(String, String, Option<String>, f64)>) -> Vec<FtsHit> {
    let best = raw.iter().map(|(_, _, _, s)| -s).fold(0.0_f64, f64::max);
    raw.into_iter()
        .filter_map(|(kind, id, project_id, bm25)| {
            let kind: RecordKind = kind.parse().ok()?;
            let score = if best > 0.0 { (-bm25) / best } else { 0.0 };
            Some(FtsHit { record: RecordRef::new(kind, id), project_id, score })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_are_normalised_to_unit_range() {
        let hits = normalise_scores(vec![
            ("memory".into(), "a".into(), None, -8.0),
            ("memory".into(), "b".into(), None, -2.0),
        ]);
        assert_eq!(hits[0].score, 1.0);
        assert_eq!(hits[1].score, 0.25);
    }

    #[test]
    fn unknown_kind_is_dropped_not_fatal() {
        let hits = normalise_scores(vec![("wat".into(), "a".into(), None, -1.0)]);
        assert!(hits.is_empty());
    }

    #[test]
    fn clean_body_expands_json_lists() {
        let body = clean_body(RecordKind::Checkpoint, "goal\n[\"a\",\"b\"]");
        assert_eq!(body, "goal\na\nb");
    }
}
