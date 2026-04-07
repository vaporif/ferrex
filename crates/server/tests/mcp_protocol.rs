use std::process::Stdio;

use ferrex_store::QdrantSidecar;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::{Duration, timeout};

struct McpTestHarness {
    child: Child,
    stdin: tokio::process::ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    _sidecar: QdrantSidecar,
    _temp_dir: tempfile::TempDir,
    next_id: u64,
}

impl McpTestHarness {
    async fn new_raw() -> Self {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let base_dir = temp_dir.path().to_path_buf();
        let port = portpicker::pick_unused_port().expect("no free port");

        let sidecar = QdrantSidecar::start("qdrant", port, Some(base_dir))
            .await
            .expect("sidecar start");

        let db_path = temp_dir.path().join("ferrex.db");
        let config_path = temp_dir.path().join("ferrex.toml");

        let bin = std::env::var("NEXTEST_BIN_EXE_ferrex")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_ferrex").to_string());
        let mut child = Command::new(bin)
            .arg("--qdrant-url")
            .arg(format!("http://localhost:{port}"))
            .arg("--db-path")
            .arg(&db_path)
            .arg("--config-path")
            .arg(&config_path)
            .arg("--model-tier")
            .arg("small")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ferrex");

        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let reader = BufReader::new(stdout);

        Self {
            child,
            stdin,
            reader,
            _sidecar: sidecar,
            _temp_dir: temp_dir,
            next_id: 1,
        }
    }

    async fn new() -> Self {
        let mut harness = Self::new_raw().await;

        let _init_resp = harness
            .send_request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "0.1.0" }
                }),
            )
            .await;

        harness
            .send_notification("notifications/initialized", json!({}))
            .await;

        harness
    }

    async fn initialize(&mut self) -> Value {
        self.send_request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0.1.0" }
            }),
        )
        .await
    }

    async fn send_request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();

        let mut buf = String::new();
        self.reader.read_line(&mut buf).await.unwrap();
        serde_json::from_str(&buf).unwrap()
    }

    async fn send_notification(&mut self, method: &str, params: Value) {
        let req = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        self.send_request(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments
            }),
        )
        .await
    }

    async fn shutdown(&mut self) {
        let _ = self.child.kill().await;
    }
}

