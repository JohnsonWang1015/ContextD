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
| `remote add/list/remove` | Machines to exchange memory with |
| `remote pull` / `remote push` | Sync memory over SSH, record by record |
| `bundle export/import` | The same exchange as a JSON file |
| `mcp serve` / `mcp tools` | Run the MCP server; list its tools |
| `config` | Show paths and settings; `set`, `get`, `--check` |

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

## Several machines

Working on a laptop and a workstation used to mean two disjoint memories.
ContextD exchanges *records*, not files:

```sh
contextd remote add lab dev@lab-box          # a Host alias from ~/.ssh/config works too
contextd remote pull lab                     # bring their memory here
contextd remote push lab                     # send yours there
contextd remote pull lab --dry-run           # see what would change first
```

`pull` runs `contextd bundle export` on the far side over SSH and merges what
comes back. Merging is by UUID, so:

- running it twice changes nothing the second time;
- where a record exists on both sides, the newer `updated_at` wins;
- where **both** sides changed, the local copy is kept and the divergence is
  listed rather than silently resolved;
- supersede links travel, so history closed on one machine stays closed on the
  other.

Copying `contextd.db` around was rejected deliberately: two machines that both
recorded something since the last exchange must both keep their work, and a
file copy can only pick a winner.

Projects are matched across machines by git remote (SSH and HTTPS URL forms are
treated as the same repository), then by slug. A project that arrives from
elsewhere has no local path; running `contextd attach` in your checkout adopts
it instead of creating a second project for the same code.

No SSH? The same exchange works through a file:

```sh
contextd bundle export --out memory.json     # on one machine
contextd bundle import --file memory.json    # on the other
```

Embeddings are not shipped — they are derived, the other machine may use a
different provider, and a pull re-embeds locally faster than the transfer would
take.

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
(Ollama, TEI, vLLM, LM Studio, OpenAI itself). **bge-m3** is a good default:
multilingual, so a question in Chinese finds a memory written in English.

```sh
ollama pull bge-m3
contextd config set embeddings.provider   openai
contextd config set embeddings.model      bge-m3
contextd config set embeddings.api_base   http://localhost:11434/v1
contextd config set embeddings.dimensions 1024
contextd config --check                       # asks the endpoint for a real vector
contextd refresh --force-embeddings           # re-embed with the new model
```

The API key, when one is needed, is read from the environment variable named in
`embeddings.api_key_env` — never written to the config file or the database.
`provider = "none"` disables vectors entirely and ContextD falls back to
full-text search.

## Vector store

Vectors are searched through a `VectorIndex` trait with two backends:

| Backend | When |
|---|---|
| `sqlite` (default) | Brute-force cosine over the vectors already in the database. Nothing to install, sub-millisecond at personal scale. |
| `qdrant` | You already run Qdrant, or your memory has outgrown a scan. |

```sh
contextd config set vector.backend    qdrant
contextd config set vector.url        http://localhost:6333
contextd config set vector.collection contextd
contextd refresh --reindex-vectors            # publish existing vectors, no re-embedding
contextd config --check
```

The collection is created on first use, sized from the embedding model and
using cosine distance; an existing collection of the wrong width (switching a
384-dimension model for bge-m3's 1024, say) is reported with the command that
fixes it rather than producing meaningless neighbours.

SQLite keeps the authoritative copy of every vector whichever backend is
selected, so an external index can always be rebuilt, `contextd bundle` keeps
working, and a machine without Qdrant can still read the same memory.

If the vector store or the embedding endpoint is unreachable, retrieval falls
back to full-text search and says so — `contextd status` shows the backend and
whether it answers.

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
│   └── vector/   VectorIndex trait, sqlite scan, qdrant client
├── embeddings/   EmbeddingProvider trait, local, openai-compatible
├── agents/       AgentAdapter trait, claude, codex, cursor, generic
├── sync/         agent files, Markdown mirror, bundles, SSH remotes
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

[vector]
backend    = "sqlite"        # or "qdrant"
url        = "http://localhost:6333"
collection = "contextd"

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
Markdown mirror with conflict detection, refresh, cross-machine sync over SSH,
pluggable embedding providers (local or any OpenAI-compatible endpoint),
pluggable vector stores (SQLite or Qdrant), and the MCP server.

Planned: richer conflict resolution in `refresh`, more agent adapters, and a
scheduled background pull for machines that are usually reachable.

## Licence

MIT — see [LICENSE](LICENSE).
