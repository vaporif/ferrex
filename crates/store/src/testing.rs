//! Test helpers shared with other crates' tests via the `testing` feature.

use std::fs::{self, OpenOptions};
use std::time::Duration;

// Nextest runs each test in its own process, and tests across crates run concurrently,
// so claim ports via marker files to keep qdrant tests off each other's gRPC/HTTP pair.
fn claim(port: u16) -> bool {
    let dir = std::env::temp_dir().join("ferrex-test-ports");
    let _ = fs::create_dir_all(&dir);
    let marker = dir.join(port.to_string());
    let stale = fs::metadata(&marker)
        .and_then(|m| m.modified())
        .is_ok_and(|m| m.elapsed().is_ok_and(|e| e > Duration::from_secs(600)));
    if stale {
        let _ = fs::remove_file(&marker);
    }
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)
        .is_ok()
}

/// A free gRPC port whose HTTP neighbour (`port - 1`, used by the sidecar) is free too,
/// claimed so no other test process picks the same pair.
pub fn free_port() -> u16 {
    loop {
        if let Some(p) = portpicker::pick_unused_port()
            && p > 2
            && portpicker::is_free(p - 1)
            && claim(p)
            && claim(p - 1)
        {
            return p;
        }
    }
}
