use std::fmt::Display;

use ferrex_core::FerrexConfig;

use crate::daemon::addr;

/// Refuses when a live daemon already serves `config.db_path`.
pub fn refuse_if_live(config: &FerrexConfig) -> eyre::Result<()> {
    if ferrex_store::is_memory_path(&config.db_path) {
        return Ok(());
    }
    refuse(
        addr::live_for_db(&addr::run_dir(), &config.db_path),
        config.db_path.display(),
    )
}

#[allow(dead_code, reason = "used by Task 14")]
pub fn refuse_if_any_live() -> eyre::Result<()> {
    refuse(
        addr::any_live(&addr::run_dir()),
        "the qdrant sidecar in ~/.ferrex",
    )
}

fn refuse(live_pid: Option<u32>, target: impl Display) -> eyre::Result<()> {
    live_pid.map_or(Ok(()), |pid| {
        Err(eyre::eyre!(
            "a ferrex daemon (pid {pid}) is using {target}. Close your Claude sessions or wait for the daemon to exit when idle (FERREX_IDLE_TIMEOUT), then retry."
        ))
    })
}
