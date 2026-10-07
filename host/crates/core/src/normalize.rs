//! Protocol frames from one device connection -> [`SessionEvent`]s.
//!
//! The handshake (`hello` / `hello_ack`) is the backend's job; it hands every later frame to a
//! [`Normalizer`], which drops duplicates by `seq` (PROTOCOL.md §6, resume) and returns
//! connection-control messages to the caller.

use traffic_police_proto::Frame;
use traffic_police_proto::msg::{self, DeviceMsg};

use crate::event::{self, SessionEvent};
use crate::model::{SourceId, TxnKey};

/// Connection-level messages the backend must handle itself.
#[derive(Debug, Clone, PartialEq)]
pub enum Control {
    Hello(Box<msg::Hello>),
    Replay(msg::Replay),
    RulesAck(msg::RulesAck),
    ConfigAck(msg::ConfigAck),
    Pong(msg::Pong),
    Bye(msg::Bye),
}

#[derive(Debug, thiserror::Error)]
pub enum NormalizeError {
    #[error("undecodable JSON message: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0} out of range")]
    OutOfRange(&'static str),
}

#[derive(Debug)]
pub struct Normalizer {
    source: SourceId,
    last_seq: u64,
    duplicates: u64,
}

impl Normalizer {
    pub fn new(source: SourceId) -> Self {
        Normalizer { source, last_seq: 0, duplicates: 0 }
    }

    /// Resume after reconnecting to the same instance: skip everything up to `seq`.
    pub fn resume_after(source: SourceId, seq: u64) -> Self {
        Normalizer { source, last_seq: seq, duplicates: 0 }
    }

    pub fn source(&self) -> SourceId {
        self.source
    }

    /// Highest event sequence number applied (send as `resume_after_seq` on reconnect).
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Events skipped because their `seq` was already applied.
    pub fn duplicates(&self) -> u64 {
        self.duplicates
    }

    fn fresh(&mut self, seq: u64) -> bool {
        if seq <= self.last_seq {
            self.duplicates += 1;
            false
        } else {
            self.last_seq = seq;
            true
        }
    }

    fn key(&self, txn: u64) -> TxnKey {
        TxnKey { source: self.source, txn }
    }

    /// A `done` or `fail` that carries trailers or a gRPC status: those first.
    fn trailers(
        &self,
        txn: u64,
        at: u64,
        trailers: msg::Headers,
        grpc: Option<msg::GrpcStatus>,
        out: &mut Vec<SessionEvent>,
    ) {
        if trailers.is_empty() && grpc.is_none() {
            return;
        }
        let grpc = grpc.map(|g| crate::model::Grpc { code: g.code, name: g.status, message: g.message });
        out.push(SessionEvent::Trailers { key: self.key(txn), at, trailers, grpc });
    }

    /// Feed one frame. Events are appended to `out`; control messages are returned.
    pub fn frame(&mut self, frame: Frame, out: &mut Vec<SessionEvent>) -> Result<Option<Control>, NormalizeError> {
        match frame {
            Frame::Json(json) => {
                let msg = msg::parse_device(&json)?;
                Ok(self.message(msg, out))
            }
            Frame::Body(c) => {
                // a chunk past any device time or body size comes only from a broken stream
                if c.ts > msg::MAX_DEVICE_TS || c.offset.checked_add(c.data.len() as u64).is_none() {
                    return Err(NormalizeError::OutOfRange("body chunk"));
                }
                if self.fresh(c.seq) {
                    out.push(SessionEvent::Body {
                        key: self.key(c.txn),
                        dir: c.dir,
                        at: c.ts,
                        offset: c.offset,
                        bytes: c.data,
                    });
                }
                Ok(None)
            }
            Frame::Other { .. } => Ok(None),
        }
    }

