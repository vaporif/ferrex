mod audit;
mod backfill;
mod cli;
mod commands;
#[allow(dead_code)]
mod daemon;
mod hint;
mod params;
mod server;
mod standalone;

use std::io::IsTerminal;
use std::path::PathBuf;

use clap::Parser;

use cli::{AuditCommand, BackfillCommand, Command, JournalCommand};

fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    ferrex_core::ensure_cache_dir();
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

        Some(Command::Nuke { force }) => commands::nuke(&cli::build_config(cli)?, force),

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
        Some(Command::DaemonPaths) => commands::daemon_paths(&cli::build_config(cli)?),
        Some(Command::Daemon { key }) => daemon::run(cli::build_config(cli)?, &key),
        None => standalone::run(cli::build_config(cli)?),
    }
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
                .with_ansi(std::io::stderr().is_terminal())
                .init();
        }
    }
}
