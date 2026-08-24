//! Model Context Protocol server.
//!
//! The MCP layer is a *client* of [`crate::core`], exactly like the CLI: it
//! adds a transport and a tool schema, and holds no memory logic of its own.
//! That is what keeps "MCP" from becoming entangled with storage — swapping
//! the transport, or dropping MCP entirely, would not touch a line below this
//! module.

pub mod protocol;
pub mod server;
pub mod tools;

pub use server::{McpServer, ServerOptions};
