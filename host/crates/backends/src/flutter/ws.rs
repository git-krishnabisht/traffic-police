//! A minimal WebSocket client (RFC 6455), enough for the Dart VM service's JSON-RPC: text
//! messages (fragments joined), client frames masked, pings answered, a redirect reported
//! instead of followed (the VM service answers 302 when DDS owns it).

use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use bytes::{Buf, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The most one message may hold. A body fetched from the VM service arrives as a JSON array of
/// byte values in one message, about four bytes of JSON per byte of body.
pub const MAX_MESSAGE: usize = 512 << 20;

pub const OP_CONTINUATION: u8 = 0x0;
pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
pub const OP_PONG: u8 = 0xA;

/// The outcome of an opening handshake.
pub enum Connected {
    Open(WebSocket),
    /// The server sent the client elsewhere (`Location`).
    Redirect(String),
}

pub struct WebSocket {
    stream: TcpStream,
    buf: BytesMut,
    /// A text message being joined from fragments.
    partial: Vec<u8>,
    mask_state: u64,
}

/// One frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub fin: bool,
    pub opcode: u8,
    pub payload: Vec<u8>,
}

fn seed() -> u64 {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    (t ^ (std::process::id() as u64).rotate_left(32)) | 1
}

fn next(state: &mut u64) -> u64 {
    // xorshift64*: masks need not be secret here (no proxies between us and the device)
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// Opens `ws://host:port/path`. A response other than 101 ends in an error, except a redirect.
pub async fn connect(host: &str, port: u16, path: &str) -> io::Result<Connected> {
    let mut stream = TcpStream::connect((host, port)).await?;
    let _ = stream.set_nodelay(true);
    let mut state = seed();
    let key_bytes: Vec<u8> = (0..2).flat_map(|_| next(&mut state).to_le_bytes()).collect();
    let key = base64::engine::general_purpose::STANDARD.encode(key_bytes);
    // the VM service checks Host: loopback names pass (and the forwarded port is any port)
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    let mut buf = BytesMut::with_capacity(16 * 1024);
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 64 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "the handshake response is too long"));
        }
        if stream.read_buf(&mut buf).await? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the connection closed during the handshake"));
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    buf.advance(head_end);
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let header = |name: &str| {
        head.split("\r\n")
            .skip(1)
            .filter_map(|l| l.split_once(':'))
            .find(|(n, _)| n.trim().eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_string())
    };
    match status {
        101 => Ok(Connected::Open(WebSocket { stream, buf, partial: Vec::new(), mask_state: state })),
        301 | 302 | 303 | 307 | 308 => match header("location") {
            Some(l) => Ok(Connected::Redirect(l)),
            None => Err(io::Error::other(format!("{status_line} without a Location"))),
        },
        _ => Err(io::Error::other(format!("the WebSocket handshake was refused: {status_line}"))),
    }
}

/// A frame as bytes: masked with `mask` (a client's frames must be), or not (a server's).
pub fn encode(opcode: u8, payload: &[u8], mask: Option<[u8; 4]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | opcode);
    let m = if mask.is_some() { 0x80 } else { 0 };
    match payload.len() {
        n if n < 126 => out.push(m | n as u8),
        n if n <= u16::MAX as usize => {
            out.push(m | 126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            out.push(m | 127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    match mask {
        Some(k) => {
            out.extend_from_slice(&k);
            out.extend(payload.iter().enumerate().map(|(i, b)| b ^ k[i % 4]));
        }
        None => out.extend_from_slice(payload),
    }
    out
}

/// One frame from the front of `buf`, when it holds a whole one (masked frames unmasked).
pub fn decode(buf: &mut BytesMut) -> io::Result<Option<Frame>> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let (b0, b1) = (buf[0], buf[1]);
    let masked = b1 & 0x80 != 0;
    let (len, mut at) = match b1 & 0x7f {
        126 => {
            if buf.len() < 4 {
                return Ok(None);
            }
            (u16::from_be_bytes([buf[2], buf[3]]) as u64, 4)
        }
        127 => {
            if buf.len() < 10 {
                return Ok(None);
            }
            (u64::from_be_bytes(buf[2..10].try_into().expect("8 bytes")), 10)
        }
        n => (n as u64, 2),
    };
    if len > MAX_MESSAGE as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("a {len}-byte WebSocket frame")));
    }
    let key = if masked {
        if buf.len() < at + 4 {
            return Ok(None);
        }
        let k = [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]];
        at += 4;
        Some(k)
    } else {
        None
    };
    let len = len as usize;
    if buf.len() < at + len {
        return Ok(None);
    }
    buf.advance(at);
    let mut payload = buf.split_to(len).to_vec();
    if let Some(k) = key {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= k[i % 4];
        }
    }
    Ok(Some(Frame { fin: b0 & 0x80 != 0, opcode: b0 & 0x0f, payload }))
}

impl WebSocket {
    pub async fn send_text(&mut self, text: &str) -> io::Result<()> {
        self.send(OP_TEXT, text.as_bytes()).await
    }

    async fn send(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mask = (next(&mut self.mask_state) as u32).to_le_bytes();
        self.stream.write_all(&encode(opcode, payload, Some(mask))).await
    }

    /// The next text message; `None` once the server closed. Cancel-safe: what has arrived stays
    /// buffered for the next call.
    pub async fn recv_text(&mut self) -> io::Result<Option<String>> {
        loop {
            while let Some(f) = decode(&mut self.buf)? {
                match f.opcode {
                    OP_TEXT | OP_BINARY | OP_CONTINUATION => {
                        if self.partial.len() + f.payload.len() > MAX_MESSAGE {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, "a WebSocket message over 512 MiB"));
                        }
                        self.partial.extend_from_slice(&f.payload);
                        if f.fin {
                            let m = std::mem::take(&mut self.partial);
                            return String::from_utf8(m).map(Some).map_err(|_| {
                                io::Error::new(io::ErrorKind::InvalidData, "a message that is not UTF-8")
                            });
                        }
                    }
                    OP_PING => self.send(OP_PONG, &f.payload).await?,
                    OP_PONG => {}
                    OP_CLOSE => {
                        let _ = self.send(OP_CLOSE, &f.payload).await;
                        return Ok(None);
                    }
                    other => {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("WebSocket opcode {other}")));
                    }
                }
            }
            if self.stream.read_buf(&mut self.buf).await? == 0 {
                return Ok(None);
            }
        }
    }

    /// Says goodbye (best effort).
    pub async fn close(&mut self) {
        let _ = self.send(OP_CLOSE, &1000u16.to_be_bytes()).await;
        let _ = self.stream.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_masked_or_not_at_every_length_size() {
        for len in [0usize, 5, 125, 126, 1000, 65_535, 65_536, 200_000] {
            let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            for mask in [None, Some([1, 2, 3, 4])] {
                let mut buf = BytesMut::from(&encode(OP_TEXT, &payload, mask)[..]);
                // nothing until the whole frame is there
                let mut half = BytesMut::from(&buf[..buf.len() / 2]);
                if len > 0 {
                    assert_eq!(decode(&mut half).unwrap(), None);
                }
                let f = decode(&mut buf).unwrap().unwrap();
                assert_eq!((f.fin, f.opcode, f.payload.len()), (true, OP_TEXT, len));
                assert_eq!(f.payload, payload);
                assert!(buf.is_empty());
            }
        }
    }
}
