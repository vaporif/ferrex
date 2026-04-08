use ferrex_core::{FerrexConfig, MemoryService};

pub fn diagnose(config: FerrexConfig) -> eyre::Result<()> {
    runtime()?.block_on(async move {
        let service = MemoryService::from_config(config).await?;
        let report = service.diagnostics().await?;

        println!("ferrex v{} | {}", report.version, report.embedding_model);
        println!();
        println!("Qdrant:");
        println!(
            "  url: {}{}",
            report.qdrant_url,
            report
                .qdrant_pid
                .map(|p| format!(" (sidecar, pid {p})"))
                .unwrap_or_default()
        );
        println!(
            "  collection: {} -- {} points",
            report.collection, report.memory_count
        );
        println!();
        println!("SQLite:");
        #[allow(clippy::cast_precision_loss)]
        let size_mb = report.sqlite_size_bytes as f64 / 1_048_576.0;
        println!("  path: {} -- {size_mb:.1} MB", report.sqlite_path);
        println!(
            "  memories: {} | entities: {}",
            report.memory_count, report.entity_count
        );
        println!("  pending ops: {}", report.pending_ops);
        println!();
        print_cache_stats(&report.cache);

        if !report.recent_ops.is_empty() {
            println!();
            println!("Last {} operations:", report.recent_ops.len());
            for op in &report.recent_ops {
                let ago = chrono::Utc::now() - op.timestamp;
                let ago_str = if ago.num_seconds() < 60 {
                    format!("{}s ago", ago.num_seconds())
                } else if ago.num_minutes() < 60 {
                    format!("{}m ago", ago.num_minutes())
                } else {
                    format!("{}h ago", ago.num_hours())
                };
                println!(
                    "  {:<8} {:<30} {:>6}ms  {:<12} {}",
                    op.kind,
                    op.detail.chars().take(30).collect::<String>(),
                    op.duration_ms,
                    op.outcome,
                    ago_str
                );
            }
        }

        service.shutdown().await;
        Ok(())
    })
}

#[allow(clippy::cast_precision_loss)]
fn print_cache_stats(cache: &ferrex_core::CacheStats) {
    let emb_total = cache.embedding_hits + cache.embedding_misses;
    let emb_rate = if emb_total > 0 {
        cache.embedding_hits as f64 / emb_total as f64 * 100.0
    } else {
        0.0
    };
    let res_total = cache.result_hits + cache.result_misses;
    let res_rate = if res_total > 0 {
        cache.result_hits as f64 / res_total as f64 * 100.0
    } else {
        0.0
    };
    println!("Cache:");
    println!(
        "  embedding: {}/{} entries, {emb_rate:.0}% hit rate",
        cache.embedding_len, cache.embedding_capacity
    );
    println!(
        "  result: {}/{} entries, {res_rate:.0}% hit rate",
        cache.result_len, cache.result_capacity
    );
}

pub fn journal_show(
    config: FerrexConfig,
    status: Option<String>,
    limit: usize,
    since: Option<String>,
    format: String,
) -> eyre::Result<()> {
    runtime()?.block_on(async move {
        let service = MemoryService::from_config(config).await?;

        let since_dt = since.as_deref().map(parse_since).transpose()?;

        let status_filter = status.as_deref();
        let show_pending = status_filter.is_none() || status_filter == Some("pending");
        let show_completed = status_filter.is_none()
            || status_filter == Some("completed")
            || status_filter == Some("failed");

        service.prune_journal().await?;

        if format.eq_ignore_ascii_case("json") {
            print_journal_json(
                &service,
                show_pending,
                show_completed,
                status_filter,
                limit,
                since_dt,
            )
            .await?;
        } else {
            print_journal_text(
                &service,
                show_pending,
                show_completed,
                status_filter,
                limit,
                since_dt,
            )
            .await?;
        }

        service.shutdown().await;
        Ok(())
    })
}

fn parse_since(s: &str) -> eyre::Result<chrono::DateTime<chrono::Utc>> {
    let now = chrono::Utc::now();
    match s {
        "1h" => Ok(now - chrono::Duration::hours(1)),
        "24h" => Ok(now - chrono::Duration::hours(24)),
        "7d" => Ok(now - chrono::Duration::days(7)),
        _ => Err(eyre::eyre!(
            "invalid --since value: {s}. Use 1h, 24h, or 7d."
        )),
    }
}

async fn print_journal_json(
    service: &MemoryService,
    show_pending: bool,
    show_completed: bool,
    status_filter: Option<&str>,
    limit: usize,
    since_dt: Option<chrono::DateTime<chrono::Utc>>,
) -> eyre::Result<()> {
    let mut json_obj = serde_json::Map::new();

    if show_pending {
        let pending = service.list_pending_ops().await?;
        json_obj.insert(
            "pending".to_string(),
            serde_json::json!(
                pending
                    .iter()
                    .map(|op| serde_json::json!({
                        "op_id": op.op_id,
                        "kind": op.kind.as_str(),
                        "memory_id": op.memory_id,
                        "namespace": op.namespace,
                        "started_at": op.started_at.to_rfc3339(),
                    }))
                    .collect::<Vec<_>>()
            ),
        );
    }

    if show_completed {
        let completed = service
            .list_completed_ops(status_filter, limit, since_dt)
            .await?;
        json_obj.insert(
            "completed".to_string(),
            serde_json::json!(
                completed
                    .iter()
                    .map(|op| serde_json::json!({
                        "op_id": op.op_id,
                        "kind": op.kind.as_str(),
                        "memory_id": op.memory_id,
                        "outcome": op.outcome,
                        "duration_ms": op.duration_ms,
                        "completed_at": op.completed_at.to_rfc3339(),
                    }))
                    .collect::<Vec<_>>()
            ),
        );
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&json_obj).unwrap_or_default()
    );
    Ok(())
}

async fn print_journal_text(
    service: &MemoryService,
    show_pending: bool,
    show_completed: bool,
    status_filter: Option<&str>,
    limit: usize,
    since_dt: Option<chrono::DateTime<chrono::Utc>>,
) -> eyre::Result<()> {
    if show_pending {
        let pending = service.list_pending_ops().await?;
        println!("Pending ({}):", pending.len());
        if pending.is_empty() {
            println!("  (none)");
        } else {
            for op in &pending {
                println!(
                    "  {}  {}  {}  since {}",
                    op.kind.as_str(),
                    op.memory_id,
                    op.namespace,
                    op.started_at.format("%Y-%m-%d %H:%M:%S")
                );
            }
        }
        println!();
    }

    if show_completed {
        let completed = service
            .list_completed_ops(status_filter, limit, since_dt)
            .await?;
        println!("Completed (last {}):", completed.len());
        for op in &completed {
            println!(
                "  {}  {}  {}  {:>5}  {:>6}ms",
                op.completed_at.format("%Y-%m-%d %H:%M:%S"),
                op.kind.as_str(),
                op.memory_id,
                op.outcome,
                op.duration_ms,
            );
        }
    }

    Ok(())
}

fn runtime() -> eyre::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(Into::into)
}
