mod common;

use std::time::Duration;

use serde_json::{Value, json};

use common::{CmdOpts, DaemonGuard, ProxyClient, TestEnv};
use pretty_assertions::assert_eq;

const PID_FILE_WITHIN: Duration = Duration::from_secs(30);

fn namespace_opts(namespace: &str) -> CmdOpts {
    CmdOpts {
        env: vec![("FERREX_NAMESPACE".to_owned(), namespace.to_owned())],
        ..Default::default()
    }
}

async fn session(env: &TestEnv, namespace: &str) -> ProxyClient {
    let mut proxy = ProxyClient::spawn(env, namespace_opts(namespace));
    let resp = proxy.initialize().await;
    assert!(resp["result"]["serverInfo"].is_object(), "{resp}");
    proxy
}

async fn call_tool(proxy: &mut ProxyClient, name: &str, arguments: Value) -> Value {
    let resp = proxy
        .request("tools/call", json!({"name": name, "arguments": arguments}))
        .await;
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{name} failed: {resp}"));
    serde_json::from_str(text).expect("tool result json")
}

fn ids(result: &Value) -> Vec<&str> {
    result
        .as_array()
        .unwrap_or_else(|| panic!("expected array: {result}"))
        .iter()
        .filter_map(|r| r["id"].as_str())
        .collect()
}

async fn store_episodic(proxy: &mut ProxyClient, content: &str, entity: &str) -> String {
    let stored = call_tool(
        proxy,
        "store",
        json!({"content": content, "memory_type": "episodic", "entities": [entity]}),
    )
    .await;
    assert_eq!(stored["stored"], true, "{stored}");
    stored["id"].as_str().expect("id").to_owned()
}

#[tokio::test]
async fn two_namespaces_on_one_daemon_stay_separate() {
    let env = TestEnv::new().await;
    let paths = env.paths().await;
    let mut a = session(&env, "ns_a").await;
    let _guard = DaemonGuard::from_pid_file(&paths.pid_file, PID_FILE_WITHIN).await;
    let mut b = session(&env, "ns_b").await;
    let pid_files = std::fs::read_dir(&paths.run_dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "pid")
        })
        .count();
    assert_eq!(pid_files, 1, "both sessions share one daemon");

    let id_a = store_episodic(&mut a, "Deployed the billing service", "billing").await;
    let id_b = store_episodic(&mut b, "Rotated the gateway certificates", "gateway").await;

    let recall_a = call_tool(&mut a, "recall", json!({"query": "service deployment"})).await;
    assert_eq!(ids(&recall_a), vec![id_a.as_str()], "{recall_a}");
    let recall_b = call_tool(&mut b, "recall", json!({"query": "certificates"})).await;
    assert_eq!(ids(&recall_b), vec![id_b.as_str()], "{recall_b}");

    let timeline_a = call_tool(&mut a, "timeline", json!({"entity": "billing"})).await;
    let entries_a = timeline_a["entries"].as_array().expect("entries");
    assert_eq!(entries_a.len(), 1, "{timeline_a}");
    let timeline_a_other = call_tool(&mut a, "timeline", json!({"entity": "gateway"})).await;
    assert!(
        timeline_a_other["entries"]
            .as_array()
            .expect("entries")
            .is_empty(),
        "{timeline_a_other}"
    );
    let timeline_b = call_tool(&mut b, "timeline", json!({"entity": "gateway"})).await;
    assert_eq!(timeline_b["entries"].as_array().expect("entries").len(), 1);
    let timeline_b_other = call_tool(&mut b, "timeline", json!({"entity": "billing"})).await;
    assert!(
        timeline_b_other["entries"]
            .as_array()
            .expect("entries")
            .is_empty(),
        "{timeline_b_other}"
    );
}

#[tokio::test]
async fn explicit_namespace_param_wins() {
    let env = TestEnv::new().await;
    let paths = env.paths().await;
    let mut a = session(&env, "ns_a").await;
    let _guard = DaemonGuard::from_pid_file(&paths.pid_file, PID_FILE_WITHIN).await;
    let mut b = session(&env, "ns_b").await;

    let stored = call_tool(
        &mut a,
        "store",
        json!({
            "content": "Migrated the orders table",
            "memory_type": "episodic",
            "namespace": "ns_b",
        }),
    )
    .await;
    let id = stored["id"].as_str().expect("id");

    let in_b = call_tool(&mut b, "recall", json!({"query": "orders table"})).await;
    assert_eq!(ids(&in_b), vec![id], "{in_b}");
    let in_a = call_tool(&mut a, "recall", json!({"query": "orders table"})).await;
    assert!(ids(&in_a).is_empty(), "{in_a}");

    let explicit = call_tool(
        &mut a,
        "recall",
        json!({"query": "orders table", "namespace": "ns_b"}),
    )
    .await;
    assert_eq!(ids(&explicit), vec![id], "{explicit}");
}

#[tokio::test]
async fn taxonomy_unscoped_uses_session_namespace() {
    let env = TestEnv::new().await;
    let paths = env.paths().await;
    let mut a = session(&env, "ns_a").await;
    let _guard = DaemonGuard::from_pid_file(&paths.pid_file, PID_FILE_WITHIN).await;
    let mut b = session(&env, "ns_b").await;
    store_episodic(&mut a, "Deployed the billing service", "billing").await;
    store_episodic(&mut b, "Rotated the gateway certificates", "gateway").await;

    let taxonomy = call_tool(&mut b, "taxonomy", json!({})).await;
    assert_eq!(taxonomy["namespace"], "ns_b", "{taxonomy}");
    assert_eq!(taxonomy["total_memories"], 1, "{taxonomy}");
    let listed: Vec<&str> = taxonomy["all_namespaces"]
        .as_array()
        .expect("all_namespaces listed when unscoped")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        listed.contains(&"ns_a") && listed.contains(&"ns_b"),
        "{taxonomy}"
    );
}
