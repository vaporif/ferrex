#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnEvent {
    HelloPassed,
    HelloFailed,
    Ended,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IdleState {
    pub init_ok: bool,
    pub in_hello: usize,
    pub active: usize,
}

impl IdleState {
    #[must_use]
    pub const fn should_arm(&self) -> bool {
        self.init_ok && self.in_hello == 0 && self.active == 0
    }

    pub const fn accepted(&mut self) {
        self.in_hello += 1;
    }

    pub fn apply(&mut self, event: ConnEvent) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
