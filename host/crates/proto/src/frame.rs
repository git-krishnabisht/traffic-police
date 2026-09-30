//! Frame codec (PROTOCOL.md §3 framing, §5 body chunks).
//!
//! Every frame is `u32 BE length | u8 type | payload`, where `length` counts the type byte plus
//! the payload. The decoder never allocates the claimed length up front: it waits until the whole
//! frame is buffered, so a hostile length cannot make it reserve 16 MiB.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};

/// Largest allowed value of the length field (type byte + payload).
pub const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;
/// Largest body slice a device puts in one body chunk.
pub const MAX_CHUNK_BYTES: usize = 64 * 1024;
/// Size of the fixed body-chunk header that precedes the bytes.
pub const BODY_HEADER_LEN: usize = 34;

/// Frame type bytes.
pub mod kind {
    /// One UTF-8 JSON object.
    pub const JSON: u8 = 1;
    /// Binary body chunk.
    pub const BODY: u8 = 2;
    /// Session file header record.
    pub const SESSION: u8 = 16;
    /// Session file: start of (or switch to) a source.
    pub const SOURCE: u8 = 17;
    /// Session file: a source ended.
    pub const SOURCE_END: u8 = 18;
    /// Session file: host annotations (pins, markers).
    pub const ANNOTATIONS: u8 = 19;
}

/// Which body a chunk or `body_end` belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BodyDir {
    /// What the app wrote as the request body.
    Request,
    /// The response body as received from the network (still Content-Encoded for OkHttp).
    Response,
    /// The rewritten response body the app actually received (only when a rule changed it).
    Delivered,
}

impl BodyDir {
    pub const ALL: [BodyDir; 3] = [BodyDir::Request, BodyDir::Response, BodyDir::Delivered];

    pub fn to_wire(self) -> u8 {
        match self {
            BodyDir::Request => 0,
            BodyDir::Response => 1,
            BodyDir::Delivered => 2,
        }
    }

    pub fn from_wire(v: u8) -> Option<Self> {
        match v {
            0 => Some(BodyDir::Request),
            1 => Some(BodyDir::Response),
            2 => Some(BodyDir::Delivered),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            BodyDir::Request => "request",
            BodyDir::Response => "response",
            BodyDir::Delivered => "delivered",
        }
    }
}

/// A slice of a request or response body (frame type 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyChunk {
    pub seq: u64,
    pub txn: u64,
    pub dir: BodyDir,
    /// Device monotonic ns when the app wrote or read these bytes.
    pub ts: u64,
    /// Position of `data[0]` within the body.
    pub offset: u64,
    pub data: Bytes,
}

/// One decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Frame type 1: the raw JSON bytes (parse with [`crate::msg`]).
    Json(Bytes),
    /// Frame type 2.
    Body(BodyChunk),
    /// Any other type: session-file records, or types from a newer protocol (ignore them).
    Other { kind: u8, payload: Bytes },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("frame length {0} exceeds the {MAX_FRAME_LEN}-byte limit")]
    TooLarge(u32),
    #[error("frame length 0 (a frame needs at least its type byte)")]
    Empty,
    #[error("malformed body chunk: {0}")]
    BadBodyChunk(&'static str),
}

impl FrameError {
    /// A fatal error means the byte stream can no longer be trusted: close the connection.
    /// A non-fatal error affects one frame only; the decoder has already skipped it.
    pub fn is_fatal(&self) -> bool {
        matches!(self, FrameError::TooLarge(_) | FrameError::Empty)
    }
}

fn put_header(out: &mut BytesMut, kind: u8, payload_len: usize) {
    let len = u32::try_from(payload_len + 1).expect("frame payload larger than u32");
    assert!(len <= MAX_FRAME_LEN, "frame of {len} bytes exceeds MAX_FRAME_LEN");
    out.reserve(4 + len as usize);
    out.put_u32(len);
    out.put_u8(kind);
}

/// Append a frame of any type.
pub fn encode_raw(kind: u8, payload: &[u8], out: &mut BytesMut) {
    put_header(out, kind, payload.len());
    out.put_slice(payload);
}

/// Append a JSON frame. `json` must be one UTF-8 JSON object.
pub fn encode_json(json: &[u8], out: &mut BytesMut) {
    encode_raw(kind::JSON, json, out);
}

