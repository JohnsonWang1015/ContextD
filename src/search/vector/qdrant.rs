//! Qdrant backend, over the REST API.
//!
//! REST rather than the official gRPC client: it keeps ContextD's dependency
//! tree small (one HTTP client it already has), it works through proxies, and
//! the four operations needed here — ensure collection, upsert, search,
//! delete — are a handful of JSON documents.
//!
//! The collection is created on first use, sized from the first vector seen,
//! with cosine distance. If a collection already exists with a different
//! width — which is what switching from a 384-dimension local embedder to
//! bge-m3's 1024 looks like — the mismatch is reported with the command that
//! fixes it rather than producing silently meaningless results.

use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::VectorConfig;
use crate::core::model::{RecordKind, RecordRef, Status};
use crate::embeddings::provider::BoxFuture;
use crate::error::{Error, Result};
use crate::search::vector::{
    is_retrievable, VectorHealth, VectorIndex, VectorMatch, VectorPoint, VectorQuery,
};
use crate::storage::repository::ProjectScope;

/// Qdrant-backed vector index.
pub struct QdrantIndex {
    base_url: String,
    collection: String,
    api_key: Option<String>,
    batch_size: usize,
    client: reqwest::Client,
    /// Set once the collection is known to exist, so the check costs one
    /// request per process rather than one per upsert.
    collection_ready: AtomicBool,
}

impl QdrantIndex {
    pub fn new(config: &VectorConfig) -> Result<Self> {
        let base_url = config.url.trim().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            return Err(Error::VectorStore("qdrant".into(), "vector.url must not be empty".into()));
        }
        let collection = config.collection.trim().to_string();
        if collection.is_empty() {
            return Err(Error::VectorStore(
                "qdrant".into(),
                "vector.collection must not be empty".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs.max(1)))
            .build()
            .map_err(|err| Error::VectorStore("qdrant".into(), err.to_string()))?;

        Ok(Self {
            base_url,
            collection,
            api_key: std::env::var(&config.api_key_env).ok().filter(|k| !k.trim().is_empty()),
            batch_size: config.batch_size.max(1),
            client,
            collection_ready: AtomicBool::new(false),
        })
    }

    fn url(&self, suffix: &str) -> String {
        format!("{}/collections/{}{suffix}", self.base_url, self.collection)
    }

    fn request(&self, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
        let request = self.client.request(method, url);
        match &self.api_key {
            Some(key) => request.header("api-key", key),
            None => request,
        }
    }

    async fn send(&self, request: reqwest::RequestBuilder, what: &str) -> Result<Value> {
        let response = request.send().await.map_err(|err| {
            Error::VectorStore(
                "qdrant".into(),
                format!("{what} failed: {err} (is Qdrant reachable at {}?)", self.base_url),
            )
        })?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Error::VectorStore(
                "qdrant".into(),
                format!("{what} returned {status}: {}", body.trim()),
            ));
        }
        Ok(serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    /// Create the collection if it is missing, and check its width otherwise.
    async fn ensure_collection(&self, dimensions: usize) -> Result<()> {
        if self.collection_ready.load(Ordering::Relaxed) {
            return Ok(());
        }

        let existing =
            self.request(reqwest::Method::GET, self.url("")).send().await.map_err(|err| {
                Error::VectorStore(
                    "qdrant".into(),
                    format!("cannot reach Qdrant at {}: {err}", self.base_url),
                )
            })?;

        if existing.status().is_success() {
            let body: Value = existing.json().await.unwrap_or(Value::Null);
            if let Some(size) = collection_size(&body) {
                if size != dimensions {
                    return Err(Error::VectorStore(
                        "qdrant".into(),
                        format!(
                            "collection `{}` stores {size}-dimension vectors but the embedding \
                             provider produces {dimensions}. Point vector.collection at a new \
                             name, or drop the collection and run \
                             `contextd refresh --reindex-vectors`",
                            self.collection
                        ),
                    ));
                }
            }
            self.collection_ready.store(true, Ordering::Relaxed);
            return Ok(());
        }

        if existing.status() != reqwest::StatusCode::NOT_FOUND {
            let status = existing.status();
            let body = existing.text().await.unwrap_or_default();
            return Err(Error::VectorStore(
                "qdrant".into(),
                format!("inspecting collection returned {status}: {}", body.trim()),
            ));
        }

        self.send(
            self.request(reqwest::Method::PUT, self.url(""))
                .json(&json!({"vectors": {"size": dimensions, "distance": "Cosine"}})),
            "creating the collection",
        )
        .await?;
        tracing::info!(collection = %self.collection, dimensions, "created Qdrant collection");
        self.collection_ready.store(true, Ordering::Relaxed);
        Ok(())
    }
}

