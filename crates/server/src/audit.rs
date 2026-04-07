use ferrex_core::{FerrexConfig, MemoryService};

pub fn run_reconcile(
    config: FerrexConfig,
    fix: bool,
    sample: Option<usize>,
    format: &str,
) -> eyre::Result<()> {
    if sample.is_some() {
        tracing::warn!("--sample is not yet implemented, running full audit");
    }
    let audit_fix_limit = config.reconciliation.audit_fix_limit;
    let is_json = format.eq_ignore_ascii_case("json");
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let service = MemoryService::from_config(config).await?;
            let report = service.audit_reconcile(fix, audit_fix_limit).await?;

            if is_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).unwrap_or_default()
                );
            } else {
                println!(
                    "Qdrant-only orphans (no SQLite row):  {}",
                    report.qdrant_only.len()
                );
                if !report.qdrant_only.is_empty() {
                    println!("  {}", report.qdrant_only.join(", "));
                }
                println!();
                println!(
                    "SQLite-only orphans (no Qdrant point): {}",
                    report.sqlite_only.len()
                );
                if !report.sqlite_only.is_empty() {
                    println!("  {}", report.sqlite_only.join(", "));
                }
                println!();
                let total = report.qdrant_only.len() + report.sqlite_only.len();
                if fix {
                    println!("Fixed: {} Qdrant orphans deleted", report.fixed.len());
                } else {
                    println!("Total: {total} inconsistencies found");
                    if total > 0 {
                        println!("Run with --fix to resolve.");
                    }
                }
            }

            service.shutdown().await;
            Ok::<_, eyre::Report>(())
        })
}
