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