/// Pull the vector width out of a collection description, tolerating both the
/// unnamed-vector and named-vector layouts Qdrant can report.
fn collection_size(body: &Value) -> Option<usize> {
    let vectors = body.pointer("/result/config/params/vectors")?;
    if let Some(size) = vectors.get("size").and_then(Value::as_u64) {
        return Some(size as usize);
    }
    vectors
        .as_object()?
        .values()
        .find_map(|named| named.get("size").and_then(Value::as_u64))
        .map(|size| size as usize)
}

/// Build the JSON body for an upsert.
///
/// The payload carries `kind`, `project_id` and `status`, which is exactly
/// what [`filter_for`] needs to restrict a search.
pub(crate) fn upsert_body(points: &[VectorPoint]) -> Value {
    json!({
        "points": points
            .iter()
            .map(|point| json!({
                "id": point.record.id,
                "vector": point.vector,
                "payload": {
                    "kind": point.record.kind.as_str(),
                    "project_id": point.project_id,
                    "status": point.status.as_str(),
                },
            }))
            .collect::<Vec<_>>()
    })
}

/// Build the filter for a query.
///
/// Scope is expressed as a nested clause so that "this project or global"
/// stays a single OR inside the outer AND; flattening it would quietly turn
/// the kind restriction into an alternative rather than a requirement.
pub(crate) fn filter_for(scope: &ProjectScope, kinds: &[RecordKind]) -> Value {
    let mut must: Vec<Value> = Vec::new();

    if !kinds.is_empty() {
        must.push(json!({
            "key": "kind",
            "match": {"any": kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>()}
        }));
    }

    match scope {
        ProjectScope::Any => {}
        ProjectScope::GlobalOnly => must.push(json!({"is_empty": {"key": "project_id"}})),
        ProjectScope::Project(id) => {
            must.push(json!({"key": "project_id", "match": {"value": id}}))
        }
        ProjectScope::ProjectWithGlobal(id) => must.push(json!({
            "should": [
                {"key": "project_id", "match": {"value": id}},
                {"is_empty": {"key": "project_id"}},
            ]
        })),
    }

    json!({
        "must": must,
        // Archived memories stay in the store but never come back from search.
        "must_not": [{"key": "status", "match": {"value": Status::Archived.as_str()}}],
    })
}

