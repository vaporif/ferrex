use ferrex_core::{FerrexConfig, MemoryService};

pub fn run_normalized_predicates(
    config: FerrexConfig,
    namespace: Option<String>,
    dry_run: bool,
) -> eyre::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let service = MemoryService::from_config(config).await?;
            let report = service
                .backfill_normalized_predicates(namespace.as_deref(), dry_run)
                .await?;
            println!(
                "backfill: scanned={} updated={} dry_run={dry_run}",
                report.scanned, report.updated,
            );
            Ok::<_, eyre::Report>(())
        })
}
