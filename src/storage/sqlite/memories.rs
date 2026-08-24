//! Memory rows, their tags, and the search-index entries derived from them.
//!
//! Writes maintain the FTS index in the same transaction as the row itself, so
//! a crash can never leave a memory that is invisible to search.

use rusqlite::{params, Connection, OptionalExtension, Row, ToSql};

use crate::core::model::{Category, Memory, RecordKind, RecordRef, Source, Status};
use crate::error::{Error, Result};
use crate::storage::repository::{
    IndexableRecord, MemoryFilter, MemoryOrder, MemoryRepository, ProjectScope,
};
use crate::util::time;

use super::{from_json_list, placeholders, to_json_list, SqliteStore};

const COLUMNS: &str = "id, project_id, category, title, content, source, priority, status, \
                       superseded_by, related_files, commit_hash, symbol, created_at, updated_at";

fn map_memory(row: &Row<'_>) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: row.get(0)?,
        project_id: row.get(1)?,
        // A row with an unrecognised category still has to be readable, so it
        // degrades to `knowledge` rather than failing the whole query.
        category: row.get::<_, String>(2)?.parse().unwrap_or(Category::Knowledge),
        title: row.get(3)?,
        content: row.get(4)?,
        source: Source::from_storage(&row.get::<_, String>(5)?),
        priority: row.get(6)?,
        status: row.get::<_, String>(7)?.parse().unwrap_or(Status::Active),
        superseded_by: row.get(8)?,
        tags: Vec::new(),
        files: from_json_list(&row.get::<_, String>(9)?),
        commit: row.get(10)?,
        symbol: row.get(11)?,
        created_at: time::from_storage(&row.get::<_, String>(12)?),
        updated_at: time::from_storage(&row.get::<_, String>(13)?),
    })
}

fn indexable(memory: &Memory) -> IndexableRecord {
    IndexableRecord {
        record: RecordRef::memory(&memory.id),
        project_id: memory.project_id.clone(),
        title: memory.title.clone(),
        body: memory.content.clone(),
        tags: memory.tags.clone(),
    }
}

/// Insert the row, its tags and its index entry. Shared by create and update.
fn write_row(conn: &Connection, memory: &Memory, insert: bool) -> Result<()> {
    let sql = if insert {
        "INSERT INTO memories (id, project_id, category, title, content, source, priority,
                               status, superseded_by, related_files, commit_hash, symbol,
                               created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
    } else {
        "UPDATE memories
            SET project_id = ?2, category = ?3, title = ?4, content = ?5, source = ?6,
                priority = ?7, status = ?8, superseded_by = ?9, related_files = ?10,
                commit_hash = ?11, symbol = ?12, created_at = ?13, updated_at = ?14
          WHERE id = ?1"
    };
    conn.execute(
        sql,
        params![
            memory.id,
            memory.project_id,
            memory.category.as_str(),
            memory.title,
            memory.content,
            memory.source.to_storage(),
            memory.priority,
            memory.status.as_str(),
            memory.superseded_by,
            to_json_list(&memory.files),
            memory.commit,
            memory.symbol,
            time::to_storage(&memory.created_at),
            time::to_storage(&memory.updated_at),
        ],
    )?;

    conn.execute("DELETE FROM memory_tags WHERE memory_id = ?1", params![memory.id])?;
    let mut stmt =
        conn.prepare("INSERT OR IGNORE INTO memory_tags (memory_id, tag) VALUES (?1, ?2)")?;
    for tag in &memory.tags {
        let tag = tag.trim().to_lowercase();
        if !tag.is_empty() {
            stmt.execute(params![memory.id, tag])?;
        }
    }
    drop(stmt);

    super::fts::index_record(conn, &indexable(memory))
}

impl MemoryRepository for SqliteStore {
    fn create_memory(&self, memory: &Memory) -> Result<()> {
        memory.validate()?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        write_row(&tx, memory, true)?;
        tx.commit()?;
        Ok(())
    }

    fn update_memory(&self, memory: &Memory) -> Result<()> {
        memory.validate()?;
        let mut updated = memory.clone();
        updated.updated_at = time::now();
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        write_row(&tx, &updated, false)?;
        tx.commit()?;
        Ok(())
    }

