use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use crate::StoreError;

const HEALTH_CHECK_CLIENT_TIMEOUT: Duration = Duration::from_secs(2);
const HEALTH_CHECK_INITIAL_DELAY: Duration = Duration::from_millis(100);
const HEALTH_CHECK_MAX_TIMEOUT: Duration = Duration::from_secs(5);
const HEALTH_CHECK_MAX_DELAY: Duration = Duration::from_secs(1);
const MIN_PORT: u16 = 2;

pub struct QdrantSidecar {
    pid_file: PathBuf,
    process: Option<Child>,
    port: u16,
}

impl QdrantSidecar {
    pub async fn start(bin: &str, port: u16) -> Result<Self, StoreError> {
        let ferrex_dir = dirs_home().join(".ferrex");
        fs::create_dir_all(&ferrex_dir)
            .map_err(|e| StoreError::Sidecar(format!("failed to create ~/.ferrex: {e}")))?;

        let pid_file = ferrex_dir.join("qdrant.pid");
        let data_dir = ferrex_dir.join("qdrant-data");
        let config_path = ferrex_dir.join("qdrant-config.yaml");

        // Check existing PID
        if let Some(existing_pid) = read_pid(&pid_file) {
            if is_process_alive(existing_pid) {
                tracing::info!(pid = existing_pid, "reusing existing Qdrant process");
                let sidecar = Self {
                    pid_file,
                    process: None,
                    port,
                };
                sidecar.health_check().await?;
                return Ok(sidecar);
            }
            tracing::info!(pid = existing_pid, "cleaning stale PID file");
            let _ = fs::remove_file(&pid_file);
        }

        if port < MIN_PORT {
            return Err(StoreError::Sidecar(format!(
                "port must be >= {MIN_PORT} (got {port}), http_port is port - 1"
            )));
        }

        // Write config
        fs::create_dir_all(&data_dir)
            .map_err(|e| StoreError::Sidecar(format!("failed to create qdrant-data: {e}")))?;
        let http_port = port - 1;
        let config = format!(
            "storage:\n  storage_path: {}\nservice:\n  grpc_port: {port}\n  http_port: {http_port}\n",
            data_dir.display()
        );
        fs::write(&config_path, config)
            .map_err(|e| StoreError::Sidecar(format!("failed to write qdrant config: {e}")))?;

        // Spawn Qdrant
        tracing::info!(bin = bin, port = port, "starting Qdrant sidecar");
        let child = Command::new(bin)
            .arg("--config-path")
            .arg(&config_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| StoreError::Sidecar(format!("failed to spawn qdrant: {e}")))?;

        let pid = child.id();
        fs::write(&pid_file, pid.to_string())
            .map_err(|e| StoreError::Sidecar(format!("failed to write PID file: {e}")))?;

        let sidecar = Self {
            pid_file,
            process: Some(child),
            port,
        };

        sidecar.health_check().await?;
        Ok(sidecar)
    }

    pub fn url(&self) -> String {
        format!("http://localhost:{}", self.port)
    }

    async fn health_check(&self) -> Result<(), StoreError> {
        let url = self.url();
        let client = qdrant_client::Qdrant::from_url(&url)
            .timeout(HEALTH_CHECK_CLIENT_TIMEOUT)
            .build()
            .map_err(|e| StoreError::Sidecar(e.to_string()))?;

        let mut delay = HEALTH_CHECK_INITIAL_DELAY;
        let start = std::time::Instant::now();

        loop {
            match client.health_check().await {
                Ok(_) => {
                    tracing::info!("Qdrant sidecar healthy");
                    return Ok(());
                }
                Err(e) => {
                    if start.elapsed() > HEALTH_CHECK_MAX_TIMEOUT {
                        return Err(StoreError::Sidecar(format!(
                            "Qdrant health check timed out after {HEALTH_CHECK_MAX_TIMEOUT:?}: {e}"
                        )));
                    }
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(HEALTH_CHECK_MAX_DELAY);
                }
            }
        }
    }

    pub fn shutdown(&mut self) {
        if let Some(mut child) = self.process.take() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(&self.pid_file);
            tracing::info!("Qdrant sidecar stopped");
        }
    }
}

impl Drop for QdrantSidecar {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn dirs_home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn is_process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires qdrant binary on PATH"]
    async fn test_sidecar_lifecycle() {
        let mut sidecar = QdrantSidecar::start("qdrant", 6340).await.unwrap();
        assert!(sidecar.url().contains("6340"));
        sidecar.shutdown();
        assert!(!sidecar.pid_file.exists());
    }
}
