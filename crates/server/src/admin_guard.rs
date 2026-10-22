use std::fmt::Display;

use ferrex_core::FerrexConfig;

use crate::daemon::addr;

pub fn refuse_if_live(config: &FerrexConfig) -> eyre::Result<()> {
    if ferrex_store::is_memory_path(&config.db_path) {
        return Ok(());
    }
    refuse(
        addr::live_for_db(&addr::run_dir()?, &config.db_path),
        config.db_path.display(),
    )
}

pub fn refuse_if_any_live() -> eyre::Result<()> {
    refuse(
        addr::any_live(&addr::run_dir()?),
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

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn refuse_passes_without_live_daemon() {
        assert!(refuse(None, "db").is_ok());
    }

    #[test]
    fn refuse_names_pid_and_target() {
        let err = refuse(Some(42), "/tmp/db").unwrap_err().to_string();
        assert!(err.contains("pid 42"), "{err}");
        assert!(err.contains("/tmp/db"), "{err}");
    }

    #[test]
    fn in_memory_db_is_never_refused() {
        let mut config = crate::daemon::addr::tests::base_config();
        config.db_path = ":memory:".into();
        assert!(refuse_if_live(&config).is_ok());
    }
}
