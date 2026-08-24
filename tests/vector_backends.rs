//! ContextD against an OpenAI-compatible embedding endpoint and a Qdrant
//! collection, over real HTTP.
//!
//! The services are stand-ins (see `tests/support`), but everything on
//! ContextD's side is the real code path: config, provider selection, URL and
//! header construction, request bodies, collection creation, filtering and
//! response parsing. This is the setup a bge-m3 + Qdrant user runs.

mod common;
mod support;

use common::Sandbox;
use support::MockService;

/// bge-m3 produces 1024-dimension vectors; the mock does the same so the
/// collection-width logic is exercised as it would be in practice.
const BGE_M3_DIMENSIONS: usize = 1024;

/// Point a sandbox at the mock embedding endpoint and Qdrant collection.
fn configure(sandbox: &Sandbox, service: &MockService, backend: &str) {
    sandbox.run(&["config", "set", "embeddings.provider", "openai"]);
    sandbox.run(&["config", "set", "embeddings.model", "bge-m3"]);
    sandbox.run(&["config", "set", "embeddings.api_base", &format!("{}/v1", service.base_url)]);
    sandbox.run(&["config", "set", "embeddings.dimensions", &BGE_M3_DIMENSIONS.to_string()]);
    sandbox.run(&["config", "set", "vector.backend", backend]);
    sandbox.run(&["config", "set", "vector.url", &service.base_url]);
    sandbox.run(&["config", "set", "vector.collection", "contextd_test"]);
}

#[test]
fn recall_runs_through_bge_m3_and_qdrant() {
    let service = MockService::start(BGE_M3_DIMENSIONS);
    let sandbox = Sandbox::new();
    sandbox.run(&["init"]);
    configure(&sandbox, &service, "qdrant");
    sandbox.run(&["attach", "--name", "FerroGrid"]);

    // Adding a memory embeds it remotely and publishes the point to Qdrant.
    sandbox.run(&[
        "add",
        "-c",
        "architecture",
        "After evaluating Redis and PostgreSQL LISTEN/NOTIFY, the scheduler transport was \
         migrated to NATS",
    ]);
    sandbox.run(&["add", "-c", "convention", "Format with rustfmt before committing"]);

    // The collection was created at the model's width, not the default.
    assert_eq!(service.collection_size(), Some(BGE_M3_DIMENSIONS));
    assert_eq!(service.point_count(), 2, "both memories reached Qdrant");

    let embedding_calls = service.requests_to("/embeddings");
    assert!(!embedding_calls.is_empty());
    assert_eq!(embedding_calls[0].body["model"], "bge-m3");

    // Recall: the question shares little wording with the memory, and the
    // ranking comes back through Qdrant's search endpoint.
    let recall = sandbox.run(&["recall", "which message transport does the scheduler use?"]);
    assert!(recall.contains("NATS"), "recall output: {recall}");
    assert!(!service.requests_to("/points/search").is_empty(), "Qdrant search must be used");

    // The search request carried the filters ContextD relies on.
    let search = service.requests_to("/points/search").pop().unwrap();
    assert!(search.body["with_payload"].as_bool().unwrap());
    assert_eq!(search.body["filter"]["must_not"][0]["match"]["value"], "archived");
    assert_eq!(search.body["vector"].as_array().unwrap().len(), BGE_M3_DIMENSIONS);

    // Status reports the external store, and it is reachable.
    let status = sandbox.run_json(&["status"]);
    assert_eq!(status["vector"]["backend"], "qdrant");
    assert_eq!(status["vector"]["reachable"], true);
    assert_eq!(status["vector"]["points"], 2);

    // `config --check` exercises provider and store together.
    let check = sandbox.run_json(&["config", "--check"]);
    assert_eq!(check["embeddings"]["ok"], true);
    assert_eq!(check["embeddings"]["detail"], format!("{BGE_M3_DIMENSIONS} dimensions"));
    assert_eq!(check["vector_store"]["ok"], true);
}

