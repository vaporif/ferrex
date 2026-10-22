use std::collections::VecDeque;

use chrono::Utc;
use serde::Serialize;

use crate::types::OpRecord;

const OPS_BUFFER_CAPACITY: usize = 100;

pub struct OpsBuffer {
    ops: std::sync::Mutex<VecDeque<OpRecord>>,
}

impl OpsBuffer {
    pub(crate) fn new() -> Self {
        Self {
            ops: std::sync::Mutex::new(VecDeque::with_capacity(OPS_BUFFER_CAPACITY)),
        }
    }

    const MAX_DETAIL_LEN: usize = 120;

    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn record(
        &self,
        kind: &str,
        detail: &str,
        elapsed: std::time::Duration,
        outcome: &str,
    ) {
        let duration_ms = elapsed.as_millis() as u64;
        let detail = if detail.len() > Self::MAX_DETAIL_LEN {
            &detail[..detail.floor_char_boundary(Self::MAX_DETAIL_LEN)]
        } else {
            detail
        };
        let mut ops = self.ops.lock().expect("ops buffer poisoned");
        if ops.len() == OPS_BUFFER_CAPACITY {
            ops.pop_front();
        }
        ops.push_back(OpRecord {
            kind: kind.to_string(),
            detail: detail.to_string(),
            duration_ms,
            outcome: outcome.to_string(),
            timestamp: Utc::now(),
        });
    }

    pub(crate) fn recent(&self, n: usize) -> Vec<OpRecord> {
        let ops = self.ops.lock().expect("ops buffer poisoned");
        ops.iter().rev().take(n).cloned().collect()
    }
}

#[derive(Debug, Default, Serialize)]
pub struct AuditReport {
    pub sqlite_only: Vec<String>,
    pub qdrant_only: Vec<String>,
    pub fixed: Vec<String>,
}

#[derive(Debug, Default)]
pub struct BackfillReport {
    pub scanned: u64,
    pub updated: u64,
}

#[derive(Debug, Default)]
pub struct RecoveryReport {
    pub compensated_stores: u64,
    pub rolled_forward_forgets: u64,
    pub cleared_pre_qdrant: u64,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use pretty_assertions::assert_eq;
    use std::time::Duration;

    use super::*;

    #[test]
    fn recent_returns_newest_first() {
        let buf = OpsBuffer::new();
        buf.record("store", "a", Duration::from_millis(3), "ok");
        buf.record("recall", "b", Duration::from_millis(7), "err");
        let ops = buf.recent(10);
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].kind, "recall");
        assert_eq!(ops[0].duration_ms, 7);
        assert_eq!(ops[0].outcome, "err");
        assert_eq!(ops[1].detail, "a");
        assert_eq!(buf.recent(1).len(), 1);
    }

    #[test]
    fn evicts_oldest_at_capacity() {
        let buf = OpsBuffer::new();
        for i in 0..=OPS_BUFFER_CAPACITY {
            buf.record("store", &i.to_string(), Duration::ZERO, "ok");
        }
        let ops = buf.recent(usize::MAX);
        assert_eq!(ops.len(), OPS_BUFFER_CAPACITY);
        assert_eq!(ops[0].detail, OPS_BUFFER_CAPACITY.to_string());
        assert_eq!(ops.last().unwrap().detail, "1");
    }

    #[test]
    fn truncates_detail_on_char_boundary() {
        let buf = OpsBuffer::new();
        let detail = "é".repeat(OpsBuffer::MAX_DETAIL_LEN);
        buf.record("store", &detail, Duration::ZERO, "ok");
        let stored = &buf.recent(1)[0].detail;
        assert!(stored.len() <= OpsBuffer::MAX_DETAIL_LEN);
        assert_eq!(stored.len(), OpsBuffer::MAX_DETAIL_LEN);
        assert!(stored.chars().all(|c| c == 'é'));
    }
}