    /// Feed one already-parsed message.
    pub fn message(&mut self, msg: DeviceMsg, out: &mut Vec<SessionEvent>) -> Option<Control> {
        if let Some(seq) = msg.seq()
            && !self.fresh(seq)
        {
            return None;
        }
        let source = self.source;
        match msg {
            DeviceMsg::Req(m) => out.push(SessionEvent::Request(Box::new(event::RequestStarted {
                key: self.key(m.txn),
                at: m.ts,
                call: m.call,
                hop: m.hop,
                method: m.method,
                url: m.url,
                headers: m.headers,
                client: m.client,
                thread: m.thread,
                stack: m.stack,
                stack_truncated: m.stack_truncated,
                body: m.body,
                marks: m.marks,
                conn: m.conn,
            }))),
            DeviceMsg::Resp(m) => out.push(SessionEvent::Response(Box::new(event::ResponseStarted {
                key: self.key(m.txn),
                at: m.ts,
                status: m.status,
                message: m.message,
                protocol: m.protocol,
                headers: m.headers,
                conn: m.conn,
            }))),
            DeviceMsg::BodyEnd(m) => out.push(SessionEvent::BodyEnd {
                key: self.key(m.txn),
                dir: m.dir,
                at: m.ts,
                total: m.bytes,
                captured: m.captured,
                state: m.state,
            }),
            DeviceMsg::Prog(m) => {
                out.push(SessionEvent::BodyProgress { key: self.key(m.txn), dir: m.dir, at: m.ts, total: m.bytes })
            }
            DeviceMsg::Mark(m) => out.push(SessionEvent::Mark { key: self.key(m.txn), at: m.ts, name: m.m }),
            DeviceMsg::Done(m) => {
                self.trailers(m.txn, m.ts, m.trailers, m.grpc, out);
                out.push(SessionEvent::Completed { key: self.key(m.txn), at: m.ts })
            }
            DeviceMsg::Fail(m) => {
                self.trailers(m.txn, m.ts, m.trailers, m.grpc, out);
                out.push(SessionEvent::Failed(Box::new(event::Failed {
                    key: self.key(m.txn),
                    at: m.ts,
                    phase: m.phase,
                    canceled: m.canceled,
                    simulated: m.simulated,
                    error: m.error,
                    conn: m.conn,
                })))
            }
            DeviceMsg::Rule(m) => out.push(SessionEvent::RuleApplied(Box::new(event::RuleApplied {
                key: self.key(m.txn),
                at: m.ts,
                rules: m.rules,
                changes: m.changes,
                delivered: m.delivered,
            }))),
            DeviceMsg::Ws(m) => {
                let data = match (m.text, m.base64) {
                    (Some(t), _) => bytes::Bytes::from(t.into_bytes()),
                    (None, Some(b)) => {
                        use base64::Engine;
                        base64::engine::general_purpose::STANDARD
                            .decode(b.trim())
                            .map(bytes::Bytes::from)
                            .unwrap_or_default()
                    }
                    (None, None) => bytes::Bytes::new(),
                };
                out.push(SessionEvent::WsMessage {
                    key: self.key(m.txn),
                    msg: crate::model::WsMessage {
                        at: m.ts,
                        out: m.dir == "out",
                        op: m.op,
                        size: m.size,
                        data,
                        truncated: m.truncated,
                        code: m.code,
                        reason: m.reason,
                    },
                })
            }
            DeviceMsg::Dropped(m) => {
                out.push(SessionEvent::Dropped { source, at: m.ts, events: m.events, bytes: m.bytes, txns: m.txns })
            }
            DeviceMsg::Traffic(m) => {
                out.push(SessionEvent::Traffic { source, at: m.ts, since: m.since, rx: m.rx, tx: m.tx })
            }
            DeviceMsg::Diag(m) => out.push(SessionEvent::Diagnostic {
                source,
                at: m.ts,
                level: m.level,
                code: m.code,
                message: m.message,
            }),
            DeviceMsg::Hello(h) => return Some(Control::Hello(Box::new(h))),
            DeviceMsg::Replay(r) => return Some(Control::Replay(r)),
            DeviceMsg::RulesAck(a) => {
                out.push(SessionEvent::RulesAck { source, ack: a.clone() });
                return Some(Control::RulesAck(a));
            }
            DeviceMsg::ConfigAck(a) => return Some(Control::ConfigAck(a)),
            DeviceMsg::Pong(p) => {
                out.push(SessionEvent::Clock { source, ts: p.clock.ts, wall_ms: p.clock.wall_ms });
                return Some(Control::Pong(p));
            }
            DeviceMsg::Bye(b) => return Some(Control::Bye(b)),
            DeviceMsg::Unknown => {}
        }
        None
    }
}

impl crate::model::SourceInfo {
    /// Build source info from a device's `hello` plus what the host knows about the device.
    pub fn from_hello(id: SourceId, h: &msg::Hello, device_label: String, serial: Option<String>) -> Self {
        crate::model::SourceInfo {
            id,
            device_label,
            serial,
            package: h.app.package.clone(),
            process: h.app.process.clone(),
            pid: h.app.pid,
            instance: h.instance.clone(),
            mode: h.runtime.mode.clone(),
            api: Some(h.device.api),
            runtime_version: Some(h.runtime.version.clone()),
            capabilities: h.capabilities.clone(),
            hooks: h.hooks.clone(),
            okhttp_version: h.clients.get("okhttp").cloned().flatten(),
            clock: Some((h.clock.ts, h.clock.wall_ms)),
            started: h.started_ts.unwrap_or(h.clock.ts).min(h.clock.ts),
            ended: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use traffic_police_proto::{BodyChunk, BodyDir};

    #[test]
    fn duplicates_by_seq_are_dropped() {
        let mut n = Normalizer::new(3);
        let mut out = Vec::new();
        let done = |seq| Frame::Json(Bytes::from(format!(r#"{{"t":"done","seq":{seq},"ts":1,"txn":9}}"#)));
        n.frame(done(1), &mut out).unwrap();
        n.frame(done(2), &mut out).unwrap();
        n.frame(done(2), &mut out).unwrap();
        n.frame(done(1), &mut out).unwrap();
        let chunk =
            BodyChunk { seq: 3, txn: 9, dir: BodyDir::Response, ts: 5, offset: 0, data: Bytes::from_static(b"x") };
        n.frame(Frame::Body(chunk.clone()), &mut out).unwrap();
        n.frame(Frame::Body(chunk), &mut out).unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(n.duplicates(), 3);
        assert_eq!(n.last_seq(), 3);
        assert!(matches!(out[0], SessionEvent::Completed { key: TxnKey { source: 3, txn: 9 }, at: 1 }));
    }

    #[test]
    fn control_messages_are_returned() {
        let mut n = Normalizer::resume_after(1, 10);
        let mut out = Vec::new();
        let c = n
            .frame(
                Frame::Json(Bytes::from_static(br#"{"t":"pong","id":4,"clock":{"ts":100,"wall_ms":2000}}"#)),
                &mut out,
            )
            .unwrap();
        assert!(matches!(c, Some(Control::Pong(_))));
        assert_eq!(out, vec![SessionEvent::Clock { source: 1, ts: 100, wall_ms: 2000 }]);
        assert!(n.frame(Frame::Json(Bytes::from_static(b"not json")), &mut out).is_err());
    }
}
