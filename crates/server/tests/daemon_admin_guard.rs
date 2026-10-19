mod common;

use std::ffi::OsString;
use std::fs;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use common::{CmdOpts, TestEnv};
use ferrex_store::{QdrantSidecar, SidecarPolicy, pid_alive};
use serde_json::json;

/// A test-held listener plus a PID file naming this process stands in for a daemon.
fn fake_live_daemon(env: &TestEnv) -> UnixListener {
    fs::create_dir_all(&env.run_dir).unwrap();
    let listener = UnixListener::bind(env.run_dir.join("feedfacefeedface.sock")).unwrap();
    write_pid_file(env);
    listener
}

fn write_pid_file(env: &TestEnv) {
    write_pid_file_for(env, &env.db_path);
}

fn write_pid_file_for(env: &TestEnv, db_path: &Path) {
    fs::create_dir_all(&env.run_dir).unwrap();
    let body = json!({ "pid": std::process::id(), "db_path": db_path });
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

fn sidecar_dir(env: &TestEnv) -> PathBuf {
    env.root.join(".ferrex")
}

async fn nuke(env: &TestEnv, db_path: &Path) -> Output {
    nuke_with_url(env, db_path, None).await
}

async fn nuke_with_url(env: &TestEnv, db_path: &Path, qdrant_url: Option<&str>) -> Output {
    env.ferrex_cmd_with(CmdOpts {
        db_path: Some(db_path.to_path_buf()),
        qdrant_url: qdrant_url.map(|url| Some(url.to_owned())),
        extra_args: vec!["nuke".into(), "--force".into()],
        env: vec![
            ("HOME".into(), env.root.display().to_string()),
            ("FERREX_QDRANT_PORT".into(), env.qdrant_port.to_string()),
        ],
        ..Default::default()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await
    .expect("run nuke")
}

async fn start_sidecar(env: &TestEnv) -> (QdrantSidecar, u32) {
    let sidecar = QdrantSidecar::start(
        "qdrant",
        env.qdrant_port,
        Some(sidecar_dir(env)),
        SidecarPolicy::KillOnDrop,
    )
    .await
    .expect("start sidecar");
    let pid = sidecar.pid().expect("sidecar pid");
    (sidecar, pid)
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[tokio::test]
async fn nuke_default_path_refuses_with_daemon_on_other_db() {
    let env = TestEnv::without_sidecar();
    let _daemon = fake_live_daemon(&env);
    write_pid_file_for(&env, Path::new("/elsewhere/x.db"));
    let db = sidecar_dir(&env).join("ferrex.db");
    fs::create_dir_all(sidecar_dir(&env)).unwrap();
    fs::write(&db, b"x").unwrap();
    assert_refused(&nuke(&env, &db).await);
    assert!(db.exists());
}

#[tokio::test]
async fn nuke_custom_path_refuses_only_for_same_db_and_leaves_qdrant() {
    let env = TestEnv::without_sidecar();
    let (_sidecar, qdrant_pid) = start_sidecar(&env).await;
    let custom = env.root.join("custom/x.db");
    fs::create_dir_all(custom.parent().unwrap()).unwrap();
    fs::write(&custom, b"x").unwrap();

    let _daemon = fake_live_daemon(&env);
    assert_ok(&nuke(&env, &custom).await);
    assert!(!custom.exists());
    assert!(pid_alive(qdrant_pid));

    fs::write(&custom, b"x").unwrap();
    write_pid_file_for(&env, &custom);
    assert_refused(&nuke(&env, &custom).await);
    assert!(custom.exists());
    assert!(pid_alive(qdrant_pid));
}

#[tokio::test]
async fn nuke_default_path_with_qdrant_url_still_stops_sidecar() {
    let env = TestEnv::without_sidecar();
    let (_sidecar, qdrant_pid) = start_sidecar(&env).await;
    let dir = sidecar_dir(&env);
    let db = dir.join("ferrex.db");

    let started = Instant::now();
    let out = nuke_with_url(&env, &db, Some("http://localhost:1")).await;
    assert_ok(&out);
    assert!(started.elapsed() < Duration::from_secs(12));
    assert!(!pid_alive(qdrant_pid), "stdout: {}", stdout(&out));
    assert!(!dir.join("qdrant.pid").exists());
    assert!(!dir.join("qdrant.port").exists());
}

#[tokio::test]
async fn nuke_skips_kill_when_pid_is_not_qdrant() {
    let env = TestEnv::without_sidecar();
    let dir = sidecar_dir(&env);
    fs::create_dir_all(&dir).unwrap();
    let mut sleeper = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    fs::write(dir.join("qdrant.pid"), sleeper.id().to_string()).unwrap();

    let out = nuke(&env, &dir.join("ferrex.db")).await;
    let alive = pid_alive(sleeper.id());
    let _ = sleeper.kill();
    let _ = sleeper.wait();

    assert_ok(&out);
    assert!(stdout(&out).contains("skipped"), "stdout: {}", stdout(&out));
    assert!(alive);
    assert!(!dir.join("qdrant.pid").exists());
}

#[tokio::test]
async fn nuke_with_dead_qdrant_pid_deletes_files() {
    let env = TestEnv::without_sidecar();
    let dir = sidecar_dir(&env);
    fs::create_dir_all(&dir).unwrap();
    let mut done = std::process::Command::new("true").spawn().unwrap();
    done.wait().unwrap();
    fs::write(dir.join("qdrant.pid"), done.id().to_string()).unwrap();

    let out = nuke(&env, &dir.join("ferrex.db")).await;
    assert_ok(&out);
    assert!(
        !stdout(&out).contains("skipped"),
        "stdout: {}",
        stdout(&out)
    );
    assert!(!dir.join("qdrant.pid").exists());
}
