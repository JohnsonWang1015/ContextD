//! The MCP server loop (stdio transport).
//!
//! Reads newline-delimited JSON-RPC from stdin and writes replies to stdout.
//! Nothing else may write to stdout while the server runs — logs go to stderr,
//! because a stray `println!` would corrupt the protocol stream.

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::app::App;
use crate::error::Result;
use crate::mcp::protocol::{negotiate_version, Request, Response, RpcError, ToolResult};
use crate::mcp::tools;

/// Server configuration.
#[derive(Debug, Clone, Default)]
pub struct ServerOptions {
    /// Project used when a tool call does not name one.
    pub default_project: Option<String>,
    /// Refuse tools that write.
    pub read_only: bool,
}

/// Handles one MCP session.
pub struct McpServer {
    app: App,
    options: ServerOptions,
    initialized: bool,
}

impl McpServer {
    pub fn new(app: App, options: ServerOptions) -> Self {
        Self { app, options, initialized: false }
    }

    /// Serve until stdin closes.
    pub async fn serve_stdio(mut self) -> Result<()> {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut stdout = tokio::io::stdout();
        let mut line = String::new();

        tracing::info!(
            project = ?self.options.default_project,
            read_only = self.options.read_only,
            "contextd mcp server ready on stdio"
        );

        loop {
            line.clear();
            let read = reader.read_line(&mut line).await?;
            if read == 0 {
                tracing::info!("stdin closed, shutting down");
                return Ok(());
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let Some(response) = self.handle_line(trimmed).await else {
                continue; // notification: no reply
            };
            let mut encoded = serde_json::to_string(&response)?;
            encoded.push('\n');
            stdout.write_all(encoded.as_bytes()).await?;
            stdout.flush().await?;
        }
    }

    /// Handle one line, returning the response to write (if any).
    pub async fn handle_line(&mut self, line: &str) -> Option<Response> {
        let request: Request = match serde_json::from_str(line) {
            Ok(request) => request,
            Err(err) => {
                return Some(Response::failure(
                    None,
                    RpcError::new(RpcError::PARSE_ERROR, format!("invalid JSON: {err}")),
                ));
            }
        };

        if request.is_notification() {
            self.handle_notification(&request);
            return None;
        }

        let id = request.id.clone();
        match self.handle_request(&request).await {
            Ok(result) => Some(Response::success(id, result)),
            Err(error) => Some(Response::failure(id, error)),
        }
    }

    fn handle_notification(&mut self, request: &Request) {
        match request.method.as_str() {
            "notifications/initialized" => {
                self.initialized = true;
                tracing::debug!("client reported initialized");
            }
            other => tracing::debug!(method = other, "ignoring notification"),
        }
    }

    async fn handle_request(&mut self, request: &Request) -> std::result::Result<Value, RpcError> {
        match request.method.as_str() {
            "initialize" => Ok(self.initialize(request.params.as_ref())),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({
                "tools": tools::specs(self.options.read_only)
            })),
            "tools/call" => self.call_tool(request.params.as_ref()).await,
            // Declared capabilities do not include these, but some clients ask
            // anyway; an empty list is friendlier than an error.
            "resources/list" => Ok(json!({"resources": []})),
            "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
            "prompts/list" => Ok(json!({"prompts": []})),
            other => Err(RpcError::method_not_found(other)),
        }
    }

    fn initialize(&mut self, params: Option<&Value>) -> Value {
        let requested = params.and_then(|p| p.get("protocolVersion")).and_then(Value::as_str);
        let client = params
            .and_then(|p| p.get("clientInfo"))
            .and_then(|c| c.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        tracing::info!(client, "initialize");

        json!({
            "protocolVersion": negotiate_version(requested),
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "contextd", "version": crate::VERSION},
            "instructions": "ContextD holds this developer's long-term engineering memory. \
                             Call project_context at the start of a session, semantic_recall \
                             when you need a specific fact, and checkpoint_latest to find out \
                             where the work stands. Records marked 'NOT current' are history: \
                             do not treat them as the present design."
        })
    }

    async fn call_tool(&self, params: Option<&Value>) -> std::result::Result<Value, RpcError> {
        let params = params.ok_or_else(|| RpcError::invalid_params("missing params"))?;
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("missing tool name"))?;
        let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

        let result = tools::call(
            &self.app,
            self.options.default_project.as_deref(),
            self.options.read_only,
            name,
            &arguments,
        )
        .await
        // A tool that fails is reported to the model as a tool error, not as a
        // protocol error: the model can then correct its arguments instead of
        // the client tearing down the session.
        .unwrap_or_else(|err| ToolResult::error(err.to_string()));