/// Append a body-chunk frame.
pub fn encode_body(chunk: &BodyChunk, out: &mut BytesMut) {
    put_header(out, kind::BODY, BODY_HEADER_LEN + chunk.data.len());
    out.put_u64(chunk.seq);
    out.put_u64(chunk.txn);
    out.put_u8(chunk.dir.to_wire());
    out.put_u8(0); // flags: reserved
    out.put_u64(chunk.ts);
    out.put_u64(chunk.offset);
    out.put_slice(&chunk.data);
}

/// Append any [`Frame`].
pub fn encode(frame: &Frame, out: &mut BytesMut) {
    match frame {
        Frame::Json(json) => encode_json(json, out),
        Frame::Body(chunk) => encode_body(chunk, out),
        Frame::Other { kind, payload } => encode_raw(*kind, payload, out),
    }
}

fn parse_body(mut p: Bytes) -> Result<BodyChunk, FrameError> {
    if p.len() < BODY_HEADER_LEN {
        return Err(FrameError::BadBodyChunk("shorter than the 34-byte header"));
    }
    let seq = p.get_u64();
    let txn = p.get_u64();
    let dir = BodyDir::from_wire(p.get_u8()).ok_or(FrameError::BadBodyChunk("unknown dir"))?;
    let _flags = p.get_u8();
    let ts = p.get_u64();
    let offset = p.get_u64();
    Ok(BodyChunk { seq, txn, dir, ts, offset, data: p })
}

