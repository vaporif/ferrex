mod audit;
mod backfill;
mod cli;
mod commands;
mod hint;
mod params;
mod server;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use ferrex_core::MemoryService;
use rmcp::{ServiceExt, transport::stdio};

use cli::{AuditCommand, BackfillCommand, Command, JournalCommand};

fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    let mut cli = cli::Cli::parse();

    init_tracing();

    match cli.command.take() {
        Some(Command::Audit {
            audit:
                AuditCommand::Reconcile {
                    fix,
                    sample,
                    format,
                },
        }) => audit::run_reconcile(cli::build_config(cli)?, fix, sample, &format),

        Some(Command::Backfill {
            backfill: BackfillCommand::NormalizedPredicates { namespace, dry_run },
        }) => backfill::run_normalized_predicates(cli::build_config(cli)?, namespace, dry_run),

        Some(Command::Diagnose) => commands::diagnose(cli::build_config(cli)?),

        Some(Command::Nuke { force }) => commands::nuke(cli::build_config(cli)?, force),

        Some(Command::Journal {
            journal:
                JournalCommand::Show {
                    status,
                    limit,
                    since,
                    format,
                },
        }) => commands::journal_show(cli::build_config(cli)?, status, limit, since, format),

        Some(Command::ReEmbed { dry_run: _ }) => {
            eprintln!("ferrex re-embed is not yet implemented");
            std::process::exit(1);
        }
        Some(Command::Backup { output: _ }) => {
            eprintln!("ferrex backup is not yet implemented");
            std::process::exit(1);
        }
        Some(Command::Restore { from: _ }) => {
            eprintln!("ferrex restore is not yet implemented");
            std::process::exit(1);
        }
        None => run_server(cli::build_config(cli)?),
    }
}

fn run_server(config: ferrex_core::FerrexConfig) -> eyre::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let service = MemoryService::from_config(config).await?;
            let (service, mut sidecar) = service.into_parts();
            eprintln!("ferrex ready");
            let service = Arc::new(service);
            let srv = server::FerrexServer::new(Arc::clone(&service));
            let (stdin, stdout) = stdio();
            let running = srv
                .serve((stdin, stdout))
                .await
                .map_err(|e| eyre::eyre!("MCP server error: {e}"))?;

            #[cfg(unix)]
            {
                let mut sigterm =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("failed to install SIGTERM handler");

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
            }

            #[cfg(not(unix))]
            {
                tokio::select! {
                    result = running.waiting() => {
                        result.map_err(|e| eyre::eyre!("MCP server error: {e}"))?;
                    }
                    _ = tokio::signal::ctrl_c() => {
                        tracing::info!("received SIGINT, shutting down");
                    }
                }
            }

            service.shutdown().await;
            if let Some(ref mut sc) = sidecar {
                sc.shutdown();
            }
            Ok(())
        })
}

fn init_tracing() {
    let env_filter = tracing_subscriber::EnvFilter::try_from_env("FERREX_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let log_file = std::env::var("FERREX_LOG_FILE").ok().map(|p| {
        let path = PathBuf::from(p);
        let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        let name = path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("ferrex.log"));
        tracing_appender::rolling::never(dir, name)
    });

    let log_format = std::env::var("FERREX_LOG_FORMAT").unwrap_or_default();
    let is_json = log_format.eq_ignore_ascii_case("json");

    match (log_file, is_json) {
        (Some(file), true) => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(file)
                .json()
                .init();
        }
        (Some(file), false) => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(file)
                .with_ansi(false)
                .init();
        }
        (None, true) => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(std::io::stderr)
                .json()
                .init();
        }
        (None, false) => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(std::io::stderr)
                .init();
        }
    }
}
