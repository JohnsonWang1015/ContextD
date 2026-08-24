//! One module per command group. Each function takes the resolved [`App`],
//! the global flags and its own arguments, and prints the result.
//!
//! [`App`]: crate::app::App

pub mod agent;
pub mod checkpoint;
pub mod config;
pub mod decision;
pub mod maintenance;
pub mod mcp;
pub mod memory;
pub mod project;
pub mod remote;
pub mod search;
pub mod session;
