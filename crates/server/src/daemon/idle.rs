use std::time::Duration;

use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnEvent {
    HelloPassed,
    HelloFailed,
    Ended,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct IdleState {
    init_ok: bool,
    in_hello: usize,
    active: usize,
}

impl IdleState {
    const fn should_arm(&self) -> bool {
        self.init_ok && self.in_hello == 0 && self.active == 0
    }

    const fn accepted(&mut self) {
        self.in_hello += 1;
    }

    fn apply(&mut self, event: ConnEvent) {
        match event {
            ConnEvent::HelloPassed => {
                self.in_hello = decrement(self.in_hello, "in_hello");
                self.active += 1;
            }
            ConnEvent::HelloFailed => self.in_hello = decrement(self.in_hello, "in_hello"),
            ConnEvent::Ended => self.active = decrement(self.active, "active"),
        }
    }
}

fn decrement(count: usize, name: &str) -> usize {
    count.checked_sub(1).unwrap_or_else(|| {
        debug_assert!(false, "{name} count went below zero");
        tracing::error!(counter = name, "connection count went below zero");
        0
    })
}

/// The idle-exit deadline, armed exactly when init has succeeded and no connection is open.
/// A connection that never passes its hello (an admin-guard probe) doesn't extend it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleTimer {
    state: IdleState,
    timeout: Duration,
    deadline: Option<Instant>,
    /// The deadline an accept disarmed; restored on re-arming unless a hello passed meanwhile.
    suspended: Option<Instant>,
}

impl IdleTimer {
    #[must_use]
    pub const fn new(timeout: Duration) -> Self {
        Self {
            state: IdleState {
                init_ok: false,
                in_hello: 0,
                active: 0,
            },
            timeout,
            deadline: None,
            suspended: None,
        }
    }

    #[must_use]
    pub const fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    #[must_use]
    pub const fn should_arm(&self) -> bool {
        self.state.should_arm()
    }

    pub const fn init_succeeded(&mut self) {
        self.state.init_ok = true;
    }

    pub const fn accepted(&mut self) {
        if let Some(deadline) = self.deadline.take() {
            self.suspended = Some(deadline);
        }
        self.state.accepted();
    }

    pub fn apply(&mut self, event: ConnEvent) {
        if event == ConnEvent::HelloPassed {
            self.suspended = None;
        }
        self.state.apply(event);
    }

    /// Arms or disarms the deadline to match the counts.
    pub fn update(&mut self, now: Instant) {
        match (self.state.should_arm(), self.deadline) {
            (true, None) => {
                self.deadline = Some(self.suspended.take().unwrap_or(now + self.timeout));
            }
            (false, Some(_)) => self.deadline = None,
            _ => {}
        }
    }

