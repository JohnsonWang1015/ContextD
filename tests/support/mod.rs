//! A tiny HTTP server used to stand in for external services.
//!
//! ContextD talks to two kinds of service over HTTP: an OpenAI-compatible
//! embeddings endpoint (bge-m3 behind Ollama, TEI, vLLM, …) and a Qdrant
//! collection. Both are exercised here against a real socket, so the tests
//! cover URL construction, headers, request bodies, response parsing and error
//! handling — everything except the third-party process itself.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

/// One received request, for assertions.
#[derive(Debug, Clone)]
pub struct Received {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Value,
}

/// A server that answers ContextD's HTTP calls.
pub struct MockService {
    pub base_url: String,
    pub requests: Arc<Mutex<Vec<Received>>>,
    state: Arc<Mutex<ServiceState>>,
    shutdown: Arc<AtomicBool>,
}

#[derive(Default)]
struct ServiceState {
    /// Qdrant points by id: (vector, payload).
    points: HashMap<String, (Vec<f32>, Value)>,
    /// Vector width the collection was created with, if it exists.
    collection_size: Option<usize>,
    /// Width of the vectors the embeddings endpoint returns.
    embedding_dimensions: usize,
    /// When set, every request gets this status and body instead.
    failure: Option<(u16, String)>,
}

impl MockService {
    /// Start on an ephemeral port.
    pub fn start(embedding_dimensions: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state =
            Arc::new(Mutex::new(ServiceState { embedding_dimensions, ..Default::default() }));
        let shutdown = Arc::new(AtomicBool::new(false));

        {
            let requests = Arc::clone(&requests);
            let state = Arc::clone(&state);
            let shutdown = Arc::clone(&shutdown);
            thread::spawn(move || {
                for stream in listener.incoming() {
                    if shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                    match stream {
                        Ok(stream) => handle(stream, &requests, &state),
                        Err(_) => return,
                    }
                }
            });
        }

        Self { base_url: format!("http://127.0.0.1:{port}"), requests, state, shutdown }
    }

    /// Make every subsequent request fail, to test degraded behaviour.
    pub fn fail_with(&self, status: u16, body: &str) {
        self.state.lock().unwrap().failure = Some((status, body.to_string()));
    }

    /// Pre-create the collection with a given vector width.
    pub fn set_collection_size(&self, size: usize) {
        self.state.lock().unwrap().collection_size = Some(size);
    }

    pub fn point_count(&self) -> usize {
        self.state.lock().unwrap().points.len()
    }

    pub fn collection_size(&self) -> Option<usize> {
        self.state.lock().unwrap().collection_size
    }

    /// Paths of the requests received so far.
    pub fn paths(&self) -> Vec<String> {
        self.requests.lock().unwrap().iter().map(|r| r.path.clone()).collect()
    }

    pub fn requests_to(&self, needle: &str) -> Vec<Received> {
        self.requests.lock().unwrap().iter().filter(|r| r.path.contains(needle)).cloned().collect()
    }
}

impl Drop for MockService {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Unblock the accept loop.
        let _ = std::net::TcpStream::connect(self.base_url.trim_start_matches("http://"));
    }
}

fn handle(
    mut stream: TcpStream,
    requests: &Arc<Mutex<Vec<Received>>>,
    state: &Arc<Mutex<ServiceState>>,
) {
    let Some(request) = read_request(&mut stream) else { return };
    requests.lock().unwrap().push(request.clone());

    let (status, body) = {
        let mut state = state.lock().unwrap();
        if let Some((status, body)) = state.failure.clone() {
            (status, body)
        } else {
            route(&request, &mut state)
        }
    };

    let response = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        if status < 300 { "OK" } else { "Error" },
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn read_request(stream: &mut TcpStream) -> Option<Received> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut headers = HashMap::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.insert(name, value);
        }
    }

    let mut raw = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut raw).ok()?;
    }
    let body = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    Some(Received { method, path, headers, body })
}

