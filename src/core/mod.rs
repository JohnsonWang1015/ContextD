//! Domain layer: types and the services that operate on them.
//!
//! Services depend on [`crate::storage::repository::Storage`], never on
//! SQLite, and know nothing about the CLI, MCP or any particular agent.

pub mod checkpoint;
pub mod context;
pub mod decision;
pub mod memory;
pub mod model;
pub mod project;
pub mod refresh;
