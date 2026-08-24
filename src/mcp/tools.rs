//! MCP tools.
//!
//! Every tool answers with *retrieved* context, never the whole store: that is
//! the point of ContextD. `project_context` is budgeted, `memory_search` and
//! `semantic_recall` are limited and ranked, and results carry their lifecycle
//! status so a model can tell current truth from history.

use serde_json::{json, Value};

use crate::app::App;
use crate::core::checkpoint::{CheckpointService, NewCheckpoint};
use crate::core::context::{render, ContextBuilder, ContextRequest};
use crate::core::decision::DecisionService;
use crate::core::memory::{MemoryService, NewMemory};
use crate::core::model::{Category, Project, RecordKind, Source};
use crate::core::project::ProjectService;
use crate::error::Result;
use crate::mcp::protocol::{ToolResult, ToolSpec};
use crate::search::{SearchMode, SearchRequest, SearchService};
use crate::storage::repository::ProjectScope;
use crate::util::{ids, text, time};

/// Tools this server exposes.
pub fn specs(read_only: bool) -> Vec<ToolSpec> {
    let mut specs = vec![
        ToolSpec {
            name: "memory_search",
            description: "Keyword-first search across project memories, architecture decisions \
                          and checkpoints. Use when you know roughly what the fact is called.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Words to search for"},
                    "project": {"type": "string", "description": "Project name or slug; defaults to the server's project"},
                    "category": {"type": "string", "description": "Restrict to one category (architecture, decision, convention, task, knowledge, …)"},
                    "kind": {"type": "string", "enum": ["memory", "decision", "checkpoint"]},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 10},
                    "include_history": {"type": "boolean", "default": false, "description": "Include superseded and archived records"}
                },
                "required": ["query"]
            }),
        },
        ToolSpec {
            name: "semantic_recall",
            description: "Answer a question from memory using hybrid semantic + keyword \
                          retrieval. Use when you want the fact but not its exact wording, \
                          e.g. 'which message transport does the scheduler use?'.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "The question, in natural language"},
                    "project": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 25, "default": 5},
                    "include_history": {"type": "boolean", "default": false}
                },
                "required": ["query"]
            }),
        },
        ToolSpec {
            name: "memory_get",
            description: "Fetch one memory in full by id or id prefix.",
            input_schema: json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"]
            }),
        },
        ToolSpec {
            name: "project_context",
            description: "The context an agent should start from: current architecture, \
                          conventions, pinned memories and the latest checkpoint, packed into \
                          a token budget. Optionally focused on a query.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project": {"type": "string"},
                    "query": {"type": "string", "description": "Focus the context on this topic"},
                    "max_tokens": {"type": "integer", "minimum": 200, "maximum": 32000}
                }
            }),
        },
        ToolSpec {
            name: "project_status",
            description: "Counts, git branch, latest checkpoint and index state for a project.",
            input_schema: json!({
                "type": "object",
                "properties": {"project": {"type": "string"}}
            }),
        },
        ToolSpec {
            name: "checkpoint_latest",
            description: "The most recent checkpoint: current goal, what is done, what is next, \
                          open problems and git state.",
            input_schema: json!({
                "type": "object",
                "properties": {"project": {"type": "string"}}
            }),
        },
        ToolSpec {
            name: "architecture_decisions",
            description: "Architecture decision records. Returns the decisions that currently \
                          hold; superseded ones only when asked for.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project": {"type": "string"},
                    "include_superseded": {"type": "boolean", "default": false}
                }
            }),
        },
    ];

    if !read_only {
        specs.push(ToolSpec {
            name: "memory_add",
            description: "Record a new memory. Use for durable facts (decisions, conventions, \
                          architecture), not for chat. Set supersedes when this replaces an \
                          earlier memory.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "content": {"type": "string"},
                    "title": {"type": "string"},
                    "category": {"type": "string", "default": "project"},
                    "project": {"type": "string"},
                    "priority": {"type": "integer", "minimum": 1, "maximum": 5},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "files": {"type": "array", "items": {"type": "string"}},
                    "supersedes": {"type": "string", "description": "Id of the memory this replaces"}
                },
                "required": ["content"]
            }),
        });
        specs.push(ToolSpec {
            name: "checkpoint_create",
            description: "Save where the work stands, so the next session can resume it.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "summary": {"type": "string"},
                    "project": {"type": "string"},
                    "goal": {"type": "string"},
                    "completed": {"type": "array", "items": {"type": "string"}},
                    "next_steps": {"type": "array", "items": {"type": "string"}},
                    "open_problems": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["summary"]
            }),
        });
    }
    specs
}

