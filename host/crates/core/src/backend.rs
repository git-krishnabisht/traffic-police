//! What a UI or headless command can ask of a running backend (ARCHITECTURE.md §5.3).
//!
//! Backends receive these over a channel and send batches of [`crate::SessionEvent`]s back.

use traffic_police_proto::msg::RuleSet;

#[derive(Debug, Clone, PartialEq)]
pub enum BackendCommand {
    /// Start or stop creating new transactions on the device (pause/resume).
    SetRecording(bool),
    /// Replace the device's rule set.
    SetRules(RuleSet),
    /// Ask for a fresh clock pair and liveness check.
    Ping,
    /// Stop and release the connection.
    Shutdown,
}

/// What a live backend says about its connection, for the header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    /// Looking for the device or the app; the text says what is missing.
    Waiting(String),
    /// Streaming; the text names the device and process.
    Live(String),
    /// The session ended; data stays.
    Detached(String),
    /// Cannot continue (protocol mismatch).
    Failed(String),
}

impl ConnectionStatus {
    pub fn text(&self) -> &str {
        match self {
            ConnectionStatus::Waiting(s)
            | ConnectionStatus::Live(s)
            | ConnectionStatus::Detached(s)
            | ConnectionStatus::Failed(s) => s,
        }
    }
}

/// What a backend can do, for greying out UI actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    pub pause: bool,
    pub rules: bool,
    pub live: bool,
}
