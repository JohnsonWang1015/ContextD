//! ContextD — developer context and semantic memory for AI coding agents.
//!
//! # Layering
//!
//! ```text
//! cli / mcp            entry points (thin)
//!   ↓
//! agents               per-agent import/export adapters
//!   ↓
//! core                 projects, memories, checkpoints, context building
//!   ↓
//! search / embeddings  retrieval, pluggable providers
//!   ↓
//! storage              repository traits + SQLite implementation
//! ```
//!
//! Each layer depends only on the ones below it. In particular nothing above
//! `storage` mentions SQLite, nothing above `embeddings` names a provider, and
//! the MCP server is a client of `core` exactly like the CLI is.

pub mod agents;
pub mod app;
pub mod cli;
pub mod config;
pub mod core;
pub mod embeddings;
pub mod error;
pub mod mcp;
pub mod search;
pub mod storage;
pub mod sync;
pub mod ui;
pub mod util;

pub use app::App;
pub use config::{Config, Paths};
pub use error::{Error, Result};

/// Crate version, surfaced by `contextd --version` and the MCP handshake.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