fn route(request: &Received, state: &mut ServiceState) -> (u16, String) {
    let path = request.path.split('?').next().unwrap_or(&request.path).to_string();

    // OpenAI-compatible embeddings, as served by Ollama, TEI or vLLM.
    if path.ends_with("/embeddings") {
        let inputs = request.body["input"].as_array().cloned().unwrap_or_default();
        let data: Vec<Value> = inputs
            .iter()
            .enumerate()
            .map(|(index, text)| {
                json!({
                    "index": index,
                    "embedding": embed(text.as_str().unwrap_or_default(), state.embedding_dimensions),
                })
            })
            .collect();
        return (200, json!({"object": "list", "data": data}).to_string());
    }

    if path.ends_with("/points/search") {
        let query: Vec<f32> = request.body["vector"]
            .as_array()
            .map(|values| values.iter().filter_map(|v| v.as_f64()).map(|v| v as f32).collect())
            .unwrap_or_default();
        let limit = request.body["limit"].as_u64().unwrap_or(10) as usize;
        let kinds: Vec<String> = request.body["filter"]["must"]
            .as_array()
            .and_then(|clauses| {
                clauses.iter().find(|clause| clause["key"] == "kind").and_then(|clause| {
                    clause["match"]["any"].as_array().map(|values| {
                        values.iter().filter_map(|v| v.as_str()).map(str::to_string).collect()
                    })
                })
            })
            .unwrap_or_default();

        let mut scored: Vec<(String, f64, Value)> = state
            .points
            .iter()
            .filter(|(_, (_, payload))| payload["status"] != "archived")
            .filter(|(_, (_, payload))| {
                kinds.is_empty() || kinds.iter().any(|kind| payload["kind"] == kind.as_str())
            })
            .map(|(id, (vector, payload))| (id.clone(), cosine(&query, vector), payload.clone()))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(limit);

        let result: Vec<Value> = scored
            .into_iter()
            .map(|(id, score, payload)| json!({"id": id, "score": score, "payload": payload}))
            .collect();
        return (200, json!({"result": result, "status": "ok"}).to_string());
    }

    if path.ends_with("/points/delete") {
        if let Some(ids) = request.body["points"].as_array() {
            for id in ids.iter().filter_map(Value::as_str) {
                state.points.remove(id);
            }
        }
        return (200, json!({"result": {"status": "completed"}}).to_string());
    }

    if path.ends_with("/points") {
        if let Some(points) = request.body["points"].as_array() {
            for point in points {
                let id = point["id"].as_str().unwrap_or_default().to_string();
                let vector: Vec<f32> = point["vector"]
                    .as_array()
                    .map(|values| {
                        values.iter().filter_map(|v| v.as_f64()).map(|v| v as f32).collect()
                    })
                    .unwrap_or_default();
                state.points.insert(id, (vector, point["payload"].clone()));
            }
        }
        return (200, json!({"result": {"status": "completed"}}).to_string());
    }

    if path.contains("/collections/") {
        match request.method.as_str() {
            "GET" => match state.collection_size {
                Some(size) => (
                    200,
                    json!({"result": {
                        "points_count": state.points.len(),
                        "config": {"params": {"vectors": {"size": size, "distance": "Cosine"}}}
                    }})
                    .to_string(),
                ),
                None => (404, json!({"status": {"error": "Not found"}}).to_string()),
            },
            "PUT" => {
                let size = request.body["vectors"]["size"].as_u64().unwrap_or(0) as usize;
                state.collection_size = Some(size);
                (200, json!({"result": true}).to_string())
            }
            "DELETE" => {
                state.collection_size = None;
                state.points.clear();
                (200, json!({"result": true}).to_string())
            }
            _ => (405, json!({"error": "method not allowed"}).to_string()),
        }
    } else {
        (404, json!({"error": "unhandled path"}).to_string())
    }
}

/// Deterministic stand-in for a real embedding model: the same feature-hashing
/// scheme ContextD ships locally, so similar text produces similar vectors and
/// the ranking assertions in these tests mean something.
fn embed(text: &str, dimensions: usize) -> Vec<f32> {
    contextd::embeddings::local::LocalEmbedder::new(dimensions).embed_text(text)
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f64 = a.iter().zip(b).map(|(x, y)| (*x as f64) * (*y as f64)).sum();
    let na: f64 = a.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}
