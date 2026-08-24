//! `add`, `edit`, `delete`, `show`, `memories`, `supersede`.

use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::{output, parse_categories, parse_statuses, GlobalArgs};
use crate::core::memory::{MemoryPatch, MemoryService, NewMemory};
use crate::core::model::{Category, Memory, RecordRef, Status};
use crate::error::{Error, Result};
use crate::search::IndexService;
use crate::storage::repository::{MemoryFilter, MemoryOrder, ProjectScope};
use crate::ui;
use crate::util::ids;

/// `contextd add`
#[derive(Debug, Args)]
pub struct AddArgs {
    /// The memory itself. Multiple words are joined.
    #[arg(required = true, value_name = "CONTENT")]
    pub content: Vec<String>,

    /// What kind of knowledge this is.
    #[arg(short, long, default_value = "project")]
    pub category: Category,

    /// Short title (default: the first sentence of the content).
    #[arg(short, long)]
    pub title: Option<String>,

    /// 1 (background) to 5 (always inject).
    #[arg(short = 'P', long)]
    pub priority: Option<i64>,

    /// Tag, repeatable or comma-separated.
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,

    /// Related file, repeatable.
    #[arg(long = "file", value_name = "PATH")]
    pub files: Vec<String>,

    /// Commit this memory relates to (`HEAD` resolves to the current commit).
    #[arg(long)]
    pub commit: Option<String>,

    /// Code symbol this memory is about.
    #[arg(long)]
    pub symbol: Option<String>,

    /// Store globally rather than against the current project.
    #[arg(short, long)]
    pub global: bool,

    /// Id (or prefix) of a memory this one replaces.
    #[arg(long, value_name = "ID")]
    pub supersedes: Option<String>,
}

/// `contextd edit`
#[derive(Debug, Args)]
pub struct EditArgs {
    /// Memory id or unique prefix.
    pub id: String,

    #[arg(short, long)]
    pub title: Option<String>,

    /// Replacement content.
    #[arg(short = 'm', long, value_name = "TEXT")]
    pub content: Option<String>,

    #[arg(short, long)]
    pub category: Option<Category>,

    #[arg(short = 'P', long)]
    pub priority: Option<i64>,

    /// active, superseded, deprecated or archived.
    #[arg(short, long)]
    pub status: Option<Status>,

    /// Replace all tags.
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,

    /// Replace all related files.
    #[arg(long = "file", value_name = "PATH")]
    pub files: Vec<String>,
}

/// `contextd delete`
#[derive(Debug, Args)]
pub struct DeleteArgs {
    /// Memory id or unique prefix.
    pub id: String,

    /// Archive instead of deleting, keeping the record.
    #[arg(long)]
    pub archive: bool,
}

/// `contextd show`
#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Memory id or unique prefix.
    pub id: String,
}

/// `contextd memories`
#[derive(Debug, Args)]
pub struct ListMemoriesArgs {
    /// Filter by category, repeatable or comma-separated.
    #[arg(short, long = "category", value_name = "CATEGORY")]
    pub categories: Vec<String>,

    /// Filter by status (default: active only).
    #[arg(short, long = "status", value_name = "STATUS")]
    pub statuses: Vec<String>,

    /// Filter by tag, repeatable.
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,

    /// Substring match on title or content.
    #[arg(long, value_name = "TEXT")]
    pub grep: Option<String>,

    /// Include superseded, deprecated and archived memories.
    #[arg(long)]
    pub all: bool,

    /// Only global memories.
    #[arg(short, long)]
    pub global: bool,

    /// Maximum rows.
    #[arg(short = 'n', long, default_value_t = 30)]
    pub limit: usize,

    /// Sort by priority instead of recency.
    #[arg(long)]
    pub by_priority: bool,
}

/// `contextd supersede`
#[derive(Debug, Args)]
pub struct SupersedeArgs {
    /// The memory that is no longer current.
    pub old: String,
    /// The memory that replaces it.
    pub new: String,
}

/// Record a memory.
pub async fn add(app: &App, global: &GlobalArgs, args: &AddArgs) -> Result<()> {
    let project = if args.global { None } else { app.resolve_project(global.project.as_deref())? };
    if project.is_none() && !args.global {
        // Global storage should be a choice, not an accident.
        return Err(Error::NoProjectHere(app.cwd().to_path_buf()));
    }

    let commit = resolve_commit(app, args.commit.as_deref());
    let memory = MemoryService::new(app).add(NewMemory {
        project: project.clone(),
        category: args.category,
        title: args.title.clone(),
        priority: args.priority,
        tags: args.tags.clone(),
        files: args.files.clone(),
        commit,
        symbol: args.symbol.clone(),
        supersedes: args.supersedes.clone(),
        ..NewMemory::new(args.category, args.content.join(" "))
    })?;

    // Index immediately so the memory is recallable in the same session.
    let indexed = IndexService::new(app)
        .embed_record(&RecordRef::memory(&memory.id))
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, "could not embed new memory");
            false
        });

    #[derive(Serialize)]
    struct AddOutput {
        memory: Memory,
        embedded: bool,
    }
    let output = AddOutput { memory: memory.clone(), embedded: indexed };

    output::render(global, &output, || {
        let scope = project.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| "global".into());
        let mut text = format!(
            "{}\n",
            ui::ok(&format!("Remembered in {} [{}]", ui::bold(&scope), memory.category))
        );
        text.push_str(&ui::kv(&[
            ("id", ids::short(&memory.id).to_string()),
            ("title", memory.title.clone()),
            ("priority", memory.priority.to_string()),
        ]));
        if args.supersedes.is_some() {
            text.push_str(&format!(
                "\n{}",
                ui::hint("The previous memory is now marked superseded.")
            ));
        }
        text
    })
}