        serde_json::to_value(result).map_err(|err| RpcError::internal(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::core::memory::{MemoryService, NewMemory};
    use crate::core::model::{Category, Project};
    use crate::core::project::{AttachRequest, ProjectService};

    fn fixture() -> (tempfile::TempDir, App, Project) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let app =
            App::open_or_create(Paths::with_root(dir.path().join("home"))).unwrap().with_cwd(&repo);
        let (project, _) = ProjectService::new(&app)
            .attach(AttachRequest {
                dir: repo,
                name: Some("FerroGrid".into()),
                description: None,
                bindings: vec![],
            })
            .unwrap();
        (dir, app, project)
    }

    fn server(app: &App, read_only: bool) -> McpServer {
        McpServer::new(
            app.clone(),
            ServerOptions { default_project: Some("FerroGrid".into()), read_only },
        )
    }

    async fn call(server: &mut McpServer, id: i64, method: &str, params: Value) -> Value {
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let response = server.handle_line(&line.to_string()).await.expect("response");
        serde_json::to_value(response).unwrap()
    }

    async fn tool(server: &mut McpServer, name: &str, arguments: Value) -> Value {
        call(server, 9, "tools/call", json!({"name": name, "arguments": arguments})).await
    }

    #[tokio::test]
    async fn initialize_and_list_tools() {
        let (_dir, app, _project) = fixture();
        let mut server = server(&app, false);

        let response = call(
            &mut server,
            1,
            "initialize",
            json!({"protocolVersion": "2024-11-05", "clientInfo": {"name": "test"}}),
        )
        .await;
        assert_eq!(response["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(response["result"]["serverInfo"]["name"], "contextd");

        let listed = call(&mut server, 2, "tools/list", json!({})).await;
        let names: Vec<String> = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        for expected in [
            "memory_search",
            "memory_get",
            "project_context",
            "project_status",
            "checkpoint_latest",
            "architecture_decisions",
            "semantic_recall",
        ] {
            assert!(names.contains(&expected.to_string()), "missing tool {expected}");
        }
    }

    #[tokio::test]
    async fn notifications_get_no_reply() {
        let (_dir, app, _project) = fixture();
        let mut server = server(&app, false);
        let line = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(server.handle_line(&line.to_string()).await.is_none());
        assert!(server.initialized);
    }

    #[tokio::test]
    async fn malformed_input_returns_a_parse_error() {
        let (_dir, app, _project) = fixture();
        let mut server = server(&app, false);
        let response = server.handle_line("{not json").await.unwrap();
        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["error"]["code"], RpcError::PARSE_ERROR);
    }

    #[tokio::test]
    async fn unknown_method_is_reported() {
        let (_dir, app, _project) = fixture();
        let mut server = server(&app, false);
        let response = call(&mut server, 3, "does/not/exist", json!({})).await;
        assert_eq!(response["error"]["code"], RpcError::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn semantic_recall_finds_the_current_answer() {
        let (_dir, app, project) = fixture();
        let memories = MemoryService::new(&app);
        let redis = memories
            .add(NewMemory {
                project: Some(project.clone()),
                ..NewMemory::new(Category::Architecture, "Task queue transport is Redis streams")
            })
            .unwrap();
        memories
            .add(NewMemory {
                project: Some(project.clone()),
                supersedes: Some(redis.id),
                ..NewMemory::new(
                    Category::Architecture,
                    "After evaluating Redis and PostgreSQL LISTEN/NOTIFY, the scheduler \
                     transport was migrated to NATS",
                )
            })
            .unwrap();
        crate::search::IndexService::new(&app)
            .embed_pending(&crate::storage::repository::ProjectScope::Any, false)
            .await
            .unwrap();

        let mut server = server(&app, false);
        let response = tool(
            &mut server,
            "semantic_recall",
            json!({"query": "which message transport does the scheduler use?"}),
        )
        .await;
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("NATS"), "unexpected recall result: {text}");
        assert!(!text.contains("Redis streams"), "superseded memory must not be returned");
    }

    #[tokio::test]
    async fn project_context_is_budgeted() {
        let (_dir, app, project) = fixture();
        let memories = MemoryService::new(&app);
        for i in 0..30 {
            memories
                .add(NewMemory {
                    project: Some(project.clone()),
                    ..NewMemory::new(
                        Category::Knowledge,
                        format!("Memory {i}: {}", "detail ".repeat(60)),
                    )
                })
                .unwrap();
        }

        let mut server = server(&app, false);
        let response = tool(&mut server, "project_context", json!({"max_tokens": 400})).await;
        let structured = &response["result"]["structuredContent"];
        assert!(structured["budget"]["used_tokens"].as_u64().unwrap() <= 400);
        assert!(structured["budget"]["dropped"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn write_tools_are_blocked_in_read_only_mode() {
        let (_dir, app, _project) = fixture();
        let mut server = server(&app, true);

        let listed = call(&mut server, 1, "tools/list", json!({})).await;
        let names: Vec<String> = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(!names.contains(&"memory_add".to_string()));

        let response = tool(&mut server, "memory_add", json!({"content": "x"})).await;
        assert_eq!(response["result"]["isError"], true);
    }

    #[tokio::test]
    async fn memory_add_then_get_roundtrips() {
        let (_dir, app, _project) = fixture();
        let mut server = server(&app, false);
        let added = tool(
            &mut server,
            "memory_add",
            json!({"content": "Workers renew GPU leases every 30s", "category": "architecture"}),
        )
        .await;
        let id = added["result"]["structuredContent"]["id"].as_str().unwrap().to_string();

        let fetched = tool(&mut server, "memory_get", json!({"id": &id[..8]})).await;
        let text = fetched["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("renew GPU leases"));
    }

    #[tokio::test]
    async fn bad_arguments_become_tool_errors_not_protocol_errors() {
        let (_dir, app, _project) = fixture();
        let mut server = server(&app, false);
        let response = tool(&mut server, "memory_search", json!({})).await;
        assert!(response["error"].is_null());
        assert_eq!(response["result"]["isError"], true);

        let unknown = tool(&mut server, "no_such_tool", json!({})).await;
        assert_eq!(unknown["result"]["isError"], true);
    }

    #[tokio::test]
    async fn checkpoint_tools_report_current_state() {
        let (_dir, app, _project) = fixture();
        let mut server = server(&app, false);
        tool(
            &mut server,
            "checkpoint_create",
            json!({
                "summary": "worker heartbeat completed",
                "goal": "Implement distributed GPU scheduling",
                "next_steps": ["Lease-based GPU allocation"]
            }),
        )
        .await;

        let latest = tool(&mut server, "checkpoint_latest", json!({})).await;
        let text = latest["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("worker heartbeat completed"));
        assert!(text.contains("Lease-based GPU allocation"));
    }
}
