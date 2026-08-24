# ContextD

**Developer context & semantic memory manager for AI coding agents.**

You explain the same things to Claude Code, Codex and Cursor every session: what
this project is, why the queue is NATS and not Redis, that you format with
`rustfmt` before committing, and where you left off last night. ContextD stores
that once — across projects and across agents — and hands back only the parts
that matter for the task at hand, through a CLI and an MCP server.

```
Claude Code ─┐
Codex ───────┤
Cursor ──────┼── MCP ── ContextD ── SQLite + FTS5 + embeddings
other agents ┘
```

## Two rules the design follows

**Store everything, inject only what matters.** A year of memory does not fit in
a context window. Retrieval is hybrid (full-text + vector), ranked, and packed
into an explicit token budget; what did not fit is counted, never silently
dropped.

**Current truth must be distinguishable from historical truth.** When the task
queue moves Redis → PostgreSQL → NATS, an agent must be told *NATS*, not the
option that happens to be mentioned most often. Superseded memories keep their
content and stay searchable, but they are marked, penalised in ranking, and
excluded from retrieval unless asked for.

## Install

```sh
cargo install --path .          # or: cargo build --release
```

Requires Rust 1.85+. SQLite is compiled in — no system libraries, no Docker.
Linux, macOS and Windows.

## Quick start

```sh
contextd init                                   # create ~/.contextd

cd ~/projects/FerroGrid
contextd attach                                 # detects git, name, agent files

contextd add --category architecture \
  "GPU scheduler uses NATS for task transport"

contextd checkpoint "worker heartbeat completed" \
  --goal "Implement distributed GPU scheduling" \
  --done Coordinator --next "Lease-based GPU allocation" \
  --problem "Worker reconnect"

contextd search "scheduler"                     # keyword search, ranked
contextd recall "which message transport does the scheduler use?"

contextd export claude                          # writes CLAUDE.md
contextd export codex                           # writes AGENTS.md
contextd status
contextd mcp serve                              # speak MCP on stdio
```

`contextd status`:

```
ContextD
─────────────────────────────────
Project      FerroGrid
Branch       main @ a1b2c3d (2 dirty)
Memories     124
Decisions    18
Checkpoints  7

Last checkpoint
worker heartbeat completed (2 hours ago)

Current goal
Implement distributed GPU scheduling

Next
- Lease-based GPU allocation

Semantic index  ✓ 149/149  local · hashing-v1
Agents          claude, codex
MCP             ✓ contextd mcp serve
```

## Commands

| Command | What it does |
|---|---|
| `init` | Create the home directory, database and config |
| `attach` / `detach` / `list` | Track a repository as a project |
| `status` | Counts, git state, latest checkpoint, index health |
| `add` / `edit` / `delete` / `show` / `memories` | Memory CRUD |
| `supersede <old> <new>` | Record that one memory replaced another |
| `search` | Keyword-first search across memories, ADRs and checkpoints |
| `recall` | Ask a question; hybrid semantic + keyword retrieval |
| `checkpoint` / `resume` | Save and restore "where was I?" |
| `decision add/list/show/supersede` | Architecture decision records |
| `refresh` | Merge duplicates, mark history, rebuild indexes |
| `sync` | Write the Markdown mirror and bound agent files |
| `import` / `export <agent>` | Move context in and out of agent files |
| `mcp serve` / `mcp tools` | Run the MCP server; list its tools |
| `config` | Show paths and settings |

Every command takes `--json` for scripting, `--project <name>` to act on another
project, and `--home <dir>` (or `$CONTEXTD_HOME`) to point at a different store.

## MCP

```sh
contextd mcp serve            # newline-delimited JSON-RPC on stdio
contextd mcp serve --read-only
```

Register it with any MCP client — for Claude Code:

```sh
claude mcp add contextd -- contextd mcp serve
```

Tools exposed:

| Tool | Use |
|---|---|
| `project_context` | Start-of-session context, budgeted to a token limit |
| `semantic_recall` | Answer a question from memory (hybrid retrieval) |
| `memory_search` | Keyword-first search |
| `memory_get` | One memory in full |
| `project_status` | Counts, branch, index state |
| `checkpoint_latest` | Current goal, done, next, open problems |
| `architecture_decisions` | Decisions that currently hold |
| `memory_add`, `checkpoint_create` | Writes (omitted in `--read-only`) |

Results carry lifecycle status, and anything superseded is labelled
`NOT current` so a model does not mistake history for the present design.

## How retrieval works

