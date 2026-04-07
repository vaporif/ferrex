use std::process::Stdio;

use ferrex_store::QdrantSidecar;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

struct McpTestHarness {
    child: Child,
    stdin: tokio::process::ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    _sidecar: QdrantSidecar,
    _temp_dir: tempfile::TempDir,
    next_id: u64,
}

impl McpTestHarness {
    async fn new() -> Self {
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

        let mut harness = Self {
            child,
            stdin,
            reader,
            _sidecar: sidecar,
            _temp_dir: temp_dir,
            next_id: 1,
        };

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
