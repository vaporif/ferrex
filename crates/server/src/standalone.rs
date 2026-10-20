use std::sync::Arc;

use ferrex_core::{FerrexConfig, MemoryService};
use rmcp::{ServiceExt, transport::stdio};
use tokio::signal::unix::SignalKind;

use crate::server::{FerrexServer, SessionDefaults};

pub fn run(config: FerrexConfig) -> eyre::Result<()> {
    let namespace = config.namespace.clone();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
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

            let mut sigterm = tokio::signal::unix::signal(SignalKind::terminate())?;
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
        })
}
