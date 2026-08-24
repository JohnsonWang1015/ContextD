//! Drives `contextd mcp serve` the way an MCP client does: newline-delimited
//! JSON-RPC over the child process's stdio.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};

use common::Sandbox;
use serde_json::{json, Value};

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Session {
    fn start(sandbox: &Sandbox, extra_args: &[&str]) -> Self {
        let mut command = sandbox.cmd();
        command.arg("mcp").arg("serve").args(extra_args);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn mcp server");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self { child, stdin, stdout }
    }

    /// Send a request and read its response.
    fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{message}").expect("write request");
        self.stdin.flush().expect("flush");

        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read response");
        assert!(!line.trim().is_empty(), "server closed the stream");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad JSON from server: {e}\n{line}"))
    }

    fn notify(&mut self, method: &str) {
        let message = json!({"jsonrpc": "2.0", "method": method});
        writeln!(self.stdin, "{message}").expect("write notification");
        self.stdin.flush().expect("flush");
    }

    fn call_tool(&mut self, id: i64, name: &str, arguments: Value) -> Value {
        self.request(id, "tools/call", json!({"name": name, "arguments": arguments}))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Closing stdin is the protocol's shutdown signal.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn text_of(response: &Value) -> String {
    response["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string()
}

#[test]
fn an_mcp_client_can_retrieve_project_memory() {
    let sandbox = Sandbox::new();
    sandbox.bootstrap();
    let redis = sandbox.run_json(&["add", "-c", "architecture", "Task queue uses Redis streams"])
        ["memory"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    sandbox.run(&[
        "add",
        "-c",
        "architecture",
        "After evaluating Redis and PostgreSQL LISTEN/NOTIFY, the scheduler transport was \
         migrated to NATS",
        "--supersedes",
        &redis,
    ]);
    sandbox.run(&["decision", "add", "Use NATS JetStream", "--title", "Task transport"]);
    sandbox.run(&["checkpoint", "worker heartbeat completed", "--next", "GPU lease allocation"]);

    let mut session = Session::start(&sandbox, &[]);

    let initialize = session.request(
        1,
        "initialize",
        json!({"protocolVersion": "2025-06-18", "clientInfo": {"name": "integration-test", "version": "1"}}),
    );
    assert_eq!(initialize["result"]["serverInfo"]["name"], "contextd");
    assert_eq!(initialize["result"]["protocolVersion"], "2025-06-18");
    assert!(initialize["result"]["capabilities"]["tools"].is_object());
    session.notify("notifications/initialized");

    let tools = session.request(2, "tools/list", json!({}));
    let names: Vec<String> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect();
    for required in [
        "memory_search",
        "memory_get",
        "project_context",
        "project_status",
        "checkpoint_latest",
        "architecture_decisions",
        "semantic_recall",
    ] {
        assert!(names.contains(&required.to_string()), "missing tool {required}: {names:?}");
    }

    // Semantic recall: the question shares almost no keywords with the memory.
    let recall = session.call_tool(
        3,
        "semantic_recall",
        json!({"project": "FerroGrid", "query": "GPU worker heartbeat architecture"}),
    );
    assert!(recall["error"].is_null(), "{recall}");
    assert!(text_of(&recall).contains("heartbeat"));

    // Superseded memory must not be presented as current.
    let transport = session.call_tool(
        4,
        "semantic_recall",
        json!({"query": "which message transport does the scheduler use?"}),
    );
    let text = text_of(&transport);
    assert!(text.contains("NATS"), "recall text: {text}");
    assert!(!text.contains("Redis streams"), "history was returned as current: {text}");

    let decisions = session.call_tool(5, "architecture_decisions", json!({}));
    assert!(text_of(&decisions).contains("NATS JetStream"));

    let checkpoint = session.call_tool(6, "checkpoint_latest", json!({}));
    let checkpoint_text = text_of(&checkpoint);
    assert!(checkpoint_text.contains("worker heartbeat completed"));
    assert!(checkpoint_text.contains("GPU lease allocation"));

    let status = session.call_tool(7, "project_status", json!({}));
    assert!(text_of(&status).contains("FerroGrid"));

    // Context is budgeted, not dumped.
    let context = session.call_tool(8, "project_context", json!({"max_tokens": 800}));
    let budget = &context["result"]["structuredContent"]["budget"];
    assert!(budget["used_tokens"].as_u64().unwrap() <= 800, "budget: {budget}");
    assert!(text_of(&context).contains("FerroGrid"));

    // Protocol hygiene.
    let ping = session.request(9, "ping", json!({}));
    assert!(ping["error"].is_null());
    let unknown = session.request(10, "no/such/method", json!({}));
    assert_eq!(unknown["error"]["code"], -32601);
}

#[test]
fn an_agent_can_write_memory_and_read_it_back() {
    let sandbox = Sandbox::new();
    sandbox.bootstrap();
    let mut session = Session::start(&sandbox, &[]);
    session.request(1, "initialize", json!({"protocolVersion": "2025-06-18"}));

    let added = session.call_tool(
        2,
        "memory_add",
        json!({
            "content": "Workers renew GPU leases every 30 seconds",
            "category": "architecture",
            "tags": ["leases"]
        }),
    );
    assert!(added["result"]["isError"].is_null(), "{added}");
    let id = added["result"]["structuredContent"]["id"].as_str().unwrap().to_string();

    let fetched = session.call_tool(3, "memory_get", json!({"id": id}));
    assert!(text_of(&fetched).contains("renew GPU leases"));

    // The CLI sees it too — one store, two front ends.
    let memories = sandbox.run_json(&["memories"]);
    assert_eq!(memories.as_array().unwrap().len(), 1);
    assert_eq!(memories[0]["source"], "agent:mcp");
}

#[test]
fn read_only_servers_refuse_writes() {
    let sandbox = Sandbox::new();
    sandbox.bootstrap();
    let mut session = Session::start(&sandbox, &["--read-only"]);
    session.request(1, "initialize", json!({"protocolVersion": "2025-06-18"}));

    let tools = session.request(2, "tools/list", json!({}));
    let names: Vec<String> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect();
    assert!(!names.contains(&"memory_add".to_string()));

    let refused = session.call_tool(3, "memory_add", json!({"content": "nope"}));
    assert_eq!(refused["result"]["isError"], true);
    assert!(sandbox.run_json(&["memories"]).as_array().unwrap().is_empty());
}
