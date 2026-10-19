mod common;

use std::ffi::OsString;
use std::fs;
use std::os::unix::net::UnixListener;
use std::process::{Output, Stdio};

use common::{CmdOpts, TestEnv};
use serde_json::json;

/// A test-held listener plus a PID file naming this process stands in for a daemon.
fn fake_live_daemon(env: &TestEnv) -> UnixListener {
    fs::create_dir_all(&env.run_dir).unwrap();
    let listener = UnixListener::bind(env.run_dir.join("feedfacefeedface.sock")).unwrap();
    write_pid_file(env);
    listener
}

fn write_pid_file(env: &TestEnv) {
    fs::create_dir_all(&env.run_dir).unwrap();
    let body = json!({ "pid": std::process::id(), "db_path": env.db_path });
    fs::write(
        env.run_dir.join("feedfacefeedface.pid"),
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
}

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

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn assert_refused(out: &Output) {
    assert!(!out.status.success(), "expected refusal");
    assert!(
        stderr(out).contains("Close your Claude sessions"),
        "stderr: {}",
        stderr(out)
    );
}

fn assert_ok(out: &Output) {
    assert!(out.status.success(), "stderr: {}", stderr(out));
}

#[tokio::test]
async fn reconcile_fix_refuses_with_live_daemon_on_same_db() {
    let env = TestEnv::new().await;
    let _daemon = fake_live_daemon(&env);
    assert_refused(&run(&env, &["audit", "reconcile", "--fix"]).await);
}

#[tokio::test]
async fn backfill_refuses_with_live_daemon_on_same_db() {
    let env = TestEnv::new().await;
    let _daemon = fake_live_daemon(&env);
    assert_refused(&run(&env, &["backfill", "normalized-predicates"]).await);
}

#[tokio::test]
async fn read_only_commands_run_with_live_daemon() {
    let env = TestEnv::new().await;
    let _daemon = fake_live_daemon(&env);
    assert_ok(&run(&env, &["audit", "reconcile"]).await);
    assert_ok(&run(&env, &["backfill", "normalized-predicates", "--dry-run"]).await);
}

#[tokio::test]
async fn all_succeed_with_no_daemon() {
    let env = TestEnv::new().await;
    assert_ok(&run(&env, &["audit", "reconcile", "--fix"]).await);
    assert_ok(&run(&env, &["backfill", "normalized-predicates"]).await);
}

#[tokio::test]
async fn stale_pid_file_does_not_block() {
    let env = TestEnv::new().await;
    write_pid_file(&env);
    assert_ok(&run(&env, &["audit", "reconcile", "--fix"]).await);
}