/// Incremental frame decoder: feed bytes with [`Decoder::push`], take frames with
/// [`Decoder::next_frame`] until it returns `Ok(None)`.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: BytesMut,
    fatal: Option<FrameError>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, data: &[u8]) {
        if self.fatal.is_none() {
            self.buf.extend_from_slice(data);
        }
    }

    /// Bytes received but not yet returned as frames.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// The next complete frame, `Ok(None)` if more bytes are needed.
    ///
    /// After a fatal error every later call returns the same error.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        if let Some(err) = &self.fatal {
            return Err(err.clone());
        }
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
        if len == 0 {
            return Err(self.fail(FrameError::Empty));
        }
        if len > MAX_FRAME_LEN {
            return Err(self.fail(FrameError::TooLarge(len)));
        }
        if self.buf.len() < 4 + len as usize {
            return Ok(None);
        }
        self.buf.advance(4);
        let mut frame = self.buf.split_to(len as usize).freeze();
        let kind = frame.get_u8();
        Ok(Some(match kind {
            kind::JSON => Frame::Json(frame),
            kind::BODY => Frame::Body(parse_body(frame)?),
            other => Frame::Other { kind: other, payload: frame },
        }))
    }

    fn fail(&mut self, err: FrameError) -> FrameError {
        self.buf.clear();
        self.fatal = Some(err.clone());
        err
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn sample_chunk() -> BodyChunk {
        BodyChunk {
            seq: 42,
            txn: 7,
            dir: BodyDir::Response,
            ts: 5_822_011_000_000,
            offset: 65_536,
            data: Bytes::from_static(b"{\"ok\":true}"),
        }
    }

    #[test]
    fn round_trips_every_frame_kind_byte_by_byte() {
        let frames = vec![
            Frame::Json(Bytes::from_static(br#"{"t":"ping","id":1}"#)),
            Frame::Body(sample_chunk()),
            Frame::Other { kind: kind::ANNOTATIONS, payload: Bytes::from_static(b"{}") },
        ];
        let mut wire = BytesMut::new();
        for f in &frames {
            encode(f, &mut wire);
        }
        let mut dec = Decoder::new();
        let mut out = Vec::new();
        for b in wire.iter() {
            dec.push(std::slice::from_ref(b));
            while let Some(f) = dec.next_frame().unwrap() {
                out.push(f);
            }
        }
        assert_eq!(out, frames);
        assert_eq!(dec.buffered(), 0);
    }

    #[test]
    fn body_chunk_layout_matches_protocol() {
        let mut wire = BytesMut::new();
        encode_body(&sample_chunk(), &mut wire);
        // length = 1 (type) + 34 (header) + 11 (data)
        assert_eq!(&wire[0..4], &46u32.to_be_bytes());
        assert_eq!(wire[4], kind::BODY);
        assert_eq!(&wire[5..13], &42u64.to_be_bytes()); // seq
        assert_eq!(&wire[13..21], &7u64.to_be_bytes()); // txn
        assert_eq!(wire[21], 1); // dir = response
        assert_eq!(wire[22], 0); // flags
        assert_eq!(&wire[23..31], &5_822_011_000_000u64.to_be_bytes()); // ts
        assert_eq!(&wire[31..39], &65_536u64.to_be_bytes()); // offset
        assert_eq!(&wire[39..], b"{\"ok\":true}");
    }

    /// The largest legal frame, generated rather than stored (PROTOCOL.md §11), arriving in pieces.
    #[test]
    fn the_largest_allowed_frame_decodes() {
        let overhead = r#"{"t":"x","pad":""}"#.len();
        let json = format!(r#"{{"t":"x","pad":"{}"}}"#, "a".repeat(MAX_FRAME_LEN as usize - 1 - overhead));
        let mut wire = BytesMut::new();
        encode_json(json.as_bytes(), &mut wire);
        assert_eq!(wire.len(), 4 + MAX_FRAME_LEN as usize);
        let mut dec = Decoder::new();
        for piece in wire.chunks(1 << 20) {
            assert_eq!(dec.next_frame().unwrap(), None);
            dec.push(piece);
        }
        let Some(Frame::Json(got)) = dec.next_frame().unwrap() else { panic!("expected a JSON frame") };
        assert_eq!(got.len(), MAX_FRAME_LEN as usize - 1);
        assert_eq!(dec.buffered(), 0);
    }

    #[test]
    fn oversized_and_empty_frames_are_fatal() {
        let mut dec = Decoder::new();
        dec.push(&(MAX_FRAME_LEN + 1).to_be_bytes());
        let err = dec.next_frame().unwrap_err();
        assert_eq!(err, FrameError::TooLarge(MAX_FRAME_LEN + 1));
        assert!(err.is_fatal());
        // stays failed, and does not buffer more
        dec.push(&[0, 0, 0, 2, 1, b'{']);
        assert_eq!(dec.next_frame().unwrap_err(), FrameError::TooLarge(MAX_FRAME_LEN + 1));
        assert_eq!(dec.buffered(), 0);

        let mut dec = Decoder::new();
        dec.push(&[0, 0, 0, 0]);
        assert_eq!(dec.next_frame().unwrap_err(), FrameError::Empty);
    }

    #[test]
    fn bad_body_chunk_skips_one_frame_only() {
        let mut wire = BytesMut::new();
        encode_raw(kind::BODY, &[1, 2, 3], &mut wire); // too short
        let mut bad_dir = BytesMut::new();
        encode_body(&sample_chunk(), &mut bad_dir);
        bad_dir[21] = 9; // unknown dir
        wire.extend_from_slice(&bad_dir);
        encode_json(b"{}", &mut wire);

        let mut dec = Decoder::new();
        dec.push(&wire);
        let e1 = dec.next_frame().unwrap_err();
        assert!(!e1.is_fatal());
        let e2 = dec.next_frame().unwrap_err();
        assert_eq!(e2, FrameError::BadBodyChunk("unknown dir"));
        assert_eq!(dec.next_frame().unwrap(), Some(Frame::Json(Bytes::from_static(b"{}"))));
        assert_eq!(dec.next_frame().unwrap(), None);
    }

    #[test]
    fn waits_for_the_whole_frame_without_reserving_the_claimed_length() {
        let mut dec = Decoder::new();
        dec.push(&(MAX_FRAME_LEN).to_be_bytes());
        dec.push(&[kind::JSON]);
        assert_eq!(dec.next_frame().unwrap(), None);
        assert!(dec.buffered() < 64);
    }

    proptest! {
        #[test]
        fn arbitrary_bytes_never_panic(chunks in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..64), 0..32)) {
            let mut dec = Decoder::new();
            for c in chunks {
                dec.push(&c);
                loop {
                    match dec.next_frame() {
                        Ok(Some(_)) => continue,
                        Ok(None) => break,
                        Err(e) if e.is_fatal() => break,
                        Err(_) => continue,
                    }
                }
            }
        }

        #[test]
        fn encoded_chunks_always_decode(seq: u64, txn: u64, d in 0u8..3, ts: u64, offset: u64, data in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let chunk = BodyChunk { seq, txn, dir: BodyDir::from_wire(d).unwrap(), ts, offset, data: Bytes::from(data) };
            let mut wire = BytesMut::new();
            encode_body(&chunk, &mut wire);
            let mut dec = Decoder::new();
            dec.push(&wire);
            prop_assert_eq!(dec.next_frame().unwrap(), Some(Frame::Body(chunk)));
        }
    }
}
