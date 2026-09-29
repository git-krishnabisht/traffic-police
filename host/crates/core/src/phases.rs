//! Timing phases derived from marks (PROTOCOL.md §9). Missing marks give `None`, never guesses.

use crate::fmt::Ts;
use crate::model::Transaction;

pub type Span = (Ts, Ts);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Phases {
    pub queued: Option<Span>,
    pub dns: Option<Span>,
    /// TCP connect including TLS (as OkHttp reports it).
    pub connect: Option<Span>,
    pub tls: Option<Span>,
    pub send: Option<Span>,
    pub wait: Option<Span>,
    pub receive: Option<Span>,
}

/// The three shades of a timeline bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segments {
    pub start: Ts,
    /// Request fully written.
    pub sent: Ts,
    /// First response byte.
    pub first_byte: Option<Ts>,
    pub end: Ts,
}

fn first(t: &Transaction, name: &str) -> Option<Ts> {
    t.marks.iter().find(|(n, _)| n == name).map(|&(_, ts)| ts)
}

fn last(t: &Transaction, name: &str) -> Option<Ts> {
    t.marks.iter().rev().find(|(n, _)| n == name).map(|&(_, ts)| ts)
}

fn span(a: Option<Ts>, b: Option<Ts>) -> Option<Span> {
    match (a, b) {
        (Some(a), Some(b)) if b >= a => Some((a, b)),
        _ => None,
    }
}

impl Transaction {
    /// When the request was fully written.
    pub fn sent_at(&self) -> Ts {
        last(self, "req_body_end")
            .or_else(|| last(self, "req_headers_end"))
            .or(self.req_body.ended)
            .unwrap_or(self.req_at)
            .max(self.start)
    }

    /// First response byte.
    pub fn first_byte_at(&self) -> Option<Ts> {
        first(self, "resp_headers_start").or(self.resp.as_ref().map(|r| r.at))
    }

    pub fn segments(&self, now: Ts) -> Segments {
        let end = self.end.or(self.resp_body.ended).unwrap_or(now).max(self.start);
        let sent = self.sent_at().min(end);
        let first_byte = self.first_byte_at().map(|f| f.clamp(sent, end));
        Segments { start: self.start, sent, first_byte, end }
    }

    pub fn phases(&self, now: Ts) -> Phases {
        let seg = self.segments(now);
        let setup_start = [first(self, "dns_start"), first(self, "connect_start"), first(self, "conn_acquired")]
            .into_iter()
            .flatten()
            .min();
        let queued = first(self, "call_start").and_then(|c| span(Some(c), Some(setup_start.unwrap_or(self.req_at))));
        let send_start = first(self, "req_headers_start").unwrap_or(self.req_at);
        Phases {
            queued: queued.filter(|(a, b)| b > a),
            dns: span(first(self, "dns_start"), last(self, "dns_end")),
            connect: span(first(self, "connect_start"), last(self, "connect_end")),
            tls: span(first(self, "tls_start"), last(self, "tls_end")),
            send: span(Some(send_start), Some(seg.sent)),
            wait: seg.first_byte.and_then(|f| span(Some(seg.sent), Some(f))),
            receive: seg.first_byte.and_then(|f| {
                span(Some(f), Some(last(self, "resp_body_end").or(self.resp_body.ended).unwrap_or(seg.end)))
            }),
        }
    }
}
