// Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The request/reply exchange with a `SharedMemfd` memory backend on its handshake connection.
//!
//! After the (unframed) handshake, every message on the connection is a frame:
//!
//! | offset         | size       | field                                     |
//! | :------------- | :--------- | :---------------------------------------- |
//! | 0              | 4          | `json_len`, `u32`, little-endian          |
//! | 4              | 4          | `blob_len`, `u32`, little-endian          |
//! | 8              | `json_len` | a JSON object, UTF-8                      |
//! | 8 + `json_len` | `blob_len` | binary payload, meaning given by the JSON |
//!
//! The backend sends `{"request": "DirtyPages"}` (no blob); Firecracker answers with the JSON
//! form of [`SnapshotMemoryLayout`] (`total_size`, `page_size`, `unplugged`) and the dirty page
//! bitmap as the blob, or `{"error": "..."}` with no blob. Firecracker never writes to the
//! connection unsolicited. See `docs/snapshotting/shared-memfd.md`.
//!
//! This module runs on the API thread: a request is turned into [`VmmAction::GetDirtyPages`]
//! and goes to the VMM thread over the same channel as HTTP requests, so requests from the
//! backend and from the HTTP API are serialised, and the paused VMM loop sees them alike.

use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

use serde::{Deserialize, Serialize};
use vmm::logger::{error_unrestricted, info_unrestricted, warn_unrestricted};
use vmm::rpc_interface::{VmmAction, VmmActionError, VmmData};
use vmm::vmm_config::snapshot::SnapshotMemoryLayout;

/// Size of a frame header.
pub const HEADER_LEN: usize = 8;
/// Largest JSON part Firecracker accepts in a frame from a backend.
pub const MAX_JSON_LEN: usize = 64 * 1024;

/// Errors after which the connection cannot be resynchronised and is closed.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum ProtocolError {
    /// Frame JSON part too large: {0} bytes (max 65536).
    JsonTooLarge(u32),
    /// Unexpected binary payload of {0} bytes in a request.
    UnexpectedBlob(u32),
    /// I/O error on the backend connection: {0}
    Io(io::Error),
    /// The backend closed the connection.
    Closed,
}

/// Encodes a frame header.
pub fn encode_header(json_len: usize, blob_len: usize) -> [u8; HEADER_LEN] {
    let mut header = [0u8; HEADER_LEN];
    header[..4].copy_from_slice(&u32::try_from(json_len).unwrap().to_le_bytes());
    header[4..].copy_from_slice(&u32::try_from(blob_len).unwrap().to_le_bytes());
    header
}

/// Encodes a complete frame (header, JSON, blob) into one buffer.
pub fn encode_frame(json: &[u8], blob: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(HEADER_LEN + json.len() + blob.len());
    frame.extend_from_slice(&encode_header(json.len(), blob.len()));
    frame.extend_from_slice(json);
    frame.extend_from_slice(blob);
    frame
}

/// A decoded frame: its JSON part and its binary payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The JSON object, as bytes.
    pub json: Vec<u8>,
    /// The binary payload (empty for every request defined today).
    pub blob: Vec<u8>,
}

/// Incremental decoder of frames sent by a backend. Bytes are fed as they arrive; complete
/// frames come out one by one.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    /// Appends bytes read from the connection.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Returns the next complete frame, if any. Validates the header (limits) before the payload
    /// is read: an invalid header is a [`ProtocolError`] and the stream position is lost.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, ProtocolError> {
        if self.buf.len() < HEADER_LEN {
            return Ok(None);
        }
        let json_len = u32::from_le_bytes(self.buf[..4].try_into().unwrap());
        let blob_len = u32::from_le_bytes(self.buf[4..8].try_into().unwrap());
        if json_len as usize > MAX_JSON_LEN {
            return Err(ProtocolError::JsonTooLarge(json_len));
        }
        if blob_len != 0 {
            return Err(ProtocolError::UnexpectedBlob(blob_len));
        }
        let total = HEADER_LEN + json_len as usize;
        if self.buf.len() < total {
            return Ok(None);
        }
        let rest = self.buf.split_off(total);
        let json = self.buf.split_off(HEADER_LEN);
        self.buf = rest;
        Ok(Some(Frame {
            json,
            blob: Vec::new(),
        }))
    }
}

