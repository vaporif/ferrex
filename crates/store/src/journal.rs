use chrono::{DateTime, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingOp {
    pub op_id: String,
    pub kind: PendingOpKind,
    pub memory_id: String,
    pub namespace: String,
    pub target_id: Option<String>,
    pub qdrant_written: bool,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingOpKind {
    Store,
    Forget,
    Supersede,
}

#[derive(Debug, Clone)]
pub struct CompletedOp {
    pub op_id: String,
    pub kind: PendingOpKind,
    pub memory_id: String,
    pub namespace: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub duration_ms: i64,
    pub outcome: String,
}

impl PendingOp {
    pub fn into_completed(self, memory_id: String, namespace: String) -> CompletedOp {
        let completed_at = Utc::now();
        CompletedOp {
            op_id: self.op_id,
            kind: self.kind,
            memory_id,
            namespace,
            started_at: self.started_at,
            completed_at,
            duration_ms: (completed_at - self.started_at).num_milliseconds(),
            outcome: "ok".to_string(),
        }
    }
}

impl PendingOpKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Store => "store",
            Self::Forget => "forget",
            Self::Supersede => "supersede",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "store" => Some(Self::Store),
            "forget" => Some(Self::Forget),
            "supersede" => Some(Self::Supersede),
            _ => None,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn kind_round_trips_through_str() {
        for kind in [
            PendingOpKind::Store,
            PendingOpKind::Forget,
            PendingOpKind::Supersede,
        ] {
            assert_eq!(PendingOpKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(PendingOpKind::parse("bogus"), None);
    }

    #[test]
    fn into_completed_keeps_op_and_overrides_target() {
        let started_at = Utc::now() - chrono::Duration::milliseconds(50);
        let op = PendingOp {
            op_id: "op".into(),
            kind: PendingOpKind::Forget,
            memory_id: "old".into(),
            namespace: "old-ns".into(),
            target_id: None,
            qdrant_written: true,
            started_at,
        };
        let done = op.into_completed("mem".into(), "ns".into());
        assert_eq!(done.op_id, "op");
        assert_eq!(done.kind, PendingOpKind::Forget);
        assert_eq!(done.memory_id, "mem");
        assert_eq!(done.namespace, "ns");
        assert_eq!(done.started_at, started_at);
        assert!(done.duration_ms >= 50);
        assert_eq!(done.outcome, "ok");
    }
}
