use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

/// Bytes of daemon output queued for stdout before the proxy stops reading the socket.
const STDOUT_BUDGET: usize = 4 * 1024 * 1024;
/// How long the pump must wait for budget before stdout counts as blocked.
const STALL_GRACE: Duration = Duration::from_secs(1);

/// Meters daemon lines onto stdout and holds the one the pump has read but not yet queued.
#[derive(Debug)]
pub struct StdoutBudget {
    permits: Arc<Semaphore>,
    held: Mutex<Option<Vec<u8>>>,
    park: Mutex<Park>,
    parked: Notify,
}

/// `seq` tells a park apart from a later one that starts at the same instant.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Park {
    seq: u64,
    since: Option<Instant>,
}

impl Default for StdoutBudget {
    fn default() -> Self {
        Self {
            permits: Arc::new(Semaphore::new(STDOUT_BUDGET)),
            held: Mutex::default(),
            park: Mutex::default(),
            parked: Notify::new(),
        }
    }
}

impl StdoutBudget {
    /// Holds `line` until its share of the budget is free; `None` if the budget was closed.
    pub async fn hold_until_free(&self, line: Vec<u8>) -> Option<OwnedSemaphorePermit> {
        // A line over the whole budget takes all of it, so it still gets through alone.
        let cost = u32::try_from(line.len().min(STDOUT_BUDGET)).unwrap_or(u32::MAX);
        *self.held.lock().unwrap_or_else(PoisonError::into_inner) = Some(line);
        if let Ok(permit) = Arc::clone(&self.permits).try_acquire_many_owned(cost) {
            return Some(permit);
        }
        let _parked = Parked::start(self);
        self.parked.notify_waiters();
        Arc::clone(&self.permits)
            .acquire_many_owned(cost)
            .await
            .ok()
    }

    #[must_use]
    pub fn take_held(&self) -> Option<Vec<u8>> {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Resolves once one daemon line has waited `STALL_GRACE` for budget, i.e. stdout is blocked.
    pub async fn stalled(&self) {
        loop {
            // Created before the check, so a park in between isn't missed.
            let notified = self.parked.notified();
            let park = *self.park();
            let Some(since) = park.since else {
                notified.await;
                continue;
            };
            tokio::time::sleep_until(since + STALL_GRACE).await;
            if *self.park() == park {
                return;
            }
        }
    }

    fn park(&self) -> MutexGuard<'_, Park> {
        self.park.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Ends the park on drop, so an aborted pump doesn't leave the budget looking stalled.
struct Parked<'a>(&'a StdoutBudget);

impl<'a> Parked<'a> {
    fn start(budget: &'a StdoutBudget) -> Self {
        let mut park = budget.park();
        park.seq += 1;
        park.since = Some(Instant::now());
        drop(park);
        Self(budget)
    }
}

impl Drop for Parked<'_> {
    fn drop(&mut self) {
        self.0.park().since = None;
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Takes the whole budget and parks a pump behind it.
    async fn parked_pump(
        budget: &Arc<StdoutBudget>,
    ) -> (OwnedSemaphorePermit, tokio::task::JoinHandle<bool>) {
        let all = budget.hold_until_free(vec![0; STDOUT_BUDGET]).await;
        let _ = budget.take_held();
        let pump = tokio::spawn({
            let budget = Arc::clone(budget);
            async move { budget.hold_until_free(vec![0; 10]).await.is_some() }
        });
        tokio::task::yield_now().await;
        assert!(budget.park().since.is_some(), "pump didn't park");
        (all.unwrap_or_else(|| panic!("budget closed")), pump)
    }

    async fn not_stalled_within(budget: &StdoutBudget, within: Duration) -> bool {
        tokio::time::timeout(within, budget.stalled())
            .await
            .is_err()
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_parked_is_not_stalled() {
        let budget = StdoutBudget::default();
        assert!(not_stalled_within(&budget, Duration::from_secs(10)).await);
    }

    #[tokio::test(start_paused = true)]
    async fn park_shorter_than_grace_is_not_stalled() {
        let budget = Arc::new(StdoutBudget::default());
        let (all, pump) = parked_pump(&budget).await;
        let drain = tokio::spawn({
            let budget = Arc::clone(&budget);
            async move { not_stalled_within(&budget, Duration::from_secs(10)).await }
        });
        tokio::time::sleep(STALL_GRACE / 2).await;
        drop(all);
        assert!(pump.await.unwrap_or(false));
        assert!(
            drain.await.unwrap_or(false),
            "a brief park counted as a stall"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn park_longer_than_grace_is_stalled() {
        let budget = Arc::new(StdoutBudget::default());
        let (_all, _pump) = parked_pump(&budget).await;
        let start = Instant::now();
        budget.stalled().await;
        assert!(start.elapsed() >= STALL_GRACE);
    }

    #[tokio::test(start_paused = true)]
    async fn freed_budget_resets_the_grace() {
        let budget = Arc::new(StdoutBudget::default());
        let (all, pump) = parked_pump(&budget).await;
        tokio::time::sleep(STALL_GRACE * 3 / 4).await;
        drop(all);
        assert!(pump.await.unwrap_or(false));
        let (_all, _pump) = parked_pump(&budget).await;
        assert!(not_stalled_within(&budget, STALL_GRACE * 3 / 4).await);
        let restarted = Instant::now();
        budget.stalled().await;
        assert!(restarted.elapsed() >= STALL_GRACE / 4);
    }

    #[tokio::test]
    async fn aborted_pump_ends_the_park() {
        let budget = Arc::new(StdoutBudget::default());
        let (_all, pump) = parked_pump(&budget).await;
        pump.abort();
        let _ = pump.await;
        assert_eq!(budget.park().since, None);
    }
}