/// Execute a tool call.
pub async fn call(
    app: &App,
    default_project: Option<&str>,
    read_only: bool,
    name: &str,
    arguments: &Value,
) -> Result<ToolResult> {
    if read_only && matches!(name, "memory_add" | "checkpoint_create") {
        return Ok(ToolResult::error(format!(
            "`{name}` is unavailable: this ContextD server is running read-only"
        )));
    }

    match name {
        "memory_search" => memory_search(app, default_project, arguments).await,
        "semantic_recall" => semantic_recall(app, default_project, arguments).await,
        "memory_get" => memory_get(app, arguments),
        "project_context" => project_context(app, default_project, arguments).await,
        "project_status" => project_status(app, default_project, arguments),
        "checkpoint_latest" => checkpoint_latest(app, default_project, arguments),
        "architecture_decisions" => architecture_decisions(app, default_project, arguments),
        "memory_add" => memory_add(app, default_project, arguments).await,
        "checkpoint_create" => checkpoint_create(app, default_project, arguments),
        other => Ok(ToolResult::error(format!("unknown tool `{other}`"))),
    }
}

async fn memory_search(
    app: &App,
    default_project: Option<&str>,
    args: &Value,
) -> Result<ToolResult> {
    let Some(query) = str_arg(args, "query") else {
        return Ok(ToolResult::error("`query` is required"));
    };
    let project = resolve_project(app, default_project, args)?;
    let category = match str_arg(args, "category") {
        Some(value) => match value.parse::<Category>() {
            Ok(category) => vec![category],
            Err(err) => return Ok(ToolResult::error(err.to_string())),
        },
        None => Vec::new(),
    };
    let kinds = match str_arg(args, "kind") {
        Some(value) => match value.parse::<RecordKind>() {
            Ok(kind) => vec![kind],
            Err(err) => return Ok(ToolResult::error(err.to_string())),
        },
        None => Vec::new(),
    };

    let request = SearchRequest {
        categories: category,
        kinds,
        limit: usize_arg(args, "limit").unwrap_or(10).clamp(1, 50),
        include_history: bool_arg(args, "include_history").unwrap_or(false),
        mode: SearchMode::Hybrid,
        ..SearchRequest::new(query).in_scope(scope(&project))
    };
    let hits = SearchService::new(app).search(&request).await?;
    Ok(hits_result(&hits, &request.query))
}

async fn semantic_recall(
    app: &App,
    default_project: Option<&str>,
    args: &Value,
) -> Result<ToolResult> {
    let Some(query) = str_arg(args, "query") else {
        return Ok(ToolResult::error("`query` is required"));
    };
    let project = resolve_project(app, default_project, args)?;
    let request = SearchRequest {
        limit: usize_arg(args, "limit").unwrap_or(5).clamp(1, 25),
        include_history: bool_arg(args, "include_history").unwrap_or(false),
        mode: SearchMode::Hybrid,
        ..SearchRequest::new(query).in_scope(scope(&project))
    };
    let hits = SearchService::new(app).search(&request).await?;
    Ok(hits_result(&hits, &request.query))
}

fn memory_get(app: &App, args: &Value) -> Result<ToolResult> {
    let Some(id) = str_arg(args, "id") else {
        return Ok(ToolResult::error("`id` is required"));
    };
    match MemoryService::new(app).get(&id) {
        Ok(memory) => {
            let text = format!(
                "# {}\n\n{}\n\ncategory: {} · status: {} · priority: {}{}",
                memory.title,
                memory.content,
                memory.category,
                memory.status,
                memory.priority,
                memory
                    .superseded_by
                    .as_ref()
                    .map(|id| format!(" · replaced by {}", ids::short(id)))
                    .unwrap_or_default()
            );
            Ok(ToolResult::text(text, Some(serde_json::to_value(&memory)?)))
        }
        Err(err) => Ok(ToolResult::error(err.to_string())),
    }
}

