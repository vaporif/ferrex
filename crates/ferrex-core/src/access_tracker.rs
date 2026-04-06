use std::collections::HashSet;
use std::sync::Mutex;

const DEFAULT_FLUSH_THRESHOLD: usize = 50;

pub struct AccessTracker {
    buffer: Mutex<Vec<String>>,
    flush_threshold: usize,
}

impl AccessTracker {
    pub const fn new() -> Self {
        Self {
            buffer: Mutex::new(Vec::new()),
            flush_threshold: DEFAULT_FLUSH_THRESHOLD,
        }
    }

    pub fn record(&self, ids: &[String]) {
        let mut buf = self.buffer.lock().expect("poisoned");
        buf.extend(ids.iter().cloned());
    }

    /// Drain if buffer has reached the flush threshold. Single lock acquisition.
    pub fn drain_if_full(&self) -> Option<Vec<String>> {
        let mut buf = self.buffer.lock().expect("poisoned");
        if buf.len() < self.flush_threshold {
            return None;
        }
        Some(dedup_drain(&mut buf))
    }

    /// Drain unconditionally (for shutdown / background timer).
    pub fn drain(&self) -> Vec<String> {
        let mut buf = self.buffer.lock().expect("poisoned");
        dedup_drain(&mut buf)
    }
}

fn dedup_drain(buf: &mut Vec<String>) -> Vec<String> {
    let mut seen = HashSet::with_capacity(buf.len());
    buf.drain(..).filter(|id| seen.insert(id.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_deduplicates() {
        let tracker = AccessTracker::new();
        tracker.record(&["a".into(), "b".into(), "a".into(), "c".into(), "b".into()]);
        let drained = tracker.drain();
        assert_eq!(drained, vec!["a", "b", "c"]);
    }

    #[test]
    fn drain_empties_buffer() {
        let tracker = AccessTracker::new();
        tracker.record(&["x".into()]);
        let _ = tracker.drain();
        assert!(tracker.drain().is_empty());
    }

    #[test]
    fn drain_if_full_at_threshold() {
        let tracker = AccessTracker {
            buffer: Mutex::new(Vec::new()),
            flush_threshold: 3,
        };
        tracker.record(&["a".into(), "b".into()]);
        assert!(tracker.drain_if_full().is_none());
        tracker.record(&["c".into()]);
        assert!(tracker.drain_if_full().is_some());
    }
}