/// Change a memory.
pub async fn edit(app: &App, global: &GlobalArgs, args: &EditArgs) -> Result<()> {
    let patch = MemoryPatch {
        title: args.title.clone(),
        content: args.content.clone(),
        category: args.category,
        priority: args.priority,
        status: args.status,
        tags: (!args.tags.is_empty()).then(|| args.tags.clone()),
        files: (!args.files.is_empty()).then(|| args.files.clone()),
        commit: None,
        symbol: None,
    };
    let memory = MemoryService::new(app).edit(&args.id, patch)?;
    let _ = IndexService::new(app).embed_record(&RecordRef::memory(&memory.id)).await;

    output::render(global, &memory, || {
        format!("{}\n{}", ui::ok("Updated."), output::memory_detail(&memory))
    })
}

/// Delete or archive a memory.
pub async fn delete(app: &App, global: &GlobalArgs, args: &DeleteArgs) -> Result<()> {
    let service = MemoryService::new(app);
    let memory =
        if args.archive { service.archive(&args.id)? } else { service.delete(&args.id)? };

    // An external vector store has to be told; the SQLite one holds the
    // vectors that were just removed with the record itself.
    let indexer = IndexService::new(app);
    if args.archive {
        // Archived records stay stored but are never retrieved, so the index
        // payload is refreshed rather than dropped.
        let _ = indexer.embed_record(&RecordRef::memory(&memory.id)).await;
    } else {
        indexer.forget_record(&RecordRef::memory(&memory.id)).await?;
    }

    output::render(global, &memory, || {
        if args.archive {
            format!(
                "{}\n{}",
                ui::ok(&format!("Archived “{}”.", memory.title)),
                ui::hint("Archived memories stay searchable with `contextd memories --all`.")
            )
        } else {
            ui::ok(&format!("Deleted “{}”.", memory.title))
        }
    })
}

/// Show one memory.
pub fn show(app: &App, global: &GlobalArgs, args: &ShowArgs) -> Result<()> {
    let memory = MemoryService::new(app).get(&args.id)?;
    output::render(global, &memory, || output::memory_detail(&memory))
}

/// List memories.
pub fn list(app: &App, global: &GlobalArgs, args: &ListMemoriesArgs) -> Result<()> {
    let project = app.resolve_project(global.project.as_deref())?;
    let scope = if args.global {
        ProjectScope::GlobalOnly
    } else {
        match &project {
            Some(p) => ProjectScope::ProjectWithGlobal(p.id.clone()),
            None => ProjectScope::Any,
        }
    };

    let statuses = if args.all { Status::ALL.to_vec() } else { parse_statuses(&args.statuses)? };

    let memories = MemoryService::new(app).list(&MemoryFilter {
        categories: parse_categories(&args.categories)?,
        statuses,
        tags: args.tags.clone(),
        contains: args.grep.clone(),
        order: if args.by_priority { MemoryOrder::PriorityFirst } else { MemoryOrder::RecentFirst },
        limit: Some(args.limit),
        ..MemoryFilter::for_scope(scope)
    })?;

    output::render(global, &memories, || output::memories_table(&memories))
}

/// Mark one memory as replaced by another.
pub fn supersede(app: &App, global: &GlobalArgs, args: &SupersedeArgs) -> Result<()> {
    let (old, new) = MemoryService::new(app).supersede(&args.old, &args.new)?;

    #[derive(Serialize)]
    struct SupersedeOutput {
        superseded: Memory,
        current: Memory,
    }
    let output = SupersedeOutput { superseded: old.clone(), current: new.clone() };

    output::render(global, &output, || {
        format!(
            "{}\n{}",
            ui::ok("History recorded."),
            ui::kv(&[
                ("was", format!("{} {}", ui::dim(ids::short(&old.id)), old.title)),
                ("now", format!("{} {}", ui::dim(ids::short(&new.id)), new.title)),
            ])
        )
    })
}

/// Turn `HEAD` into a real commit hash; anything else is passed through.
fn resolve_commit(app: &App, commit: Option<&str>) -> Option<String> {
    let commit = commit?;
    if !commit.eq_ignore_ascii_case("head") {
        return Some(commit.to_string());
    }
    let snapshot = crate::util::git::GitSnapshot::capture(app.cwd());
    snapshot.commit
}