/// The requests a backend can send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum RequestKind {
    /// Report (and consume) the pages dirtied since the last such request.
    DirtyPages,
}

/// A request frame's JSON. Unknown fields are rejected on purpose: a future field such as
/// `"consume": false` must fail on a Firecracker that does not know it rather than be ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackendRequest {
    /// What is being asked.
    pub request: RequestKind,
}

/// An error reply's JSON.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ErrorReply {
    /// Why the request could not be served.
    pub error: String,
}

/// Encodes the reply to a successful `DirtyPages` request: the layout's JSON as header, its
/// bitmap as blob.
pub fn encode_dirty_pages_reply(layout: &SnapshotMemoryLayout) -> Vec<u8> {
    let json = serde_json::to_vec(layout).expect("SnapshotMemoryLayout serialises");
    encode_frame(&json, &layout.pages)
}

/// Encodes an error reply.
pub fn encode_error_reply(message: &str) -> Vec<u8> {
    let json = serde_json::to_vec(&ErrorReply {
        error: message.to_string(),
    })
    .expect("ErrorReply serialises");
    encode_frame(&json, &[])
}

/// Outcome of servicing the connection once.
#[derive(Debug, PartialEq, Eq)]
pub enum ConnectionStatus {
    /// Keep polling the connection.
    Open,
    /// The connection is closed (by the peer, or by us after a protocol error); stop polling
    /// it and drop it.
    Closed,
}

/// The connection to a `SharedMemfd` memory backend, as serviced by the API thread.
#[derive(Debug)]
pub struct MemBackendConnection {
    stream: UnixStream,
    decoder: FrameDecoder,
}