/// Read search results back into matches.
pub(crate) fn parse_matches(body: &Value) -> Vec<VectorMatch> {
    body.get("result")
        .and_then(Value::as_array)
        .map(|results| {
            results
                .iter()
                .filter_map(|hit| {
                    let id = hit.get("id")?.as_str()?.to_string();
                    let payload = hit.get("payload")?;
                    let kind: RecordKind = payload.get("kind")?.as_str()?.parse().ok()?;
                    let status: Status = payload
                        .get("status")
                        .and_then(Value::as_str)?
                        .parse()
                        .unwrap_or(Status::Active);
                    if !is_retrievable(status) {
                        return None;
                    }
                    Some(VectorMatch {
                        record: RecordRef::new(kind, id),
                        project_id: payload
                            .get("project_id")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        // Qdrant returns cosine in -1..=1; negatives are
                        // actively dissimilar and score nothing.
                        score: hit
                            .get("score")
                            .and_then(Value::as_f64)
                            .unwrap_or(0.0)
                            .clamp(0.0, 1.0),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

impl VectorIndex for QdrantIndex {
    fn backend(&self) -> &str {
        "qdrant"
    }

    fn is_external(&self) -> bool {
        true
    }

    fn upsert<'a>(&'a self, points: &'a [VectorPoint]) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(first) = points.iter().find(|point| !point.vector.is_empty()) else {
                return Ok(());
            };
            self.ensure_collection(first.vector.len()).await?;

            for chunk in points.chunks(self.batch_size) {
                let usable: Vec<VectorPoint> =
                    chunk.iter().filter(|p| !p.vector.is_empty()).cloned().collect();
                if usable.is_empty() {
                    continue;
                }
                self.send(
                    self.request(reqwest::Method::PUT, self.url("/points?wait=true"))
                        .json(&upsert_body(&usable)),
                    "upserting points",
                )
                .await?;
            }
            Ok(())
        })
    }

    fn delete<'a>(&'a self, records: &'a [RecordRef]) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if records.is_empty() {
                return Ok(());
            }
            let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
            self.send(
                self.request(reqwest::Method::POST, self.url("/points/delete?wait=true"))
                    .json(&json!({"points": ids})),
                "deleting points",
            )
            .await?;
            Ok(())
        })
    }

    fn search<'a>(&'a self, query: &'a VectorQuery) -> BoxFuture<'a, Result<Vec<VectorMatch>>> {
        Box::pin(async move {
            if query.vector.is_empty() {
                return Ok(Vec::new());
            }
            let body = self
                .send(
                    self.request(reqwest::Method::POST, self.url("/points/search")).json(&json!({
                        "vector": query.vector,
                        "limit": query.limit.max(1),
                        "with_payload": true,
                        "filter": filter_for(&query.scope, &query.kinds),
                    })),
                    "searching",
                )
                .await?;
            Ok(parse_matches(&body))
        })
    }

    fn clear(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            // Deleting the collection is cheaper than deleting every point,
            // and the next upsert recreates it at the current width — which is
            // exactly what is wanted after switching embedding model.
            let response = self
                .request(reqwest::Method::DELETE, self.url(""))
                .send()
                .await
                .map_err(|err| Error::VectorStore("qdrant".into(), err.to_string()))?;
            if !response.status().is_success()
                && response.status() != reqwest::StatusCode::NOT_FOUND
            {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(Error::VectorStore(
                    "qdrant".into(),
                    format!("dropping the collection returned {status}: {}", body.trim()),
                ));
            }
            self.collection_ready.store(false, Ordering::Relaxed);
            Ok(())
        })
    }

    fn health(&self) -> BoxFuture<'_, Result<VectorHealth>> {
        Box::pin(async move {
            let response = self.request(reqwest::Method::GET, self.url("")).send().await;
            match response {
                Ok(response) if response.status().is_success() => {
                    let body: Value = response.json().await.unwrap_or(Value::Null);
                    Ok(VectorHealth {
                        backend: "qdrant".into(),
                        reachable: true,
                        points: body
                            .pointer("/result/points_count")
                            .and_then(Value::as_u64)
                            .map(|n| n as usize),
                        detail: Some(format!("{}/{}", self.base_url, self.collection)),
                    })
                }
                Ok(response) if response.status() == reqwest::StatusCode::NOT_FOUND => {
                    Ok(VectorHealth {
                        backend: "qdrant".into(),
                        reachable: true,
                        points: Some(0),
                        detail: Some(format!(
                            "collection `{}` not created yet (it appears on first index)",
                            self.collection
                        )),
                    })
                }
                Ok(response) => Ok(VectorHealth {
                    backend: "qdrant".into(),
                    reachable: false,
                    points: None,
                    detail: Some(format!("{} returned {}", self.base_url, response.status())),
                }),
                Err(err) => Ok(VectorHealth {
                    backend: "qdrant".into(),
                    reachable: false,
                    points: None,
                    detail: Some(format!("{}: {err}", self.base_url)),
                }),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> QdrantIndex {
        QdrantIndex::new(&VectorConfig {
            backend: "qdrant".into(),
            url: "http://localhost:6333/".into(),
            collection: "contextd".into(),
            ..VectorConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn urls_are_built_from_the_base() {
        let index = index();
        assert_eq!(index.url(""), "http://localhost:6333/collections/contextd");
        assert_eq!(
            index.url("/points/search"),
            "http://localhost:6333/collections/contextd/points/search"
        );
    }

    #[test]
    fn empty_configuration_is_rejected() {
        let config = VectorConfig { url: "  ".into(), ..VectorConfig::default() };
        assert!(QdrantIndex::new(&config).is_err());
        let config = VectorConfig { collection: "".into(), ..VectorConfig::default() };
        assert!(QdrantIndex::new(&config).is_err());
    }

    #[test]
    fn upsert_body_carries_payload_for_filtering() {
        let body = upsert_body(&[VectorPoint {
            record: RecordRef::memory("11111111-2222-3333-4444-555555555555"),
            project_id: Some("p1".into()),
            status: Status::Superseded,
            vector: vec![0.1, 0.2],
        }]);
        let point = &body["points"][0];
        assert_eq!(point["id"], "11111111-2222-3333-4444-555555555555");
        assert_eq!(point["payload"]["kind"], "memory");
        assert_eq!(point["payload"]["project_id"], "p1");
        assert_eq!(point["payload"]["status"], "superseded");
        // f32 → JSON widens to f64, so compare with a tolerance.
        assert!((point["vector"][1].as_f64().unwrap() - 0.2).abs() < 1e-6);
    }

    #[test]
    fn filters_express_scope_and_kinds() {
        let any = filter_for(&ProjectScope::Any, &[]);
        assert_eq!(any["must"].as_array().unwrap().len(), 0);
        assert_eq!(any["must_not"][0]["match"]["value"], "archived");

        let scoped = filter_for(
            &ProjectScope::ProjectWithGlobal("p1".into()),
            &[RecordKind::Memory, RecordKind::Decision],
        );
        let must = scoped["must"].as_array().unwrap();
        assert_eq!(must[0]["key"], "kind");
        assert_eq!(must[0]["match"]["any"][1], "decision");
        // The project-or-global alternative stays nested inside the AND.
        let should = must[1]["should"].as_array().unwrap();
        assert_eq!(should[0]["match"]["value"], "p1");
        assert_eq!(should[1]["is_empty"]["key"], "project_id");

        let global = filter_for(&ProjectScope::GlobalOnly, &[]);
        assert_eq!(global["must"][0]["is_empty"]["key"], "project_id");

        let project = filter_for(&ProjectScope::Project("p1".into()), &[]);
        assert_eq!(project["must"][0]["match"]["value"], "p1");
    }

    #[test]
    fn results_are_parsed_and_clamped() {
        let body = json!({"result": [
            {"id": "a", "score": 0.87, "payload": {"kind": "memory", "project_id": "p1", "status": "active"}},
            {"id": "b", "score": -0.4, "payload": {"kind": "decision", "project_id": null, "status": "superseded"}},
            {"id": "c", "score": 0.9, "payload": {"kind": "memory", "status": "archived"}},
            {"id": "d", "score": 0.9, "payload": {"kind": "nonsense", "status": "active"}},
        ]});
        let matches = parse_matches(&body);
        assert_eq!(matches.len(), 2, "archived and unparseable rows are dropped");
        assert_eq!(matches[0].record.id, "a");
        assert_eq!(matches[0].project_id.as_deref(), Some("p1"));
        assert_eq!(matches[1].score, 0.0, "negative cosine scores nothing");
        assert!(matches[1].project_id.is_none());
    }

    #[test]
    fn malformed_responses_do_not_panic() {
        assert!(parse_matches(&json!({})).is_empty());
        assert!(parse_matches(&json!({"result": "nope"})).is_empty());
        assert!(parse_matches(&Value::Null).is_empty());
    }

    #[test]
    fn collection_size_is_read_from_either_layout() {
        let unnamed = json!({"result": {"config": {"params": {"vectors": {"size": 1024, "distance": "Cosine"}}}}});
        assert_eq!(collection_size(&unnamed), Some(1024));

        let named = json!({"result": {"config": {"params": {"vectors": {"text": {"size": 384}}}}}});
        assert_eq!(collection_size(&named), Some(384));

        assert_eq!(collection_size(&json!({})), None);
    }
}
