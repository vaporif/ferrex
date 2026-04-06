use ferrex_core::{FerrexConfig, MemoryService};

pub fn run_reconcile(config: FerrexConfig, fix: bool, _sample: Option<usize>) -> eyre::Result<()> {
    let audit_fix_limit = config.reconciliation.audit_fix_limit;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let service = MemoryService::from_config(config).await?;
            let report = service.audit_reconcile(fix, audit_fix_limit).await?;
            println!(
                "audit: sqlite_only={} qdrant_only={} fixed={}",
                report.sqlite_only, report.qdrant_only, report.fixed,
            );
            Ok::<_, eyre::Report>(())
        })
}