impl MemBackendConnection {
    /// Wraps the handshake connection. The stream must be non-blocking.
    pub fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            decoder: FrameDecoder::default(),
        }
    }

    /// The fd to poll for readability.
    pub fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// Reads whatever the backend has sent and serves every complete request in it. `serve`
    /// forwards a [`VmmAction`] to the VMM thread and returns its outcome.
    pub fn process(
        &mut self,
        mut serve: impl FnMut(VmmAction) -> Result<VmmData, VmmActionError>,
    ) -> ConnectionStatus {
        match self.read_available() {
            Ok(()) => {}
            Err(ProtocolError::Closed) => {
                info_unrestricted!("Memory backend closed its connection.");
                return ConnectionStatus::Closed;
            }
            Err(err) => {
                error_unrestricted!("Memory backend connection: {err}; closing it.");
                return ConnectionStatus::Closed;
            }
        }

        loop {
            let Frame { json, .. } = match self.decoder.next_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => return ConnectionStatus::Open,
                Err(err) => {
                    error_unrestricted!("Memory backend sent a malformed frame: {err}; closing.");
                    return ConnectionStatus::Closed;
                }
            };
            let reply = match serde_json::from_slice::<BackendRequest>(&json) {
                Ok(BackendRequest {
                    request: RequestKind::DirtyPages,
                }) => match serve(VmmAction::GetDirtyPages) {
                    Ok(VmmData::SnapshotMemory(layout)) => encode_dirty_pages_reply(&layout),
                    Ok(other) => {
                        error_unrestricted!("Unexpected VMM answer to GetDirtyPages: {other:?}");
                        encode_error_reply("internal error")
                    }
                    Err(err) => {
                        warn_unrestricted!("DirtyPages request failed: {err}");
                        encode_error_reply(&err.to_string())
                    }
                },
                Err(err) => {
                    // Valid framing, unknown or malformed request: the stream position is intact,
                    // answer with an error and keep going.
                    warn_unrestricted!("Memory backend sent an invalid request: {err}");
                    encode_error_reply(&format!("invalid request: {err}"))
                }
            };
            if let Err(err) = self.write_reply(&reply) {
                error_unrestricted!("Failed to reply to memory backend: {err}; closing.");
                return ConnectionStatus::Closed;
            }
        }
    }

    fn read_available(&mut self) -> Result<(), ProtocolError> {
        let mut buf = [0u8; 4096];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => return Err(ProtocolError::Closed),
                Ok(n) => self.decoder.feed(&buf[..n]),
                Err(err) if err.kind() == ErrorKind::WouldBlock => return Ok(()),
                Err(err) if err.kind() == ErrorKind::Interrupted => {}
                Err(err) => return Err(ProtocolError::Io(err)),
            }
        }
    }

    /// Writes a reply in full. The stream is non-blocking, so a peer that does not read makes
    /// this spin (with a `poll` for writability in between) until it does or goes away; the
    /// peer asked for this reply and is expected to be reading it.
    fn write_reply(&mut self, mut reply: &[u8]) -> Result<(), ProtocolError> {
        while !reply.is_empty() {
            match self.stream.write(reply) {
                Ok(0) => return Err(ProtocolError::Closed),
                Ok(n) => reply = &reply[n..],
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    let mut pfd = libc::pollfd {
                        fd: self.stream.as_raw_fd(),
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    // SAFETY: `pfd` is a valid pollfd and we pass its count.
                    let ret = unsafe { libc::poll(&mut pfd, 1, -1) };
                    if ret < 0 {
                        let err = io::Error::last_os_error();
                        if err.kind() != ErrorKind::Interrupted {
                            return Err(ProtocolError::Io(err));
                        }
                    }
                }
                Err(err) if err.kind() == ErrorKind::Interrupted => {}
                Err(err) => return Err(ProtocolError::Io(err)),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use vmm::VmmError;
    use vmm::vmm_config::snapshot::MemoryRange;

    use super::*;

    fn request_frame(json: &str) -> Vec<u8> {
        encode_frame(json.as_bytes(), &[])
    }

    fn decode_frame(bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
        assert!(bytes.len() >= HEADER_LEN);
        let json_len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let blob_len = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        assert_eq!(bytes.len(), HEADER_LEN + json_len + blob_len);
        (
            bytes[HEADER_LEN..HEADER_LEN + json_len].to_vec(),
            bytes[HEADER_LEN + json_len..].to_vec(),
        )
    }

    fn example_layout() -> SnapshotMemoryLayout {
        // The example of the design document: pages 0, 1 and 12 set, pages 8..12 unplugged.
        let mut layout = SnapshotMemoryLayout::new(16 * 4096, 4096, 16 * 4096);
        layout.set_range(0, 2 * 4096);
        layout.set_range(12 * 4096, 4096);
        layout.unplugged.push(MemoryRange {
            offset: 8 * 4096,
            len: 4 * 4096,
        });
        layout
    }

    #[test]
    fn test_encode_request_and_reply() {
        let frame = request_frame(r#"{"request":"DirtyPages"}"#);
        assert_eq!(&frame[..HEADER_LEN], &[0x18, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&frame[HEADER_LEN..], br#"{"request":"DirtyPages"}"#);

        let reply = encode_dirty_pages_reply(&example_layout());
        assert_eq!(&reply[..HEADER_LEN], &[0x50, 0, 0, 0, 2, 0, 0, 0]);
        let (json, blob) = decode_frame(&reply);
        assert_eq!(
            json,
            br#"{"total_size":65536,"page_size":4096,"unplugged":[{"offset":32768,"len":16384}]}"#
        );
        assert_eq!(blob, vec![0x03, 0x10]);

        let (json, blob) = decode_frame(&encode_error_reply("boom"));
        assert_eq!(json, br#"{"error":"boom"}"#);
        assert!(blob.is_empty());

        // A 1 GiB guest: 32 KiB blob.
        let layout = SnapshotMemoryLayout::new(1 << 30, 4096, 1 << 30);
        let reply = encode_dirty_pages_reply(&layout);
        let (_, blob) = decode_frame(&reply);
        assert_eq!(blob.len(), 32 * 1024);
    }

    #[test]
    fn test_decoder_incremental() {
        let frame = request_frame(r#"{"request":"DirtyPages"}"#);
        let mut decoder = FrameDecoder::default();
        for byte in &frame[..frame.len() - 1] {
            decoder.feed(&[*byte]);
            assert!(decoder.next_frame().unwrap().is_none());
        }
        decoder.feed(&frame[frame.len() - 1..]);
        let decoded = decoder.next_frame().unwrap().unwrap();
        assert_eq!(decoded.json, br#"{"request":"DirtyPages"}"#);
        assert!(decoded.blob.is_empty());
        assert!(decoder.next_frame().unwrap().is_none());

        // Two frames in one read, plus a partial third.
        let mut two = frame.clone();
        two.extend_from_slice(&frame);
        two.extend_from_slice(&frame[..3]);
        decoder.feed(&two);
        assert!(decoder.next_frame().unwrap().is_some());
        assert!(decoder.next_frame().unwrap().is_some());
        assert!(decoder.next_frame().unwrap().is_none());
        decoder.feed(&frame[3..]);
        assert!(decoder.next_frame().unwrap().is_some());
    }

    #[test]
    fn test_decoder_limits() {
        let mut decoder = FrameDecoder::default();
        decoder.feed(&encode_header(MAX_JSON_LEN + 1, 0));
        assert!(matches!(
            decoder.next_frame(),
            Err(ProtocolError::JsonTooLarge(_))
        ));

        let mut decoder = FrameDecoder::default();
        decoder.feed(&encode_header(2, 1));
        assert!(matches!(
            decoder.next_frame(),
            Err(ProtocolError::UnexpectedBlob(1))
        ));

        // Exactly at the limit is fine (once the payload arrives).
        let mut decoder = FrameDecoder::default();
        decoder.feed(&encode_header(MAX_JSON_LEN, 0));
        assert!(decoder.next_frame().unwrap().is_none());
    }

    #[test]
    fn test_request_parsing() {
        let req: BackendRequest = serde_json::from_str(r#"{"request":"DirtyPages"}"#).unwrap();
        assert_eq!(req.request, RequestKind::DirtyPages);
        // Whitespace is legal.
        serde_json::from_str::<BackendRequest>(r#"{ "request" : "DirtyPages" }"#).unwrap();
        // Unknown fields and unknown requests are rejected.
        serde_json::from_str::<BackendRequest>(r#"{"request":"DirtyPages","consume":false}"#)
            .unwrap_err();
        serde_json::from_str::<BackendRequest>(r#"{"request":"Foo"}"#).unwrap_err();
        serde_json::from_str::<BackendRequest>(r#"{}"#).unwrap_err();
    }

    /// A connected, non-blocking pair: `(firecracker side, backend side)`.
    fn pair() -> (MemBackendConnection, UnixStream) {
        let (fc, backend) = UnixStream::pair().unwrap();
        fc.set_nonblocking(true).unwrap();
        (MemBackendConnection::new(fc), backend)
    }

    fn read_reply(backend: &mut UnixStream) -> (Vec<u8>, Vec<u8>) {
        let mut header = [0u8; HEADER_LEN];
        backend.read_exact(&mut header).unwrap();
        let json_len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        let blob_len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let mut json = vec![0u8; json_len];
        backend.read_exact(&mut json).unwrap();
        let mut blob = vec![0u8; blob_len];
        backend.read_exact(&mut blob).unwrap();
        (json, blob)
    }

    #[test]
    fn test_connection_serves_dirty_pages() {
        let (mut conn, mut backend) = pair();
        backend
            .write_all(&request_frame(r#"{"request":"DirtyPages"}"#))
            .unwrap();

        let mut actions = Vec::new();
        let status = conn.process(|action| {
            actions.push(action);
            Ok(VmmData::SnapshotMemory(example_layout()))
        });
        assert_eq!(status, ConnectionStatus::Open);
        assert_eq!(actions, vec![VmmAction::GetDirtyPages]);

        let (json, blob) = read_reply(&mut backend);
        let header: SnapshotMemoryLayout = serde_json::from_slice(&json).unwrap();
        assert_eq!(header.total_size, 65536);
        assert_eq!(header.page_size, 4096);
        assert_eq!(header.unplugged.len(), 1);
        assert_eq!(blob, vec![0x03, 0x10]);

        // Nothing pending: still open, nothing served.
        let status = conn.process(|_| panic!("no request pending"));
        assert_eq!(status, ConnectionStatus::Open);
    }

    #[test]
    fn test_connection_error_reply() {
        let (mut conn, mut backend) = pair();
        backend
            .write_all(&request_frame(r#"{"request":"DirtyPages"}"#))
            .unwrap();
        let status = conn.process(|_| Err(VmmActionError::InternalVmm(VmmError::NoMemBackend)));
        assert_eq!(status, ConnectionStatus::Open);
        let (json, blob) = read_reply(&mut backend);
        let reply: ErrorReply = serde_json::from_slice(&json).unwrap();
        assert!(!reply.error.is_empty());
        assert!(blob.is_empty());

        // An unknown request gets an error reply and the connection stays usable.
        backend
            .write_all(&request_frame(r#"{"request":"Frobnicate"}"#))
            .unwrap();
        let status = conn.process(|_| panic!("must not reach the VMM"));
        assert_eq!(status, ConnectionStatus::Open);
        let (json, _) = read_reply(&mut backend);
        let reply: ErrorReply = serde_json::from_slice(&json).unwrap();
        assert!(reply.error.contains("invalid request"));

        backend
            .write_all(&request_frame(r#"{"request":"DirtyPages"}"#))
            .unwrap();
        let status = conn.process(|_| Ok(VmmData::SnapshotMemory(example_layout())));
        assert_eq!(status, ConnectionStatus::Open);
        let (_, blob) = read_reply(&mut backend);
        assert_eq!(blob, vec![0x03, 0x10]);
    }

    #[test]
    fn test_connection_closes_on_protocol_error_or_hangup() {
        // Oversize header: closed, nothing forwarded.
        let (mut conn, mut backend) = pair();
        backend.write_all(&encode_header(1 << 20, 0)).unwrap();
        let status = conn.process(|_| panic!("must not reach the VMM"));
        assert_eq!(status, ConnectionStatus::Closed);
        drop(conn);
        // The backend sees EOF.
        let mut buf = [0u8; 1];
        assert_eq!(backend.read(&mut buf).unwrap(), 0);

        // A blob on a request: closed.
        let (mut conn, mut backend) = pair();
        backend.write_all(&encode_frame(b"{}", b"x")).unwrap();
        assert_eq!(
            conn.process(|_| panic!("must not reach the VMM")),
            ConnectionStatus::Closed
        );

        // Peer hangup: closed.
        let (mut conn, backend) = pair();
        drop(backend);
        assert_eq!(
            conn.process(|_| panic!("must not reach the VMM")),
            ConnectionStatus::Closed
        );

        // Garbage that is not JSON but a valid frame: error reply, not closed.
        let (mut conn, mut backend) = pair();
        backend.write_all(&request_frame("not json")).unwrap();
        assert_eq!(
            conn.process(|_| panic!("must not reach the VMM")),
            ConnectionStatus::Open
        );
        let (json, _) = read_reply(&mut backend);
        serde_json::from_slice::<ErrorReply>(&json).unwrap();
    }
}
