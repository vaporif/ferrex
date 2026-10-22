use std::sync::Arc;
use std::time::Duration;

use ferrex_core::{FerrexConfig, MemoryService};
use rmcp::{ServiceExt, transport::stdio};
use tokio::signal::unix::SignalKind;

use crate::server::{FerrexServer, SessionDefaults};

const RUNTIME_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

pub fn run(config: FerrexConfig) -> eyre::Result<()> {
    let namespace = config.namespace.clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        // Installed first so a SIGTERM during startup or the handshake still shuts down cleanly.
        let mut sigterm = tokio::signal::unix::signal(SignalKind::terminate())?;
        let service = MemoryService::from_config(config).await?;
        let (service, mut sidecar) = service.into_parts();
        eprintln!("ferrex ready");
        let service = Arc::new(service);
        let defaults = SessionDefaults { namespace };
        let srv = FerrexServer::with_session(Arc::clone(&service), defaults);
        let (stdin, stdout) = stdio();
        let running = srv
            .serve((stdin, stdout))
            .await
            .map_err(|e| eyre::eyre!("MCP server error: {e}"))?;

        tokio::select! {
            result = running.waiting() => {
                result.map_err(|e| eyre::eyre!("MCP server error: {e}"))?;
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received SIGINT, shutting down");
            }
            _ = sigterm.recv() => {
                tracing::info!("received SIGTERM, shutting down");
            }
        }

        service.shutdown().await;
        if let Some(sc) = sidecar.as_mut() {
            sc.shutdown();
        }
        Ok(())
    });
    // A plain drop waits for the blocking thread stuck reading stdin, which never returns
    // after a signal while the client keeps stdin open.
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_GRACE);
    result
}