    fn get_memory(&self, id: &str) -> Result<Option<Memory>> {
        let conn = self.conn();
        let sql = format!("SELECT {COLUMNS} FROM memories WHERE id = ?1");
        let Some(mut memory) = conn.query_row(&sql, params![id], map_memory).optional()? else {
            return Ok(None);
        };
        memory.tags = load_tags(&conn, id)?;
        Ok(Some(memory))
    }

    /// Accept a full id or an unambiguous prefix, so `contextd edit 8f3a` works.
    fn resolve_memory(&self, ident: &str) -> Result<Option<Memory>> {
        let ident = ident.trim();
        if ident.is_empty() {
            return Ok(None);
        }
        if let Some(memory) = self.get_memory(ident)? {
            return Ok(Some(memory));
        }
        let matches: Vec<String> = {
            let conn = self.conn();
            let mut stmt =
                conn.prepare("SELECT id FROM memories WHERE id LIKE ?1 || '%' LIMIT 5")?;
            let ids = stmt
                .query_map(params![ident], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ids
        };
        match matches.len() {
            0 => Ok(None),
            1 => self.get_memory(&matches[0]),
            n => Err(Error::Ambiguous { ident: ident.to_string(), count: n }),
        }
    }

    fn list_memories(&self, filter: &MemoryFilter) -> Result<Vec<Memory>> {
        let (where_sql, args) = build_where(filter);
        // rowid breaks ties so that ordering is total and stable: two rows
        // written in the same millisecond still come back in a defined order.
        let order = match filter.order {
            MemoryOrder::RecentFirst => "updated_at DESC, rowid DESC",
            MemoryOrder::OldestFirst => "updated_at ASC, rowid ASC",
            MemoryOrder::PriorityFirst => "priority DESC, updated_at DESC, rowid DESC",
        };
        // LIMIT/OFFSET are interpolated rather than bound because they are
        // `usize` values produced by ContextD itself, never user text.
        let limit = filter.limit.unwrap_or(usize::MAX >> 1);
        let sql = format!(
            "SELECT {COLUMNS} FROM memories WHERE {where_sql} \
             ORDER BY {order} LIMIT {limit} OFFSET {}",
            filter.offset
        );

        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let mut memories =
            stmt.query_map(refs.as_slice(), map_memory)?.collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        for memory in &mut memories {
            memory.tags = load_tags(&conn, &memory.id)?;
        }
        // Tag filtering is applied here rather than in SQL: the tag list is
        // short, and an AND-of-tags join clause would obscure the query.
        if !filter.tags.is_empty() {
            let wanted: Vec<String> = filter.tags.iter().map(|t| t.trim().to_lowercase()).collect();
            memories.retain(|m| wanted.iter().all(|t| m.tags.contains(t)));
        }
        Ok(memories)
    }

    fn count_memories(&self, filter: &MemoryFilter) -> Result<usize> {
        let (where_sql, args) = build_where(filter);
        let conn = self.conn();
        let sql = format!("SELECT count(*) FROM memories WHERE {where_sql}");
        let refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let n: i64 = conn.query_row(&sql, refs.as_slice(), |row| row.get(0))?;
        Ok(n as usize)
    }

    fn delete_memory(&self, id: &str) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        // The tombstone is written in the same transaction as the delete, so
        // a record can never disappear without leaving one behind — otherwise
        // the next sync would hand it straight back.
        let project_id: Option<String> = tx
            .query_row("SELECT project_id FROM memories WHERE id = ?1", params![id], |row| {
                row.get(0)
            })
            .optional()?
            .flatten();
        super::tombstones::insert(&tx, &RecordRef::memory(id), project_id.as_deref())?;
        super::fts::delete_record(&tx, &RecordRef::memory(id))?;
        tx.execute(
            "DELETE FROM embeddings WHERE record_kind = ?1 AND record_id = ?2",
            params![RecordKind::Memory.as_str(), id],
        )?;
        let removed = tx.execute("DELETE FROM memories WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(removed > 0)
    }

    fn supersede_memory(&self, old_id: &str, new_id: &str) -> Result<()> {
        if old_id == new_id {
            return Err(Error::invalid("superseded_by", "a memory cannot supersede itself"));
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let exists: i64 = tx.query_row(
            "SELECT count(*) FROM memories WHERE id IN (?1, ?2)",
            params![old_id, new_id],
            |row| row.get(0),
        )?;
        if exists != 2 {
            return Err(Error::MemoryNotFound(format!("{old_id} or {new_id}")));
        }
        tx.execute(
            "UPDATE memories SET status = 'superseded', superseded_by = ?2, updated_at = ?3
              WHERE id = ?1",
            params![old_id, new_id, time::to_storage(&time::now())],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn get_memories(&self, ids: &[String]) -> Result<Vec<Memory>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn();
        let sql =
            format!("SELECT {COLUMNS} FROM memories WHERE id IN ({})", placeholders(ids.len()));
        let mut stmt = conn.prepare(&sql)?;
        let mut memories = stmt
            .query_map(rusqlite::params_from_iter(ids), map_memory)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for memory in &mut memories {
            memory.tags = load_tags(&conn, &memory.id)?;
        }
        // Preserve the caller's ordering (usually relevance order).
        memories.sort_by_key(|m| ids.iter().position(|id| *id == m.id).unwrap_or(usize::MAX));
        Ok(memories)
    }

    fn all_tags(&self, scope: &ProjectScope) -> Result<Vec<(String, usize)>> {
        let (scope_sql, param) = scope.sql("m.project_id");
        let sql = format!(
            "SELECT t.tag, count(*) FROM memory_tags t
               JOIN memories m ON m.id = t.memory_id
              WHERE {scope_sql}
              GROUP BY t.tag ORDER BY count(*) DESC, t.tag"
        );
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = match param {
            Some(p) => stmt
                .query_map(params![p], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(rows)
    }
}

fn load_tags(conn: &Connection, memory_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT tag FROM memory_tags WHERE memory_id = ?1 ORDER BY tag")?;
    let tags =
        stmt.query_map(params![memory_id], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(tags)
}

/// Build the WHERE clause and its bound values.
fn build_where(filter: &MemoryFilter) -> (String, Vec<Box<dyn ToSql>>) {
    let mut clauses = Vec::new();
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();

    let (scope_sql, scope_param) = filter.scope.sql("project_id");
    clauses.push(scope_sql);
    if let Some(p) = scope_param {
        args.push(Box::new(p));
    }

    let statuses = filter.effective_statuses();
    clauses.push(format!("status IN ({})", placeholders(statuses.len())));
    for status in statuses {
        args.push(Box::new(status.as_str().to_string()));
    }

    if !filter.categories.is_empty() {
        clauses.push(format!("category IN ({})", placeholders(filter.categories.len())));
        for category in &filter.categories {
            args.push(Box::new(category.as_str().to_string()));
        }
    }

    if let Some(from) = &filter.created_from {
        clauses.push("created_at >= ?".to_string());
        args.push(Box::new(time::to_storage(from)));
    }

    if let Some(to) = &filter.created_to {
        clauses.push("created_at < ?".to_string());
        args.push(Box::new(time::to_storage(to)));
    }

    if let Some(needle) = &filter.contains {
        clauses.push(r"(title LIKE ? ESCAPE '\' OR content LIKE ? ESCAPE '\')".to_string());
        let pattern = format!("%{}%", escape_like(needle));
        args.push(Box::new(pattern.clone()));
        args.push(Box::new(pattern));
    }

    (clauses.join(" AND "), args)
}

/// `LIKE` treats `%` and `_` as wildcards; escape them so a literal search for
/// `100%` does not match everything.
fn escape_like(input: &str) -> String {
    input.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::Project;
    use crate::storage::repository::{FtsQuery, FullTextIndex, ProjectRepository};

    fn store_with_project() -> (SqliteStore, Project) {
        let store = SqliteStore::open_in_memory().unwrap();
        let project = Project {
            id: crate::util::ids::new_id(),
            name: "FerroGrid".into(),
            slug: "ferrogrid".into(),
            root_path: None,
            description: None,
            git_remote: None,
            default_branch: None,
            created_at: time::now(),
            updated_at: time::now(),
            active: true,
        };
        store.create_project(&project).unwrap();
        (store, project)
    }

    fn memory(project: &Project, title: &str, content: &str) -> Memory {
        let mut m = Memory::new(Category::Architecture, title, content);
        m.project_id = Some(project.id.clone());
        m
    }

    #[test]
    fn create_get_update_delete() {
        let (store, project) = store_with_project();
        let mut m = memory(&project, "Transport", "Scheduler uses NATS");
        m.tags = vec!["nats".into(), "Scheduler".into()];
        store.create_memory(&m).unwrap();

        let loaded = store.get_memory(&m.id).unwrap().unwrap();
        assert_eq!(loaded.title, "Transport");
        assert_eq!(loaded.tags, vec!["nats".to_string(), "scheduler".to_string()]);

        let mut edited = loaded.clone();
        edited.content = "Scheduler uses NATS JetStream".into();
        store.update_memory(&edited).unwrap();
        assert!(store.get_memory(&m.id).unwrap().unwrap().content.contains("JetStream"));

        assert!(store.delete_memory(&m.id).unwrap());
        assert!(store.get_memory(&m.id).unwrap().is_none());
    }

    #[test]
    fn invalid_memory_is_rejected() {
        let (store, project) = store_with_project();
        let mut m = memory(&project, "", "body");
        assert!(store.create_memory(&m).is_err());
        m.title = "ok".into();
        m.priority = 99;
        assert!(store.create_memory(&m).is_err());
    }

    #[test]
    fn writes_are_searchable_immediately() {
        let (store, project) = store_with_project();
        store.create_memory(&memory(&project, "Transport", "Scheduler uses NATS")).unwrap();
        let hits = store
            .fts_search(&FtsQuery {
                text: "NATS".into(),
                scope: ProjectScope::Project(project.id.clone()),
                kinds: vec![RecordKind::Memory],
                limit: 10,
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn deleting_a_memory_removes_it_from_search() {
        let (store, project) = store_with_project();
        let m = memory(&project, "Transport", "Scheduler uses NATS");
        store.create_memory(&m).unwrap();
        store.delete_memory(&m.id).unwrap();
        let hits = store
            .fts_search(&FtsQuery {
                text: "NATS".into(),
                scope: ProjectScope::Any,
                kinds: vec![],
                limit: 10,
            })
            .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn cjk_content_is_searchable() {
        let (store, project) = store_with_project();
        store.create_memory(&memory(&project, "排程器", "排程器使用 NATS 作為傳輸層")).unwrap();
        for query in ["排程", "傳輸", "NATS"] {
            let hits = store
                .fts_search(&FtsQuery {
                    text: query.into(),
                    scope: ProjectScope::Any,
                    kinds: vec![],
                    limit: 10,
                })
                .unwrap();
            assert!(!hits.is_empty(), "query {query} found nothing");
        }
    }

    #[test]
    fn resolve_by_prefix() {
        let (store, project) = store_with_project();
        let m = memory(&project, "Transport", "NATS");
        store.create_memory(&m).unwrap();
        let prefix = &m.id[..8];
        assert_eq!(store.resolve_memory(prefix).unwrap().unwrap().id, m.id);
        assert!(store.resolve_memory("zzzzzzzz").unwrap().is_none());
    }

    #[test]
    fn supersede_marks_history() {
        let (store, project) = store_with_project();
        let old = memory(&project, "Transport", "Redis queue");
        let new = memory(&project, "Transport", "NATS");
        store.create_memory(&old).unwrap();
        store.create_memory(&new).unwrap();
        store.supersede_memory(&old.id, &new.id).unwrap();

        let loaded = store.get_memory(&old.id).unwrap().unwrap();
        assert_eq!(loaded.status, Status::Superseded);
        assert_eq!(loaded.superseded_by.as_deref(), Some(new.id.as_str()));

        // Default listing shows current truth only.
        let active = store
            .list_memories(&MemoryFilter::for_scope(ProjectScope::Project(project.id.clone())))
            .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, new.id);
    }

    #[test]
    fn self_supersede_is_rejected() {
        let (store, project) = store_with_project();
        let m = memory(&project, "T", "c");
        store.create_memory(&m).unwrap();
        assert!(store.supersede_memory(&m.id, &m.id).is_err());
    }

    #[test]
    fn filters_by_category_tag_and_text() {
        let (store, project) = store_with_project();
        let mut a = memory(&project, "Transport", "NATS 100% reliable");
        a.tags = vec!["infra".into()];
        let mut b = Memory::new(Category::Convention, "Style", "rustfmt everywhere");
        b.project_id = Some(project.id.clone());
        b.tags = vec!["style".into()];
        store.create_memory(&a).unwrap();
        store.create_memory(&b).unwrap();

        let scope = ProjectScope::Project(project.id.clone());
        let by_cat = store
            .list_memories(
                &MemoryFilter::for_scope(scope.clone()).with_categories(vec![Category::Convention]),
            )
            .unwrap();
        assert_eq!(by_cat.len(), 1);
        assert_eq!(by_cat[0].title, "Style");

        let by_tag =
            MemoryFilter { tags: vec!["INFRA".into()], ..MemoryFilter::for_scope(scope.clone()) };
        assert_eq!(store.list_memories(&by_tag).unwrap().len(), 1);

        // `%` must be treated literally, not as a wildcard.
        let by_text = MemoryFilter {
            contains: Some("100%".into()),
            ..MemoryFilter::for_scope(scope.clone())
        };
        assert_eq!(store.list_memories(&by_text).unwrap().len(), 1);
        let no_match =
            MemoryFilter { contains: Some("%%%".into()), ..MemoryFilter::for_scope(scope) };
        assert!(store.list_memories(&no_match).unwrap().is_empty());
    }

    #[test]
    fn filters_by_creation_window() {
        let (store, project) = store_with_project();
        let mut old = memory(&project, "Old", "older memory");
        old.created_at = time::now() - chrono::Duration::hours(3);
        store.create_memory(&old).unwrap();
        store.create_memory(&memory(&project, "New", "newer memory")).unwrap();

        let since = MemoryFilter {
            created_from: Some(time::now() - chrono::Duration::hours(1)),
            ..MemoryFilter::for_scope(ProjectScope::Any)
        };
        let recent = store.list_memories(&since).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].title, "New");

        let window = MemoryFilter {
            created_from: Some(time::now() - chrono::Duration::hours(4)),
            created_to: Some(time::now() - chrono::Duration::hours(2)),
            ..MemoryFilter::for_scope(ProjectScope::Any)
        };
        assert_eq!(store.list_memories(&window).unwrap().len(), 1);
    }

    #[test]
    fn global_and_project_scopes() {
        let (store, project) = store_with_project();
        let global = Memory::new(Category::User, "Prefers", "small commits");
        store.create_memory(&global).unwrap();
        store.create_memory(&memory(&project, "Transport", "NATS")).unwrap();

        let only_global =
            store.list_memories(&MemoryFilter::for_scope(ProjectScope::GlobalOnly)).unwrap();
        assert_eq!(only_global.len(), 1);

        let both = store
            .list_memories(&MemoryFilter::for_scope(ProjectScope::ProjectWithGlobal(
                project.id.clone(),
            )))
            .unwrap();
        assert_eq!(both.len(), 2);
        assert_eq!(store.count_memories(&MemoryFilter::default()).unwrap(), 2);
    }

    #[test]
    fn get_memories_preserves_input_order() {
        let (store, project) = store_with_project();
        let a = memory(&project, "A", "a");
        let b = memory(&project, "B", "b");
        store.create_memory(&a).unwrap();
        store.create_memory(&b).unwrap();
        let ordered = store.get_memories(&[b.id.clone(), a.id.clone()]).unwrap();
        assert_eq!(ordered.iter().map(|m| m.title.as_str()).collect::<Vec<_>>(), vec!["B", "A"]);
    }

    #[test]
    fn deleting_project_cascades_to_memories_and_index() {
        let (store, project) = store_with_project();
        store.create_memory(&memory(&project, "Transport", "NATS")).unwrap();
        store.delete_project(&project.id).unwrap();
        assert_eq!(store.count_memories(&MemoryFilter::default()).unwrap(), 0);
        let hits = store
            .fts_search(&FtsQuery {
                text: "NATS".into(),
                scope: ProjectScope::Any,
                kinds: vec![],
                limit: 10,
            })
            .unwrap();
        assert!(hits.is_empty());
    }
}
