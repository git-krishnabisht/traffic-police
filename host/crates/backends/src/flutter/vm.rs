//! The Dart VM service's JSON-RPC 2.0 over [`super::ws`]: calls with string ids, and the
//! `streamNotify` events that arrive between replies, kept for the caller.

use std::collections::VecDeque;
use std::time::Duration;

use serde_json::{Value, json};

use super::ws::{Connected, WebSocket};

/// Why a call did not return a result.
#[derive(Debug)]
pub enum RpcError {
    /// The VM service closed the connection (the app exited, or DDS took over).
    Closed,
    Timeout,
    /// The JSON-RPC error object: code, message, details.
    Rpc(i64, String, Option<String>),
    Io(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::Closed => write!(f, "the VM service closed the connection"),
            RpcError::Timeout => write!(f, "the VM service did not answer in time"),
            RpcError::Rpc(code, m, Some(d)) => write!(f, "{m} ({code}): {d}"),
            RpcError::Rpc(code, m, None) => write!(f, "{m} ({code})"),
            RpcError::Io(e) => write!(f, "{e}"),
        }
    }
}

/// One connection to the VM service (or to DDS in front of it).
pub struct Vm {
    ws: WebSocket,
    next_id: u64,
    events: VecDeque<Value>,
}

/// Where a connection attempt went.
pub enum Open {
    Vm(Vm),
    /// DDS serves this VM: its address on the host.
    Redirect(String),
}

/// `http://127.0.0.1:43217/Wq9tyH3o9fo=/` (or ws://) as host, port and the WebSocket path.
pub fn ws_target(uri: &str) -> Option<(String, u16, String)> {
    let rest = uri.split_once("://").map_or(uri, |(_, r)| r);
    let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (h, p) = v6.split_once("]:")?;
        (h.to_string(), p)
    } else {
        let (h, p) = authority.rsplit_once(':')?;
        (h.to_string(), p)
    };
    let port: u16 = port.parse().ok()?;
    // the auth code's segment, without a `ws` that is there already
    let mut segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.last() == Some(&"ws") {
        segments.pop();
    }
    let prefix: String = segments.iter().map(|s| format!("/{s}")).collect();
    Some((host, port, format!("{prefix}/ws")))
}

impl Vm {
    /// Opens the VM service at `host:port` with its auth-code path (`/<token>/ws`).
    pub async fn open(host: &str, port: u16, path: &str) -> Result<Open, RpcError> {
        let attempt = tokio::time::timeout(Duration::from_secs(5), super::ws::connect(host, port, path)).await;
        match attempt {
            Err(_) => Err(RpcError::Timeout),
            Ok(Err(e)) => Err(RpcError::Io(e.to_string())),
            Ok(Ok(Connected::Redirect(l))) => Ok(Open::Redirect(l)),
            Ok(Ok(Connected::Open(ws))) => Ok(Open::Vm(Vm { ws, next_id: 1, events: VecDeque::new() })),
        }
    }

    /// Calls `method`; events that arrive meanwhile are kept (see [`Vm::take_events`]).
    pub async fn call(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, RpcError> {
        let id = format!("tp{}", self.next_id);
        self.next_id += 1;
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.ws.send_text(&request.to_string()).await.map_err(|e| RpcError::Io(e.to_string()))?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let text = match tokio::time::timeout_at(deadline, self.ws.recv_text()).await {
                Err(_) => return Err(RpcError::Timeout),
                Ok(Err(e)) => return Err(RpcError::Io(e.to_string())),
                Ok(Ok(None)) => return Err(RpcError::Closed),
                Ok(Ok(Some(t))) => t,
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
            if v.get("method").and_then(Value::as_str) == Some("streamNotify") {
                self.events.push_back(v);
                continue;
            }
            // a late reply to a call that timed out is dropped
            if v.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                continue;
            }
            if let Some(e) = v.get("error") {
                return Err(RpcError::Rpc(
                    e["code"].as_i64().unwrap_or(0),
                    e["message"].as_str().unwrap_or("error").to_string(),
                    e["data"]["details"].as_str().map(str::to_string),
                ));
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Waits up to `d` for an event; `Err(Closed)` when the connection ended. Cancel-safe.
    pub async fn next_event(&mut self, d: Duration) -> Result<Option<Value>, RpcError> {
        if let Some(e) = self.events.pop_front() {
            return Ok(Some(e));
        }
        match tokio::time::timeout(d, self.ws.recv_text()).await {
            Err(_) => Ok(None),
            Ok(Err(e)) => Err(RpcError::Io(e.to_string())),
            Ok(Ok(None)) => Err(RpcError::Closed),
            Ok(Ok(Some(t))) => match serde_json::from_str::<Value>(&t) {
                Ok(v) if v.get("method").and_then(Value::as_str) == Some("streamNotify") => Ok(Some(v)),
                _ => Ok(None),
            },
        }
    }

    /// Events that arrived during calls.
    pub fn take_events(&mut self) -> Vec<Value> {
        self.events.drain(..).collect()
    }

    pub async fn close(mut self) {
        self.ws.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_uris_become_websocket_targets() {
        assert_eq!(
            ws_target("http://127.0.0.1:43217/Wq9tyH3o9fo=/"),
            Some(("127.0.0.1".into(), 43217, "/Wq9tyH3o9fo=/ws".into()))
        );
        assert_eq!(ws_target("http://127.0.0.1:43217/"), Some(("127.0.0.1".into(), 43217, "/ws".into())));
        assert_eq!(ws_target("ws://127.0.0.1:61234/QwE=/ws"), Some(("127.0.0.1".into(), 61234, "/QwE=/ws".into())));
        assert_eq!(ws_target("http://[::1]:8181/abc=/"), Some(("::1".into(), 8181, "/abc=/ws".into())));
        assert_eq!(ws_target("http://127.0.0.1:1/abcws=/"), Some(("127.0.0.1".into(), 1, "/abcws=/ws".into())));
        assert_eq!(ws_target("http://127.0.0.1/x/"), None);
    }
}
