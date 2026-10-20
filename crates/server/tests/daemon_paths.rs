use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

fn ferrex(cwd: &Path, run_dir: &Path, args: &[&str]) -> Output {
    let bin = std::env::var("NEXTEST_BIN_EXE_ferrex")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_ferrex").to_string());
    Command::new(bin)
        .current_dir(cwd)
        .env("FERREX_RUN_DIR", run_dir)
        .env("HOME", cwd)
        .args(args)
        .output()
        .expect("run ferrex")
}

fn key_of(out: &Output) -> String {
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let line = String::from_utf8(out.stdout.clone()).expect("utf8");
    assert_eq!(line.trim_end().lines().count(), 1, "stdout: {line}");
    let json: Value = serde_json::from_str(&line).expect("json");
    json["key"].as_str().expect("key").to_string()
}

#[test]
fn daemon_paths_same_key_across_cwds_and_spellings() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let run = root.join("run");
    let a = root.join("a");
    let b = root.join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let cfg = root.join("ferrex.toml");
    let cfg = cfg.to_str().unwrap();
    let abs = root.join("data/x.db");

    let first = ferrex(
        &a,
        &run,
        &[
            "--config-path",
            cfg,
            "--db-path",
            abs.to_str().unwrap(),
            "daemon-paths",
        ],
    );
    let second = ferrex(
        &b,
        &run,
        &[
            "--config-path",
            cfg,
            "--db-path",
            "../data/x.db",
            "daemon-paths",
        ],
    );
    assert_eq!(key_of(&first), key_of(&second));
}

#[test]
fn daemon_paths_stable_across_processes_with_predicate_groups() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let cfg = root.join("ferrex.toml");
    std::fs::write(
        &cfg,
        r#"
[predicates.groups]
likes = ["loves", "enjoys"]
works_at = ["employed_by"]
lives_in = ["resides_in"]

[namespaces.work.predicates.groups]
owns = ["has"]
"#,
    )
    .unwrap();
    let db = root.join("x.db");
    let args = [
        "--config-path",
        cfg.to_str().unwrap(),
        "--db-path",
        db.to_str().unwrap(),
        "--namespace",
        "work",
        "daemon-paths",
    ];
    let run = root.join("run");
    let first = key_of(&ferrex(&root, &run, &args));
    let second = key_of(&ferrex(&root, &run, &args));
    assert_eq!(first, second);
}

#[test]
fn daemon_paths_reports_unusable_run_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let not_a_dir = root.join("file");
    std::fs::write(&not_a_dir, "x").unwrap();
    let cfg = root.join("ferrex.toml");
    let db = root.join("x.db");
    let out = ferrex(
        &root,
        &not_a_dir,
        &[
            "--config-path",
            cfg.to_str().unwrap(),
            "--db-path",
            db.to_str().unwrap(),
            "daemon-paths",
        ],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(not_a_dir.to_str().unwrap()), "{stderr}");
}