async fn project_context(
    app: &App,
    default_project: Option<&str>,
    args: &Value,
) -> Result<ToolResult> {
    let project = resolve_project(app, default_project, args)?;
    let mut request = ContextRequest::from_config(app, project.clone());
    if let Some(max_tokens) = usize_arg(args, "max_tokens") {
        request.max_tokens = max_tokens.clamp(200, 32_000);
    }
    if let Some(query) = str_arg(args, "query") {
        request = request.with_query(query);
    }

    let bundle = ContextBuilder::new(app).build(&request).await?;
    let markdown = render::markdown(&bundle, &render::RenderOptions::default());
    let structured = json!({
        "project": project.as_ref().map(|p| p.name.clone()),
        "memories": bundle.memories.len(),
        "decisions": bundle.decisions.len(),
        "budget": bundle.budget,
        "markdown": markdown,
    });
    Ok(ToolResult::text(markdown, Some(structured)))
}

fn project_status(app: &App, default_project: Option<&str>, args: &Value) -> Result<ToolResult> {
    let project = resolve_project(app, default_project, args)?;
    let report = ProjectService::new(app).status(project.as_ref())?;
    let name = report.project.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| "—".into());
    let text = format!(
        "{name}: {} memories ({} superseded), {} decisions, {} checkpoints\nbranch: {}\nlast checkpoint: {}",
        report.stats.active_memories,
        report.stats.superseded_memories,
        report.stats.decisions,
        report.stats.checkpoints,
        report.git.branch.clone().unwrap_or_else(|| "unknown".into()),
        report
            .latest_checkpoint
            .as_ref()
            .map(|c| format!("{} ({})", c.summary, time::humanize_since(&c.created_at)))
            .unwrap_or_else(|| "none".into())
    );
    let structured = json!({
        "project": report.project,
        "stats": report.stats,
        "branch": report.git.branch,
        "commit": report.git.commit,
        "dirty_files": report.git.dirty_files.len(),
        "embedded": {"current": report.embedded.0, "total": report.embedded.1},
        "embedding_provider": report.embedding_provider,
    });
    Ok(ToolResult::text(text, Some(structured)))
}

fn checkpoint_latest(app: &App, default_project: Option<&str>, args: &Value) -> Result<ToolResult> {
    let Some(project) = resolve_project(app, default_project, args)? else {
        return Ok(ToolResult::error("no project: pass `project` or start the server inside one"));
    };
    match CheckpointService::new(app).latest(&project)? {
        Some(checkpoint) => {
            let mut text = format!("{}\n", checkpoint.summary);
            if let Some(goal) = &checkpoint.current_goal {
                text.push_str(&format!("\nGoal: {goal}\n"));
            }
            for (label, items) in [
                ("Completed", &checkpoint.completed),
                ("Next", &checkpoint.next_steps),
                ("Open problems", &checkpoint.open_problems),
            ] {
                if !items.is_empty() {
                    text.push_str(&format!("\n{label}:\n"));
                    for item in items {
                        text.push_str(&format!("- {item}\n"));
                    }
                }
            }
            text.push_str(&format!("\nSaved {}", time::humanize_since(&checkpoint.created_at)));
            Ok(ToolResult::text(text, Some(serde_json::to_value(&checkpoint)?)))
        }
        None => Ok(ToolResult::text(
            format!("No checkpoint recorded for {} yet.", project.name),
            Some(json!({"checkpoint": null})),
        )),
    }
}

fn architecture_decisions(
    app: &App,
    default_project: Option<&str>,
    args: &Value,
) -> Result<ToolResult> {
    let Some(project) = resolve_project(app, default_project, args)? else {
        return Ok(ToolResult::error("no project: pass `project` or start the server inside one"));
    };
    let service = DecisionService::new(app);
    let include_superseded = bool_arg(args, "include_superseded").unwrap_or(false);
    let decisions =
        if include_superseded { service.all(&project)? } else { service.current(&project)? };

    if decisions.is_empty() {
        return Ok(ToolResult::text(
            format!("No architecture decisions recorded for {}.", project.name),
            Some(json!({"decisions": []})),
        ));
    }

    let mut text = format!("Architecture decisions for {}:\n", project.name);
    for decision in &decisions {
        let marker = if decision.status.is_current() { "•" } else { "×" };
        text.push_str(&format!("\n{marker} {} — {}\n", decision.title, decision.decision));
        if let Some(context) = decision.context.as_ref().filter(|c| !c.trim().is_empty()) {
            text.push_str(&format!("  context: {}\n", text::one_line(context, 200)));
        }
        if !decision.status.is_current() {
            text.push_str(&format!("  status: {} (no longer current)\n", decision.status));
        }
    }
    Ok(ToolResult::text(text, Some(serde_json::to_value(&decisions)?)))
}

