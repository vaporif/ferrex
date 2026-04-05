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