impl Drop for McpTestHarness {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

#[tokio::test]
async fn test_tools_list_returns_all_tools() {
    let mut h = McpTestHarness::new().await;
    let resp = h.send_request("tools/list", json!({})).await;

    let tools = resp["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    assert!(names.contains(&"store"), "missing store tool");
    assert!(names.contains(&"recall"), "missing recall tool");
    assert!(names.contains(&"forget"), "missing forget tool");
    assert!(names.contains(&"reflect"), "missing reflect tool");
    assert!(names.contains(&"stats"), "missing stats tool");
    assert_eq!(names.len(), 5, "unexpected tool count: {names:?}");

    let store_tool = tools.iter().find(|t| t["name"] == "store").unwrap();
    let props = &store_tool["inputSchema"]["properties"];
    assert!(props["content"].is_object(), "store missing content param");
    assert!(
        props["memory_type"].is_object(),
        "store missing memory_type param"
    );
    assert!(props["subject"].is_object(), "store missing subject param");

    h.shutdown().await;
}

#[tokio::test]
async fn test_store_and_recall_roundtrip() {
    let mut h = McpTestHarness::new().await;

    let store_resp = h
        .call_tool(
            "store",
            json!({
                "content": "Deployed API v2.1 to production",
                "memory_type": "episodic"
            }),
        )
        .await;

    let content = &store_resp["result"]["content"][0]["text"];
    let stored: Value = serde_json::from_str(content.as_str().unwrap()).unwrap();
    assert_eq!(stored["stored"], true);
    assert!(stored["id"].is_string());
    assert_eq!(stored["type"], "episodic");

    let memory_id = stored["id"].as_str().unwrap().to_string();

    let recall_resp = h
        .call_tool(
            "recall",
            json!({
                "query": "API deployment"
            }),
        )
        .await;
    let recall_content = &recall_resp["result"]["content"][0]["text"];
    let recalled: Value = serde_json::from_str(recall_content.as_str().unwrap()).unwrap();
    let results = recalled.as_array().expect("recall returns array");
    assert!(!results.is_empty(), "recall should find the stored memory");
    assert!(results.iter().any(|r| r["id"] == memory_id));

    let forget_resp = h
        .call_tool(
            "forget",
            json!({
                "ids": [memory_id]
            }),
        )
        .await;
    let forget_content = &forget_resp["result"]["content"][0]["text"];
    let forgotten: Value = serde_json::from_str(forget_content.as_str().unwrap()).unwrap();
    assert!(
        forgotten["deleted"]
            .as_array()
            .unwrap()
            .contains(&Value::String(memory_id.clone()))
    );

    let recall_resp2 = h
        .call_tool(
            "recall",
            json!({
                "query": "API deployment"
            }),
        )
        .await;
    let recall_content2 = &recall_resp2["result"]["content"][0]["text"];
    let recalled2: Value = serde_json::from_str(recall_content2.as_str().unwrap()).unwrap();
    let results2 = recalled2.as_array().expect("recall returns array");
    assert!(
        !results2.iter().any(|r| r["id"] == memory_id),
        "forgotten memory should not appear"
    );

    h.shutdown().await;
}

#[tokio::test]
async fn test_store_semantic_and_recall_by_type() {
    let mut h = McpTestHarness::new().await;

    let store_resp = h
        .call_tool(
            "store",
            json!({
                "subject": "ferrex",
                "predicate": "written-in",
                "object": "Rust",
                "memory_type": "semantic"
            }),
        )
        .await;
    let content = &store_resp["result"]["content"][0]["text"];
    let stored: Value = serde_json::from_str(content.as_str().unwrap()).unwrap();
    assert_eq!(stored["type"], "semantic");

    let recall_resp = h
        .call_tool(
            "recall",
            json!({
                "query": "what language is ferrex written in",
                "types": ["semantic"]
            }),
        )
        .await;
    let recall_content = &recall_resp["result"]["content"][0]["text"];
    let recalled: Value = serde_json::from_str(recall_content.as_str().unwrap()).unwrap();
    assert!(!recalled.as_array().unwrap().is_empty());

    h.shutdown().await;
}

#[tokio::test]
async fn test_reflect_returns_structured_response() {
    let mut h = McpTestHarness::new().await;

    let resp = h
        .call_tool(
            "reflect",
            json!({
                "namespace": "default"
            }),
        )
        .await;
    let content = &resp["result"]["content"][0]["text"];
    let reflected: Value = serde_json::from_str(content.as_str().unwrap()).unwrap();

    assert!(
        reflected["stale"].is_array(),
        "reflect should have stale array"
    );
    assert!(
        reflected["contradictions"].is_array(),
        "reflect should have contradictions array"
    );
    assert!(
        reflected["summary"].is_object(),
        "reflect should have summary object"
    );

    h.shutdown().await;
}

#[tokio::test]
async fn test_stats_brief_and_detailed() {
    let mut h = McpTestHarness::new().await;

    let brief = h
        .call_tool(
            "stats",
            json!({
                "namespace": "default"
            }),
        )
        .await;
    let brief_content = &brief["result"]["content"][0]["text"];
    let brief_data: Value = serde_json::from_str(brief_content.as_str().unwrap()).unwrap();
    assert!(brief_data["total_memories"].is_number());
    assert!(brief_data["needs_attention"].is_object());

    let detailed = h
        .call_tool(
            "stats",
            json!({
                "namespace": "default",
                "detailed": true
            }),
        )
        .await;
    let detailed_content = &detailed["result"]["content"][0]["text"];
    let detailed_data: Value = serde_json::from_str(detailed_content.as_str().unwrap()).unwrap();
    assert!(
        detailed_data["details"].is_object(),
        "detailed mode should have details"
    );
    assert!(detailed_data["details"]["storage_size_bytes"].is_number());

    h.shutdown().await;
}

#[tokio::test]
async fn test_store_empty_content_error() {
    let mut h = McpTestHarness::new().await;

    let resp = h
        .call_tool(
            "store",
            json!({
                "content": "",
                "memory_type": "episodic"
            }),
        )
        .await;

    let is_error = resp["result"]["isError"] == true
        || resp["error"].is_object()
        || resp["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|t| t.contains("content"));
    assert!(is_error, "empty content should produce an error: {resp}");

    h.shutdown().await;
}

#[tokio::test]
async fn test_forget_nonexistent_id() {
    let mut h = McpTestHarness::new().await;

    let resp = h
        .call_tool(
            "forget",
            json!({
                "ids": ["00000000-0000-0000-0000-000000000000"]
            }),
        )
        .await;
    let content = &resp["result"]["content"][0]["text"];
    let data: Value = serde_json::from_str(content.as_str().unwrap()).unwrap();
    assert!(
        !data["not_found"].as_array().unwrap().is_empty(),
        "should report not_found"
    );

    h.shutdown().await;
}

#[tokio::test]
async fn test_initialize_response_structure() {
    let mut h = McpTestHarness::new_raw().await;
    let resp = h.initialize().await;

    assert_eq!(resp["jsonrpc"], "2.0");

    let result = &resp["result"];
    assert!(
        result["protocolVersion"].is_string(),
        "missing protocolVersion: {result}"
    );
    assert!(
        result["serverInfo"].is_object(),
        "missing serverInfo: {result}"
    );
    assert!(
        result["serverInfo"]["name"].is_string(),
        "missing serverInfo.name"
    );
    assert!(
        result["capabilities"].is_object(),
        "missing capabilities: {result}"
    );

    h.shutdown().await;
}

#[tokio::test]
async fn test_tool_schemas_have_required_fields_and_enums() {
    let mut h = McpTestHarness::new().await;
    let resp = h.send_request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");

    let find_tool = |name: &str| -> Value {
        tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("tool {name} not found"))
            .clone()
    };

    // store: should have inputSchema with properties
    let store = find_tool("store");
    let store_schema = &store["inputSchema"];
    assert_eq!(store_schema["type"], "object");
    let store_props = &store_schema["properties"];
    assert!(store_props["content"].is_object());
    assert!(store_props["memory_type"].is_object());
    assert!(store_props["subject"].is_object());
    assert!(store_props["predicate"].is_object());
    assert!(store_props["object"].is_object());
    assert!(store_props["confidence"].is_object());
    assert!(store_props["entities"].is_object());
    assert!(store_props["namespace"].is_object());
    assert!(store_props["supersedes"].is_object());

    // recall: query should be required
    let recall = find_tool("recall");
    let recall_schema = &recall["inputSchema"];
    let recall_required = recall_schema["required"]
        .as_array()
        .expect("recall required array");
    assert!(
        recall_required.contains(&json!("query")),
        "recall should require 'query': {recall_required:?}"
    );
    let recall_props = &recall_schema["properties"];
    assert!(recall_props["time_range"].is_object());
    assert!(recall_props["explain"].is_object());
    assert!(recall_props["include_stale"].is_object());
    assert!(recall_props["include_invalidated"].is_object());
    assert!(recall_props["validate_ids"].is_object());

    // forget: ids should be required
    let forget = find_tool("forget");
    let forget_required = forget["inputSchema"]["required"]
        .as_array()
        .expect("forget required array");
    assert!(
        forget_required.contains(&json!("ids")),
        "forget should require 'ids': {forget_required:?}"
    );

    // reflect: namespace should be required
    let reflect = find_tool("reflect");
    let reflect_required = reflect["inputSchema"]["required"]
        .as_array()
        .expect("reflect required array");
    assert!(
        reflect_required.contains(&json!("namespace")),
        "reflect should require 'namespace': {reflect_required:?}"
    );

    // stats: namespace should be required
    let stats = find_tool("stats");
    let stats_required = stats["inputSchema"]["required"]
        .as_array()
        .expect("stats required array");
    assert!(
        stats_required.contains(&json!("namespace")),
        "stats should require 'namespace': {stats_required:?}"
    );

    h.shutdown().await;
}

#[tokio::test]
async fn test_invalid_memory_type_returns_error() {
    let mut h = McpTestHarness::new().await;

    let resp = h
        .call_tool(
            "store",
            json!({
                "content": "test",
                "memory_type": "invalid_type"
            }),
        )
        .await;

    let is_error = resp["result"]["isError"] == true || resp["error"].is_object();
    assert!(is_error, "invalid memory_type should error: {resp}");

    h.shutdown().await;
}

#[tokio::test]
async fn test_invalid_time_range_returns_error() {
    let mut h = McpTestHarness::new().await;

    // Bad datetime format
    let resp = h
        .call_tool(
            "recall",
            json!({
                "query": "anything",
                "time_range": { "start": "not-a-date" }
            }),
        )
        .await;

    let is_error = resp["result"]["isError"] == true || resp["error"].is_object();
    assert!(is_error, "invalid time_range datetime should error: {resp}");

    // start > end
    let resp2 = h
        .call_tool(
            "recall",
            json!({
                "query": "anything",
                "time_range": {
                    "start": "2025-12-31T00:00:00Z",
                    "end": "2025-01-01T00:00:00Z"
                }
            }),
        )
        .await;

    let is_error2 = resp2["result"]["isError"] == true || resp2["error"].is_object();
    assert!(
        is_error2,
        "time_range with start > end should error: {resp2}"
    );

    h.shutdown().await;
}

#[tokio::test]
async fn test_store_missing_content_and_triple_returns_error() {
    let mut h = McpTestHarness::new().await;

    // No content and no triple fields
    let resp = h
        .call_tool(
            "store",
            json!({
                "memory_type": "episodic"
            }),
        )
        .await;

    let is_error = resp["result"]["isError"] == true || resp["error"].is_object();
    assert!(
        is_error,
        "store with no content/triple should error: {resp}"
    );

    h.shutdown().await;
}

#[tokio::test]
async fn test_recall_with_time_range_filter() {
    let mut h = McpTestHarness::new().await;

    let store_resp = h
        .call_tool(
            "store",
            json!({
                "content": "Time-scoped memory for testing range filter",
                "memory_type": "episodic"
            }),
        )
        .await;
    let stored: Value =
        serde_json::from_str(store_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(stored["stored"], true);

    // Time range that includes now — should find the memory
    let recall_resp = h
        .call_tool(
            "recall",
            json!({
                "query": "time-scoped memory range filter",
                "time_range": {
                    "start": "2020-01-01T00:00:00Z",
                    "end": "2099-12-31T23:59:59Z"
                }
            }),
        )
        .await;
    let results: Value = serde_json::from_str(
        recall_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert!(
        !results.as_array().unwrap().is_empty(),
        "should find memory within time range"
    );

    // Time range in the past — should not find it
    let recall_resp2 = h
        .call_tool(
            "recall",
            json!({
                "query": "time-scoped memory range filter",
                "time_range": {
                    "start": "2020-01-01T00:00:00Z",
                    "end": "2020-12-31T23:59:59Z"
                }
            }),
        )
        .await;
    let results2: Value = serde_json::from_str(
        recall_resp2["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert!(
        results2.as_array().unwrap().is_empty(),
        "should not find memory outside time range"
    );

    h.shutdown().await;
}

#[tokio::test]
async fn test_recall_with_explain_flag() {
    let mut h = McpTestHarness::new().await;

    h.call_tool(
        "store",
        json!({
            "content": "Explain flag test memory with unique content xyz123",
            "memory_type": "episodic"
        }),
    )
    .await;

    let recall_resp = h
        .call_tool(
            "recall",
            json!({
                "query": "explain flag test xyz123",
                "explain": true
            }),
        )
        .await;
    let results: Value = serde_json::from_str(
        recall_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let results_arr = results.as_array().unwrap();
    assert!(!results_arr.is_empty(), "should find the stored memory");

    let first = &results_arr[0];
    assert!(
        first["scoring"].is_object(),
        "explain=true should include scoring breakdown: {first}"
    );

    // Without explain, scoring should be absent
    let recall_resp2 = h
        .call_tool(
            "recall",
            json!({
                "query": "explain flag test xyz123",
                "explain": false
            }),
        )
        .await;
    let results2: Value = serde_json::from_str(
        recall_resp2["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let first2 = &results2.as_array().unwrap()[0];
    assert!(
        first2["scoring"].is_null(),
        "explain=false should not include scoring: {first2}"
    );

    h.shutdown().await;
}

#[tokio::test]
async fn test_recall_validate_ids() {
    let mut h = McpTestHarness::new().await;

    let store_resp = h
        .call_tool(
            "store",
            json!({
                "content": "Validate IDs test memory unique content abc789",
                "memory_type": "episodic"
            }),
        )
        .await;
    let stored: Value =
        serde_json::from_str(store_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let memory_id = stored["id"].as_str().unwrap().to_string();

    // Recall with validate_ids — should not error
    let recall_resp = h
        .call_tool(
            "recall",
            json!({
                "query": "validate IDs test abc789",
                "validate_ids": [memory_id]
            }),
        )
        .await;

    assert!(
        recall_resp["error"].is_null(),
        "validate_ids should not cause error: {recall_resp}"
    );
    let results: Value = serde_json::from_str(
        recall_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert!(results.is_array());

    h.shutdown().await;
}

#[tokio::test]
async fn test_recall_include_stale_and_invalidated() {
    let mut h = McpTestHarness::new().await;

    // Store a semantic fact, then supersede it
    let store_resp = h
        .call_tool(
            "store",
            json!({
                "subject": "stale-test-proj",
                "predicate": "uses",
                "object": "old-framework-v1",
                "memory_type": "semantic"
            }),
        )
        .await;
    let stored: Value =
        serde_json::from_str(store_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let old_id = stored["id"].as_str().unwrap().to_string();

    // Supersede the old fact
    let store_resp2 = h
        .call_tool(
            "store",
            json!({
                "subject": "stale-test-proj",
                "predicate": "uses",
                "object": "new-framework-v2",
                "memory_type": "semantic",
                "supersedes": old_id
            }),
        )
        .await;
    let stored2: Value = serde_json::from_str(
        store_resp2["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(stored2["stored"], true);

    // Without include_invalidated, old memory should not appear
    let recall_resp = h
        .call_tool(
            "recall",
            json!({
                "query": "stale-test-proj framework",
                "include_invalidated": false
            }),
        )
        .await;
    let results: Value = serde_json::from_str(
        recall_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert!(
        !results
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == old_id),
        "invalidated memory should be excluded when include_invalidated=false"
    );

    // With include_invalidated=true, old memory should appear
    let recall_resp2 = h
        .call_tool(
            "recall",
            json!({
                "query": "stale-test-proj framework",
                "include_invalidated": true
            }),
        )
        .await;
    let results2: Value = serde_json::from_str(
        recall_resp2["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert!(
        results2
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == old_id),
        "invalidated memory should appear when include_invalidated=true"
    );

    h.shutdown().await;
}

#[tokio::test]
async fn test_supersedes_invalidates_old_memory() {
    let mut h = McpTestHarness::new().await;

    let store_resp = h
        .call_tool(
            "store",
            json!({
                "subject": "supersede-test-app",
                "predicate": "deployed-version",
                "object": "v1.0.0",
                "memory_type": "semantic"
            }),
        )
        .await;
    let stored: Value =
        serde_json::from_str(store_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let old_id = stored["id"].as_str().unwrap().to_string();

    // Supersede with new version
    let store_resp2 = h
        .call_tool(
            "store",
            json!({
                "subject": "supersede-test-app",
                "predicate": "deployed-version",
                "object": "v2.0.0",
                "memory_type": "semantic",
                "supersedes": old_id
            }),
        )
        .await;
    let stored2: Value = serde_json::from_str(
        store_resp2["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(stored2["stored"], true);
    let superseded = stored2["superseded"]
        .as_array()
        .expect("superseded should be an array");
    assert!(
        superseded
            .iter()
            .any(|v| v.as_str() == Some(old_id.as_str())),
        "response should confirm which memory was superseded"
    );

    // Recall without include_invalidated: old should be gone
    let recall_resp = h
        .call_tool(
            "recall",
            json!({
                "query": "supersede-test-app deployed version"
            }),
        )
        .await;
    let results: Value = serde_json::from_str(
        recall_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert!(
        !results
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["id"].as_str())
            .any(|id| id == old_id),
        "superseded memory should not appear in default recall"
    );

    h.shutdown().await;
}

#[tokio::test]
async fn test_concurrent_requests() {
    let mut h = McpTestHarness::new().await;

    // Send 3 requests back-to-back by writing directly to stdin
    let requests = [
        json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "stats",
                "arguments": { "namespace": "default" }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": {
                "name": "stats",
                "arguments": { "namespace": "default", "detailed": true }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "tools/call",
            "params": {
                "name": "reflect",
                "arguments": { "namespace": "default" }
            }
        }),
    ];

    let mut batch = String::new();
    for req in &requests {
        let mut l = serde_json::to_string(req).unwrap();
        l.push('\n');
        batch.push_str(&l);
    }
    h.stdin.write_all(batch.as_bytes()).await.unwrap();
    h.stdin.flush().await.unwrap();

    let mut responses: Vec<Value> = Vec::new();
    for _ in 0..3 {
        let mut rbuf = String::new();
        let read_result = timeout(Duration::from_secs(30), h.reader.read_line(&mut rbuf)).await;
        assert!(
            read_result.is_ok(),
            "timed out waiting for concurrent response"
        );
        let resp: Value = serde_json::from_str(&rbuf).unwrap();
        responses.push(resp);
    }

    let response_ids: Vec<u64> = responses.iter().filter_map(|r| r["id"].as_u64()).collect();
    assert!(response_ids.contains(&10), "missing response for id 10");
    assert!(response_ids.contains(&11), "missing response for id 11");
    assert!(response_ids.contains(&12), "missing response for id 12");

    for resp in &responses {
        assert!(
            resp["result"].is_object(),
            "concurrent response should have result: {resp}"
        );
    }

    h.shutdown().await;
}
