//! Normalized events: what every backend produces and the store applies (ARCHITECTURE.md §5.3).

use bytes::Bytes;
use traffic_police_proto::msg::{self, Change, Conn, DeliveredResponse, ErrorInfo, RuleRef};
use traffic_police_proto::{BodyDir, Headers};

use crate::fmt::Ts;
use crate::model::{SourceId, SourceInfo, TxnKey};

#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    /// A process segment starts.
    SourceUp(Box<SourceInfo>),
    /// A process segment ended (detach, crash, unplug, replaced, protocol error).
    SourceDown {
        source: SourceId,
        at: Ts,
        reason: String,
    },
    /// Fresh wall-clock pair for a source (from `pong`).
    Clock {
        source: SourceId,
        ts: Ts,
        wall_ms: i64,
    },
    Request(Box<RequestStarted>),
    Response(Box<ResponseStarted>),
    Body {
        key: TxnKey,
        dir: BodyDir,
        at: Ts,
        offset: u64,
        bytes: Bytes,
    },
    BodyProgress {
        key: TxnKey,
        dir: BodyDir,
        at: Ts,
        total: u64,
    },
    BodyEnd {
        key: TxnKey,
        dir: BodyDir,
        at: Ts,
        total: u64,
        captured: u64,
        state: String,
    },
    Mark {
        key: TxnKey,
        at: Ts,
        name: String,
    },
    Completed {
        key: TxnKey,
        at: Ts,
    },
    Failed(Box<Failed>),
    RuleApplied(Box<RuleApplied>),
    Traffic {
        source: SourceId,
        at: Ts,
        since: Option<Ts>,
        rx: u64,
        tx: u64,
    },
    Dropped {
        source: SourceId,
        at: Ts,
        events: u64,
        bytes: u64,
        txns: Vec<u64>,
    },
    Diagnostic {
        source: SourceId,
        at: Ts,
        level: String,
        code: String,
        message: String,
    },
    /// Host-side markers (pause, resume, notes).
    Marker {
        source: Option<SourceId>,
        at: Ts,
        kind: MarkerKind,
        label: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerKind {
    Attach,
    Detach,
    Reattach,
    Pause,
    Resume,
    Note,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RequestStarted {
    pub key: TxnKey,
    pub at: Ts,
    pub call: Option<u64>,
    pub hop: u32,
    pub method: String,
    pub url: String,
    pub headers: Headers,
    pub client: Option<msg::ClientInfo>,
    pub thread: Option<msg::ThreadInfo>,
    pub stack: Vec<msg::StackFrame>,
    pub stack_truncated: bool,
    pub body: Option<msg::ReqBodyInfo>,
    pub marks: Vec<(String, Ts)>,
    pub conn: Option<Conn>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResponseStarted {
    pub key: TxnKey,
    pub at: Ts,
    pub status: u16,
    pub message: String,
    pub protocol: Option<String>,
    pub headers: Headers,
    pub conn: Option<Conn>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Failed {
    pub key: TxnKey,
    pub at: Ts,
    pub phase: Option<String>,
    pub canceled: bool,
    pub simulated: bool,
    pub error: ErrorInfo,
    pub conn: Option<Conn>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuleApplied {
    pub key: TxnKey,
    pub at: Ts,
    pub rules: Vec<RuleRef>,
    pub changes: Vec<Change>,
    pub delivered: Option<DeliveredResponse>,
}