async fn memory_add(app: &App, default_project: Option<&str>, args: &Value) -> Result<ToolResult> {
    let Some(content) = str_arg(args, "content") else {
        return Ok(ToolResult::error("`content` is required"));
    };
    let project = resolve_project(app, default_project, args)?;
    let category = match str_arg(args, "category") {
        Some(value) => match value.parse::<Category>() {
            Ok(category) => category,
            Err(err) => return Ok(ToolResult::error(err.to_string())),
        },
        None => Category::Project,
    };

    let memory = MemoryService::new(app).add(NewMemory {
        project,
        category,
        title: str_arg(args, "title"),
        priority: args.get("priority").and_then(Value::as_i64),
        tags: string_list(args, "tags"),
        files: string_list(args, "files"),
        source: Source::Agent { agent: "mcp".into() },
        supersedes: str_arg(args, "supersedes"),
        ..NewMemory::new(category, content)
    })?;

    // Best effort: the memory is stored either way.
    let _ = crate::search::IndexService::new(app)
        .embed_record(&crate::core::model::RecordRef::memory(&memory.id))
        .await;

    Ok(ToolResult::text(
        format!("Stored “{}” ({}) as {}", memory.title, memory.category, ids::short(&memory.id)),
        Some(serde_json::to_value(&memory)?),
    ))
}

fn checkpoint_create(app: &App, default_project: Option<&str>, args: &Value) -> Result<ToolResult> {
    let Some(summary) = str_arg(args, "summary") else {
        return Ok(ToolResult::error("`summary` is required"));
    };
    let Some(project) = resolve_project(app, default_project, args)? else {
        return Ok(ToolResult::error("no project: pass `project` or start the server inside one"));
    };

    let checkpoint = CheckpointService::new(app).create(
        &project,
        NewCheckpoint {
            summary,
            current_goal: str_arg(args, "goal"),
            completed: string_list(args, "completed"),
            next_steps: string_list(args, "next_steps"),
            open_problems: string_list(args, "open_problems"),
            ..Default::default()
        },
    )?;

    Ok(ToolResult::text(
        format!("Checkpoint saved for {}: {}", project.name, checkpoint.summary),
        Some(serde_json::to_value(&checkpoint)?),
    ))
}

/// Render hits compactly, marking anything that is no longer current.
fn hits_result(hits: &[crate::search::SearchHit], query: &str) -> ToolResult {
    if hits.is_empty() {
        return ToolResult::text(
            format!("Nothing in ContextD matches “{query}”."),
            Some(json!({"results": []})),
        );
    }
    let mut text = String::new();
    for hit in hits {
        text.push_str(&format!("## {}\n", hit.title));
        text.push_str(&format!("{}\n", hit.content.trim()));
        let mut meta = vec![
            format!("kind: {}", hit.kind),
            format!("id: {}", ids::short(&hit.id)),
            format!("score: {:.2}", hit.score),
        ];
        if let Some(project) = &hit.project_name {
            meta.push(format!("project: {project}"));
        }
        if !hit.is_current() {
            meta.push(format!("status: {} — NOT current", hit.status));
        }
        text.push_str(&format!("{}\n\n", meta.join(" · ")));
    }
    ToolResult::text(text.trim_end(), Some(json!({ "results": hits })))
}

fn scope(project: &Option<Project>) -> ProjectScope {
    match project {
        Some(project) => ProjectScope::ProjectWithGlobal(project.id.clone()),
        None => ProjectScope::Any,
    }
}

/// Project named in the arguments, else the server default, else the project
/// containing the server's working directory.
fn resolve_project(
    app: &App,
    default_project: Option<&str>,
    args: &Value,
) -> Result<Option<Project>> {
    if let Some(name) = str_arg(args, "project") {
        return app.lookup_project(&name).map(Some);
    }
    app.resolve_project(default_project)
}

fn str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn bool_arg(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

fn usize_arg(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|value| value as usize)
}

fn string_list(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}