```
query → project detection → FTS5 → semantic → ranking → token budget → context
```

The score of a candidate is a weighted sum, multiplied by a lifecycle factor:

```
(fts + semantic + priority + recency + project_match) × status_multiplier
```

Every weight lives in `config.toml`, and the scorer is a trait
(`search::scoring::Scorer`) so the formula can be replaced without touching
retrieval. `contextd search --explain` prints the breakdown per hit.

## Embeddings

The default provider is **local**: an offline feature-hashing embedder — no
model download, no network, no API key. It captures lexical overlap and
phrasing, which is enough for hybrid retrieval to beat keywords alone, but it
cannot relate words that never co-occur.

For real paraphrase matching, point ContextD at any OpenAI-compatible endpoint
(OpenAI, Ollama, vLLM, LM Studio, a gateway):

```toml
[embeddings]
provider    = "openai"
model       = "text-embedding-3-small"
api_base    = "https://api.openai.com/v1"   # or http://localhost:11434/v1
api_key_env = "OPENAI_API_KEY"              # the key is read from the env, never stored
```

Then `contextd refresh` re-embeds what changed. `provider = "none"` disables
vectors entirely and ContextD falls back to full-text search.

## Storage layout

SQLite is the source of truth. The Markdown mirror exists so you can read, diff
and commit your memory:

```
~/.contextd/
├── config.toml
├── contextd.db
├── projects/FerroGrid/
│   ├── overview.md  architecture.md  decisions.md  tasks.md
│   └── checkpoints/
└── global/
    ├── coding.md  git.md  preferences.md
```

## Your files are yours

Generated content lives inside a marked block:

```markdown
# House rules            ← yours, never touched
Never force-push to main.

<!-- contextd:begin -->
...generated context...  ← ContextD's
<!-- contextd:end -->
```

ContextD records a hash of what it wrote. If the block changed since then,
`contextd export` refuses and exits non-zero until you pass `--force`. The same
applies to the Markdown mirror, where `contextd sync --adopt` turns your hand
edits into memories instead of discarding them.

## Architecture

```
cli / mcp            entry points (thin)
  ↓
agents               per-agent import/export adapters
  ↓
core                 projects, memories, checkpoints, context building
  ↓
search / embeddings  retrieval, pluggable providers
  ↓
storage              repository traits + SQLite implementation
```

Each layer depends only on the ones below. Nothing above `storage` mentions
SQLite, nothing above `embeddings` names a provider, and the MCP server is a
client of `core` exactly as the CLI is — so the planned evolution (SQLite → FTS
→ embeddings → semantic memory → MCP) does not turn into one tangled module.

```
src/
├── cli/          argument parsing, rendering, one module per command group
├── core/         model, project, memory, checkpoint, decision, context, refresh
├── storage/      repository traits + sqlite/ (migrations, FTS, vectors)
├── search/       fulltext, semantic, hybrid fusion, scoring, indexer
├── embeddings/   EmbeddingProvider trait, local, openai-compatible
├── agents/       AgentAdapter trait, claude, codex, cursor, generic
├── sync/         agent files, Markdown mirror, conflict detection
├── mcp/          JSON-RPC protocol, tools, stdio server
├── config/       config.toml, path resolution
└── ui/           terminal formatting
```

## Development

```sh
cargo fmt
cargo clippy --all-targets
cargo test              # unit + CLI + MCP + migration tests
```

Tests run against temporary `CONTEXTD_HOME` directories and never touch your
real memory store.

## Configuration

`contextd config` prints paths and current settings; `contextd config --toml`
prints the file. Notable knobs:

```toml
[context]
max_context_tokens = 6000    # the injection budget
max_memories       = 40

[search]
fts_weight = 1.0
semantic_weight = 1.0
priority_weight = 0.35
recency_weight = 0.25
project_weight = 0.5
recency_half_life_days = 90.0
superseded_penalty = 0.35    # how far history is pushed below current truth

[refresh]
duplicate_threshold = 0.9    # at or above this, memories are merged
similar_threshold   = 0.65   # at or above this, they are reported
summarizer          = "none" # or "openai" to consolidate clusters
```

## Status

Working today: projects, memories, checkpoints, decisions, FTS5 search, hybrid
semantic recall, context budgeting, Claude/Codex/Cursor/generic adapters,
Markdown mirror with conflict detection, refresh, and the MCP server.

Planned: ANN indexing for very large stores (the repository trait already
allows it), richer conflict resolution in `refresh`, and more agent adapters.

## Licence

MIT — see [LICENSE](LICENSE).