#[test]
fn deleting_a_memory_removes_its_point() {
    let service = MockService::start(64);
    let sandbox = Sandbox::new();
    sandbox.run(&["init"]);
    configure(&sandbox, &service, "qdrant");
    sandbox.run(&["config", "set", "embeddings.dimensions", "64"]);
    sandbox.run(&["attach", "--name", "FerroGrid"]);

    let id = sandbox.run_json(&["add", "-c", "task", "Wire up worker reconnect"])["memory"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(service.point_count(), 1);

    sandbox.run(&["delete", &id[..8]]);
    assert_eq!(service.point_count(), 0, "the point must go with the memory");
}

#[test]
fn switching_backend_republishes_existing_vectors() {
    let service = MockService::start(64);
    let sandbox = Sandbox::new();
    sandbox.run(&["init"]);
    // Start on the built-in index, so vectors exist only in SQLite.
    configure(&sandbox, &service, "sqlite");
    sandbox.run(&["config", "set", "embeddings.dimensions", "64"]);
    sandbox.run(&["attach", "--name", "FerroGrid"]);
    sandbox.run(&["add", "-c", "architecture", "Scheduler transport is NATS"]);
    sandbox.run(&["add", "-c", "architecture", "Workers renew GPU leases every 30 seconds"]);
    assert_eq!(service.point_count(), 0, "nothing should reach Qdrant yet");

    // Switch to Qdrant and move what SQLite already holds — no re-embedding.
    sandbox.run(&["config", "set", "vector.backend", "qdrant"]);
    let embeddings_before = service.requests_to("/embeddings").len();
    let report = sandbox.run_json(&["refresh", "--reindex-vectors"]);
    assert!(
        report["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note.as_str().unwrap_or_default().contains("re-published")),
        "{report}"
    );
    assert_eq!(service.point_count(), 2);
    assert_eq!(
        service.requests_to("/embeddings").len(),
        embeddings_before,
        "re-indexing must not pay to embed again"
    );

    assert!(sandbox.run(&["recall", "how are GPU leases handled?"]).contains("leases"));
}

#[test]
fn a_collection_of_the_wrong_width_is_reported_not_corrupted() {
    let service = MockService::start(64);
    // A collection left over from a 384-dimension model.
    service.set_collection_size(384);

    let sandbox = Sandbox::new();
    sandbox.run(&["init"]);
    configure(&sandbox, &service, "qdrant");
    sandbox.run(&["config", "set", "embeddings.dimensions", "64"]);
    sandbox.run(&["attach", "--name", "FerroGrid"]);

    // The memory is still stored — indexing is best effort — but the mismatch
    // must surface rather than silently producing meaningless neighbours.
    sandbox.run(&["add", "-c", "architecture", "Scheduler transport is NATS"]);
    assert_eq!(sandbox.run_json(&["memories"]).as_array().unwrap().len(), 1);

    let stderr = sandbox.run_failing(&["refresh", "--force-embeddings"]);
    assert!(stderr.contains("384-dimension"), "stderr: {stderr}");
    assert!(stderr.contains("reindex-vectors"), "the fix must be in the message: {stderr}");
    assert_eq!(service.point_count(), 0);
}

#[test]
fn an_unreachable_vector_store_degrades_to_keyword_search() {
    let service = MockService::start(64);
    let sandbox = Sandbox::new();
    sandbox.run(&["init"]);
    configure(&sandbox, &service, "qdrant");
    sandbox.run(&["config", "set", "embeddings.dimensions", "64"]);
    sandbox.run(&["attach", "--name", "FerroGrid"]);
    sandbox.run(&["add", "-c", "architecture", "Scheduler transport is NATS"]);

    // Qdrant falls over.
    service.fail_with(503, "{\"status\":{\"error\":\"service unavailable\"}}");

    // Status says so plainly.
    let status = sandbox.run_json(&["status"]);
    assert_eq!(status["vector"]["reachable"], false);

    // Keyword search still answers, because FTS5 is local and always there.
    let search = sandbox.run(&["search", "transport"]);
    assert!(search.contains("NATS"), "keyword search must survive: {search}");
}
