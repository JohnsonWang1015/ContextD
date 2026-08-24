//! Rendering command results as text or JSON.

use serde::Serialize;

use crate::cli::GlobalArgs;
use crate::core::model::{Memory, Status};
use crate::error::Result;
use crate::search::SearchHit;
use crate::ui;
use crate::util::{ids, text, time};

/// Print a value as JSON, or run the text renderer.
pub fn render<T: Serialize>(
    global: &GlobalArgs,
    value: &T,
    text_form: impl FnOnce() -> String,
) -> Result<()> {
    if global.json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        let rendered = text_form();
        if !rendered.trim().is_empty() {
            println!("{rendered}");
        }
    }
    Ok(())
}

/// Table of search hits.
pub fn hits_table(hits: &[SearchHit], show_scores: bool) -> String {
    if hits.is_empty() {
        return ui::dim("No matches.").to_string();
    }
    let headers: Vec<&str> = if show_scores {
        vec!["score", "kind", "id", "project", "title", "excerpt"]
    } else {
        vec!["kind", "id", "project", "title", "excerpt"]
    };
    let rows: Vec<Vec<String>> = hits
        .iter()
        .map(|hit| {
            let mut row = Vec::new();
            if show_scores {
                row.push(format!("{:.2}", hit.score));
            }
            row.push(kind_label(hit));
            row.push(ids::short(&hit.id).to_string());
            row.push(hit.project_name.clone().unwrap_or_else(|| "—".into()));
            row.push(text::one_line(&hit.title, 48));
            let excerpt = text::one_line(&hit.content, 60);
            row.push(ui::dim(if excerpt.trim() == hit.title.trim() { "" } else { &excerpt }));
            row
        })
        .collect();
    ui::table(&headers, &rows)
}

/// Kind plus lifecycle marker, so history is visible at a glance.
fn kind_label(hit: &SearchHit) -> String {
    let base = hit.kind.to_string();
    if hit.is_current() {
        base
    } else {
        ui::yellow(&format!("{base}*"))
    }
}

/// Table of memories.
pub fn memories_table(memories: &[Memory]) -> String {
    if memories.is_empty() {
        return ui::dim("No memories yet.").to_string();
    }
    let rows: Vec<Vec<String>> = memories
        .iter()
        .map(|memory| {
            vec![
                ids::short(&memory.id).to_string(),
                memory.category.to_string(),
                status_label(memory.status),
                memory.priority.to_string(),
                text::one_line(&memory.title, 56),
                ui::dim(&time::humanize_since(&memory.updated_at)),
            ]
        })
        .collect();
    ui::table(&["id", "category", "status", "pri", "title", "updated"], &rows)
}

/// Colour a status so superseded rows read as history.
pub fn status_label(status: Status) -> String {
    match status {
        Status::Active => ui::green("active"),
        Status::Superseded => ui::yellow("superseded"),
        Status::Deprecated => ui::yellow("deprecated"),
        Status::Archived => ui::dim("archived"),
    }
}

/// One memory in full.
pub fn memory_detail(memory: &Memory) -> String {
    let mut out = format!("{}\n\n{}\n\n", ui::bold(&memory.title), memory.content.trim());
    let mut rows: Vec<(&str, String)> = vec![
        ("id", memory.id.clone()),
        ("category", memory.category.to_string()),
        ("status", status_label(memory.status)),
        ("priority", memory.priority.to_string()),
        ("source", memory.source.label()),
        (
            "updated",
            format!(
                "{} ({})",
                time::to_storage(&memory.updated_at),
                time::humanize_since(&memory.updated_at)
            ),
        ),
    ];
    if let Some(successor) = &memory.superseded_by {
        rows.push(("replaced by", ids::short(successor).to_string()));
    }
    if !memory.tags.is_empty() {
        rows.push(("tags", memory.tags.join(", ")));
    }
    if !memory.files.is_empty() {
        rows.push(("files", memory.files.join(", ")));
    }
    if let Some(commit) = &memory.commit {
        rows.push(("commit", crate::util::git::short_commit(commit).to_string()));
    }
    if let Some(symbol) = &memory.symbol {
        rows.push(("symbol", symbol.clone()));
    }
    out.push_str(&ui::kv(&rows));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{Category, RecordKind};

    fn memory() -> Memory {
        let mut memory = Memory::new(Category::Architecture, "Transport", "Scheduler uses NATS");
        memory.tags = vec!["nats".into()];
        memory.files = vec!["src/scheduler.rs".into()];
        memory
    }

    #[test]
    fn memory_detail_lists_metadata() {
        ui::init_color("never", false);
        let text = memory_detail(&memory());
        assert!(text.contains("Scheduler uses NATS"));
        assert!(text.contains("category"));
        assert!(text.contains("src/scheduler.rs"));
    }

    #[test]
    fn tables_handle_empty_input() {
        ui::init_color("never", false);
        assert!(memories_table(&[]).contains("No memories"));
        assert!(hits_table(&[], false).contains("No matches"));
    }

    #[test]
    fn superseded_hits_are_marked() {
        ui::init_color("never", false);
        let hit = SearchHit {
            kind: RecordKind::Memory,
            id: "abcdef123456".into(),
            project_id: None,
            project_name: Some("FerroGrid".into()),
            title: "Task queue is Redis".into(),
            content: "Task queue is Redis".into(),
            category: Some(Category::Architecture),
            status: Status::Superseded,
            priority: 3,
            updated_at: time::now(),
            superseded_by: None,
            score: 0.5,
            breakdown: Default::default(),
        };
        let table = hits_table(&[hit], true);
        assert!(table.contains("memory*"), "superseded rows carry a marker: {table}");
        assert!(table.contains("0.50"));
    }
}
