//! JSON-RPC 2.0 types for the Model Context Protocol.
//!
//! MCP's stdio transport is newline-delimited JSON-RPC. The message shapes are
//! small and stable, so ContextD implements them directly rather than taking a
//! dependency that would tie the server's lifecycle to someone else's release
//! cadence.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Protocol revision this server implements.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Revisions this server will accept from a client.
pub const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// An incoming message. Requests carry an `id`; notifications do not.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

impl Request {
    /// A notification expects no reply.
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// An outgoing message.
#[derive(Debug, Clone, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn success(id: Option<Value>, result: Value) -> Self {
        Self { jsonrpc: "2.0", id, result: Some(result), error: None }
    }

    pub fn failure(id: Option<Value>, error: RpcError) -> Self {
        Self { jsonrpc: "2.0", id, result: None, error: Some(error) }
    }
}

/// JSON-RPC error object.
#[derive(Debug, Clone, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;

    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(Self::METHOD_NOT_FOUND, format!("unknown method `{method}`"))
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(Self::INVALID_PARAMS, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(Self::INTERNAL_ERROR, message)
    }
}

/// Tool advertised by `tools/list`.
#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// Result of `tools/call`.
#[derive(Debug, Clone, Serialize)]
pub struct ToolResult {
    pub content: Vec<Content>,
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    /// Machine-readable payload, so a client does not have to parse prose.
    #[serde(rename = "structuredContent", skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
}

impl ToolResult {
    /// Text plus the same data in structured form.
    pub fn text(text: impl Into<String>, structured: Option<Value>) -> Self {
        Self { content: vec![Content::text(text)], is_error: None, structured_content: structured }
    }

    /// A failure the model should see and can react to.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![Content::text(message)],
            is_error: Some(true),
            structured_content: None,
        }
    }
}

/// A content block.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum Content {
    #[serde(rename = "text")]
    Text { text: String },
}

impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Content::Text { text: text.into() }
    }
}

/// Negotiate a protocol version with the client.
///
/// Echoing back a version the client asked for keeps older clients working;
/// an unknown version falls back to ours, which the client may then reject.
pub fn negotiate_version(requested: Option<&str>) -> &'static str {
    match requested {
        Some(version) => SUPPORTED_VERSIONS
            .iter()
            .find(|supported| **supported == version)
            .copied()
            .unwrap_or(PROTOCOL_VERSION),
        None => PROTOCOL_VERSION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requests_and_notifications() {
        let request: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap();
        assert!(!request.is_notification());
        assert_eq!(request.method, "tools/list");

        let notification: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .unwrap();
        assert!(notification.is_notification());
    }

    #[test]
    fn responses_omit_empty_fields() {
        let ok =
            serde_json::to_value(Response::success(Some(1.into()), serde_json::json!({}))).unwrap();
        assert!(ok.get("error").is_none());
        let err = serde_json::to_value(Response::failure(
            Some(1.into()),
            RpcError::method_not_found("nope"),
        ))
        .unwrap();
        assert!(err.get("result").is_none());
        assert_eq!(err["error"]["code"], RpcError::METHOD_NOT_FOUND);
    }

    #[test]
    fn version_negotiation() {
        assert_eq!(negotiate_version(Some("2024-11-05")), "2024-11-05");
        assert_eq!(negotiate_version(Some("1999-01-01")), PROTOCOL_VERSION);
        assert_eq!(negotiate_version(None), PROTOCOL_VERSION);
    }

    #[test]
    fn tool_result_shapes() {
        let ok = serde_json::to_value(ToolResult::text("hello", None)).unwrap();
        assert_eq!(ok["content"][0]["type"], "text");
        assert!(ok.get("isError").is_none());

        let err = serde_json::to_value(ToolResult::error("boom")).unwrap();
        assert_eq!(err["isError"], true);
    }
}