    /// Starts a fresh countdown, for an idle exit that couldn't go ahead.
    pub fn postpone(&mut self, now: Instant) {
        self.deadline = Some(now + self.timeout);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(10);

    #[test]
    fn arms_only_when_init_ok_and_no_connections() {
        let state = |init_ok, in_hello, active| IdleState {
            init_ok,
            in_hello,
            active,
        };
        assert!(state(true, 0, 0).should_arm());
        assert!(!state(false, 0, 0).should_arm());
        assert!(!state(true, 1, 0).should_arm());
        assert!(!state(true, 0, 1).should_arm());
    }

    #[test]
    fn hello_failed_is_terminal() {
        let mut state = IdleState {
            init_ok: true,
            ..IdleState::default()
        };
        state.accepted();
        state.apply(ConnEvent::HelloFailed);
        assert_eq!(state.in_hello, 0);
        assert_eq!(state.active, 0);
        assert!(state.should_arm());
    }

    #[test]
    fn passed_then_ended_returns_to_idle() {
        let mut state = IdleState {
            init_ok: true,
            ..IdleState::default()
        };
        state.accepted();
        state.apply(ConnEvent::HelloPassed);
        assert_eq!((state.in_hello, state.active), (0, 1));
        assert!(!state.should_arm());
        state.apply(ConnEvent::Ended);
        assert_eq!((state.in_hello, state.active), (0, 0));
        assert!(state.should_arm());
    }

    /// A timer armed at `t0` after init.
    fn armed(t0: Instant) -> IdleTimer {
        let mut timer = IdleTimer::new(TIMEOUT);
        timer.update(t0);
        assert_eq!(timer.deadline(), None, "armed before init");
        timer.init_succeeded();
        timer.update(t0);
        assert_eq!(timer.deadline(), Some(t0 + TIMEOUT));
        timer
    }

    fn probe(timer: &mut IdleTimer, at: Instant) {
        timer.accepted();
        timer.update(at);
        assert_eq!(
            timer.deadline(),
            None,
            "armed with a connection in its hello"
        );
        timer.apply(ConnEvent::HelloFailed);
        timer.update(at);
    }

    #[test]
    fn failed_hello_restores_the_deadline() {
        let t0 = Instant::now();
        let mut timer = armed(t0);
        probe(&mut timer, t0 + Duration::from_secs(4));
        assert_eq!(timer.deadline(), Some(t0 + TIMEOUT));
        probe(&mut timer, t0 + Duration::from_secs(8));
        assert_eq!(timer.deadline(), Some(t0 + TIMEOUT));
    }

    #[test]
    fn failed_hello_past_the_deadline_fires_at_once() {
        let t0 = Instant::now();
        let mut timer = armed(t0);
        let late = t0 + TIMEOUT + Duration::from_secs(5);
        probe(&mut timer, late);
        assert!(timer.deadline().is_some_and(|d| d <= late));
    }

    #[test]
    fn overlapping_probes_restore_the_first_deadline() {
        let t0 = Instant::now();
        let mut timer = armed(t0);
        let t1 = t0 + Duration::from_secs(3);
        timer.accepted();
        timer.update(t1);
        timer.accepted();
        timer.apply(ConnEvent::HelloFailed);
        timer.update(t1);
        assert_eq!(timer.deadline(), None);
        timer.apply(ConnEvent::HelloFailed);
        timer.update(t1);
        assert_eq!(timer.deadline(), Some(t0 + TIMEOUT));
    }

    #[test]
    fn session_ending_starts_a_fresh_countdown() {
        let t0 = Instant::now();
        let mut timer = armed(t0);
        timer.accepted();
        timer.update(t0);
        timer.apply(ConnEvent::HelloPassed);
        let t1 = t0 + Duration::from_secs(30);
        timer.apply(ConnEvent::Ended);
        timer.update(t1);
        assert_eq!(timer.deadline(), Some(t1 + TIMEOUT));
    }

    #[test]
    fn session_passing_alongside_a_probe_starts_a_fresh_countdown() {
        let t0 = Instant::now();
        let mut timer = armed(t0);
        timer.accepted();
        timer.update(t0);
        timer.accepted();
        timer.apply(ConnEvent::HelloPassed);
        timer.apply(ConnEvent::HelloFailed);
        let t1 = t0 + Duration::from_secs(30);
        timer.apply(ConnEvent::Ended);
        timer.update(t1);
        assert_eq!(timer.deadline(), Some(t1 + TIMEOUT));
    }

    #[test]
    fn probe_during_a_session_does_not_restore_anything() {
        let t0 = Instant::now();
        let mut timer = armed(t0);
        timer.accepted();
        timer.update(t0);
        timer.apply(ConnEvent::HelloPassed);
        let t1 = t0 + Duration::from_secs(30);
        probe(&mut timer, t1);
        assert_eq!(timer.deadline(), None);
        timer.apply(ConnEvent::Ended);
        timer.update(t1);
        assert_eq!(timer.deadline(), Some(t1 + TIMEOUT));
    }

    #[test]
    fn probe_before_init_leaves_a_fresh_countdown() {
        let t0 = Instant::now();
        let mut timer = IdleTimer::new(TIMEOUT);
        timer.accepted();
        timer.update(t0);
        timer.init_succeeded();
        let t1 = t0 + Duration::from_secs(30);
        timer.apply(ConnEvent::HelloFailed);
        timer.update(t1);
        assert_eq!(timer.deadline(), Some(t1 + TIMEOUT));
    }

    #[test]
    fn postpone_starts_a_fresh_countdown() {
        let t0 = Instant::now();
        let mut timer = armed(t0);
        let t1 = t0 + TIMEOUT;
        timer.postpone(t1);
        assert_eq!(timer.deadline(), Some(t1 + TIMEOUT));
    }
}
