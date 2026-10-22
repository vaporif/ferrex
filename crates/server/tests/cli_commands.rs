mod common;

use std::ffi::OsString;
use std::process::{Output, Stdio};
use std::time::Duration;

use common::{CmdOpts, ProxyClient, TestEnv};
use ferrex_store::{MetadataStore, PendingOp, PendingOpKind, SqliteStore};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

async fn run(env: &TestEnv, args: &[&str]) -> Output {
    env.ferrex_cmd_with(CmdOpts {
        extra_args: args.iter().map(OsString::from).collect(),
        ..Default::default()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await
    .expect("run ferrex")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout(out)
}

/// Stores one memory through a standalone server, so the journal has a completed op.
async fn store_one(env: &TestEnv) -> String {
    let mut proxy = ProxyClient::spawn(
        env,
        CmdOpts {
            env: vec![("FERREX_NO_DAEMON".into(), "1".into())],
            ..Default::default()
        },
    );
    proxy.initialize().await;
    let resp = proxy
        .request(
            "tools/call",
            json!({"name": "store", "arguments": {"content": "the build cache lives in s3"}}),
        )
        .await;
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let stored: Value = serde_json::from_str(text).unwrap();
    proxy.close_stdin();
    proxy.wait_exit(Duration::from_secs(30)).await;
    stored["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn journal_show_text_and_json() {
    let env = TestEnv::new().await;
    let id = store_one(&env).await;

    let text = ok(&run(&env, &["journal", "show"]).await);
    assert!(text.contains("Pending (0):"), "{text}");
    assert!(text.contains("(none)"), "{text}");
    assert!(text.contains(&id), "{text}");

    let json_out = ok(&run(
        &env,
        &["journal", "show", "--format", "json", "--since", "1h"],
    )
    .await);
    let parsed: Value = serde_json::from_str(&json_out).unwrap();
    assert_eq!(parsed["pending"], json!([]));
    let completed = parsed["completed"].as_array().unwrap();
    assert!(completed.iter().any(|op| op["memory_id"] == id), "{parsed}");

    let only_pending = ok(&run(
        &env,
        &["journal", "show", "--status", "pending", "--format", "json"],
    )
    .await);
    let parsed: Value = serde_json::from_str(&only_pending).unwrap();
    assert!(parsed.get("completed").is_none(), "{parsed}");

    let only_completed = ok(&run(
        &env,
        &["journal", "show", "--status", "completed", "--since", "7d"],
    )
    .await);
    assert!(!only_completed.contains("Pending"), "{only_completed}");
    ok(&run(&env, &["journal", "show", "--since", "24h", "--limit", "1"]).await);
}

/// A daemon's in-flight op must show up, not get rolled back by the CLI's own startup.
#[tokio::test]
async fn journal_show_lists_pending_ops_without_recovering_them() {
    let env = TestEnv::new().await;
    let memory_id = store_one(&env).await;
    SqliteStore::open(&env.db_path)
        .unwrap()
        .insert_pending_op(&PendingOp {
            op_id: uuid::Uuid::now_v7().to_string(),
            kind: PendingOpKind::Store,
            memory_id: memory_id.clone(),
            namespace: "default".into(),
            target_id: None,
            qdrant_written: true,
            started_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

    for _ in 0..2 {
        let text = ok(&run(&env, &["journal", "show", "--status", "pending"]).await);
        assert!(text.contains("Pending (1):"), "{text}");
        assert!(text.contains(&memory_id), "{text}");
    }
    let report = ok(&run(&env, &["diagnose"]).await);
    assert!(report.contains("pending ops: 1"), "{report}");
}

#[tokio::test]
async fn journal_show_rejects_bad_since() {
    let env = TestEnv::new().await;
    let out = run(&env, &["journal", "show", "--since", "3w"]).await;
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("invalid --since value: 3w"),
        "{out:?}"
    );
}

#[tokio::test]
async fn audit_reconcile_text_and_json() {
    let env = TestEnv::new().await;
    store_one(&env).await;

    let text = ok(&run(&env, &["audit", "reconcile", "--sample", "5"]).await);
    assert!(text.contains("Total: 0 inconsistencies found"), "{text}");

    let json_out = ok(&run(&env, &["audit", "reconcile", "--format", "json"]).await);
    let report: Value = serde_json::from_str(&json_out).unwrap();
    assert_eq!(report["qdrant_only"], json!([]));
    assert_eq!(report["sqlite_only"], json!([]));

    let fixed = ok(&run(&env, &["audit", "reconcile", "--fix"]).await);
    assert!(fixed.contains("Fixed: 0 Qdrant orphans deleted"), "{fixed}");
}

#[tokio::test]
async fn diagnose_prints_store_health() {
    let env = TestEnv::new().await;
    store_one(&env).await;
    let out = ok(&run(&env, &["diagnose"]).await);
    assert!(out.starts_with("ferrex v"), "{out}");
    assert!(out.contains("memories: 1"), "{out}");
    assert!(out.contains("pending ops: 0"), "{out}");
}

#[tokio::test]
async fn standalone_exits_on_sigterm_with_stdin_open() {
    let env = TestEnv::new().await;
    let mut proxy = ProxyClient::spawn(
        &env,
        CmdOpts {
            env: vec![("FERREX_NO_DAEMON".into(), "1".into())],
            ..Default::default()
        },
    );
    proxy.initialize().await;
    // Blocks without driving the runtime, so the proxy's stdin stays open and nothing
    // reads its output: the server must exit on the signal alone, not SIGKILL.
    common::terminate(proxy.pid());
    let status = proxy.wait_exit(Duration::from_secs(10)).await;
    assert!(
        status.is_some_and(|s| s.success()),
        "{status:?}; stderr:\n{}",
        proxy.stderr_text()
    );
}

#[tokio::test]
async fn unimplemented_commands_fail() {
    let env = TestEnv::without_sidecar();
    for args in [
        &["re-embed", "--dry-run"][..],
        &["backup"][..],
        &["restore", "--from", "/nonexistent"][..],
    ] {
        let out = run(&env, args).await;
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("not yet implemented"),
            "{args:?}: {out:?}"
        );
    }
}

#[tokio::test]
async fn log_format_and_file_options() {
    let env = TestEnv::without_sidecar();
    for (format, to_file) in [("json", true), ("text", true), ("json", false)] {
        let log_file = env.root.join(format!("{format}-{to_file}.log"));
        let mut vars = vec![
            ("FERREX_LOG_FORMAT".to_owned(), format.to_owned()),
            ("FERREX_LOG".to_owned(), "debug".to_owned()),
        ];
        if to_file {
            vars.push(("FERREX_LOG_FILE".to_owned(), log_file.display().to_string()));
        }
        let out = env
            .ferrex_cmd_with(CmdOpts {
                extra_args: vec!["daemon-paths".into()],
                env: vars,
                ..Default::default()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .unwrap();
        ok(&out);
        assert_eq!(log_file.exists(), to_file, "{format} {to_file}");
    }
}

async fn nuke_prompted(env: &TestEnv, db: &std::path::Path, answer: Option<&str>) -> Output {
    let mut extra_args: Vec<OsString> = vec!["nuke".into()];
    if answer.is_none() {
        extra_args.push("--force".into());
    }
    let mut child = env
        .ferrex_cmd_with(CmdOpts {
            db_path: Some(db.to_path_buf()),
            extra_args,
            env: vec![("HOME".into(), env.root.display().to_string())],
            ..Default::default()
        })
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    if let Some(answer) = answer {
        tokio::io::AsyncWriteExt::write_all(&mut stdin, format!("{answer}\n").as_bytes())
            .await
            .unwrap();
    }
    drop(stdin);
    child.wait_with_output().await.unwrap()
}

#[tokio::test]
async fn nuke_reports_nothing_to_delete() {
    let env = TestEnv::without_sidecar();
    let db = env.root.join("custom").join("ferrex.db");
    let out = ok(&nuke_prompted(&env, &db, None).await);
    assert!(out.contains("(nothing found)"), "{out}");
}

#[tokio::test]
async fn nuke_asks_before_deleting() {
    let env = TestEnv::without_sidecar();
    let dir = env.root.join("custom");
    std::fs::create_dir_all(dir.join("qdrant-data")).unwrap();
    let db = dir.join("ferrex.db");
    std::fs::write(&db, b"db").unwrap();
    std::fs::write(dir.join("qdrant.pid"), b"1").unwrap();
    std::fs::write(dir.join("qdrant.port"), b"1 2").unwrap();

    let declined = ok(&nuke_prompted(&env, &db, Some("n")).await);
    assert!(declined.contains("Aborted."), "{declined}");
    assert!(db.exists());

    let accepted = ok(&nuke_prompted(&env, &db, Some("y")).await);
    assert!(accepted.contains("Qdrant data:"), "{accepted}");
    assert!(accepted.contains("Done."), "{accepted}");
    assert!(!db.exists());
    assert!(!dir.join("qdrant-data").exists());
    assert!(!dir.join("qdrant.pid").exists());
}
