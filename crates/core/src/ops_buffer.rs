use std::collections::VecDeque;

use chrono::Utc;
use serde::Serialize;

use crate::types::OpRecord;

const OPS_BUFFER_CAPACITY: usize = 100;

pub(crate) struct OpsBuffer {
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
