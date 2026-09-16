// Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Protocol between Firecracker and an external *memory backend* process.
//!
//! A memory backend is a trusted process that receives the memfd backing the guest memory (and,
//! when restoring from a snapshot, the userfaultfd used to populate it) over a Unix domain
//! socket, and that is then responsible for extracting guest memory for snapshots: Firecracker
//! never writes guest memory to disk itself while a memory backend is connected.
//!
//! The connection is established by Firecracker (it connects to the backend's socket) with a
//! single [`MemBackendHandshake`] message carrying the file descriptors via `SCM_RIGHTS`. It then
//! stays open for the lifetime of the microVM and carries newline-delimited JSON messages:
//!
//! - the backend may send a [`MemBackendRequest`] at any time and gets a [`MemBackendResponse`];
//! - on `PUT /snapshot/create`, Firecracker sends a [`MemBackendNotification::SnapshotRequest`]
//!   and blocks until the backend answers with a [`MemBackendReply::SnapshotDone`].

use std::io::{self, Read, Write};
use std::os::fd::RawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde::{Deserialize, Serialize};
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

use crate::arch::host_page_size;
use crate::logger::{IncMetric, METRICS, error};
use crate::vmm_config::snapshot::SnapshotType;
use crate::vstate::memory::{GuestMemoryExtension, MemBackendRegion, MemRange, MemoryError};
use crate::vstate::vm::VmError;
use crate::{DirtyBitmap, Vmm};

/// Name of the guest memfd in [`MemBackendHandshake::fds`].
pub const FD_NAME_MEMFD: &str = "memfd";
/// Name of the userfaultfd in [`MemBackendHandshake::fds`].
pub const FD_NAME_UFFD: &str = "uffd";

/// First message sent by Firecracker on the connection, together with the file descriptors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemBackendHandshake {
    /// Names of the file descriptors attached to this message via `SCM_RIGHTS`, in order.
    /// `memfd` is always present; `uffd` is present when restoring from a snapshot.
    pub fds: Vec<String>,
    /// Host page size used for dirty tracking. Ranges are multiples of this.
    pub page_size: usize,
    /// Whether KVM dirty page tracking is enabled for this microVM.
    pub track_dirty_pages: bool,
    /// Total size of the guest memory, i.e. of the memfd and of a full snapshot file.
    pub total_size: u64,
    /// The guest memory regions.
    pub regions: Vec<MemBackendRegion>,
}

/// Requests the backend may send to Firecracker at any time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemBackendRequest {
    /// Ask for the pages dirtied since the last time dirty tracking was reset. Consuming: the
    /// dirty bitmaps are reset once the response has been sent.
    GetDirtyRanges {},
}

/// Kinds of errors Firecracker reports to the backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemBackendErrorKind {
    /// The message could not be parsed or is not a known request.
    UnknownRequest,
    /// A valid request could not be served. No state was changed.
    Internal,
}

/// Responses to [`MemBackendRequest`]s.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemBackendResponse {
    /// Response to [`MemBackendRequest::GetDirtyRanges`].
    DirtyRanges {
        /// Sorted, merged, page aligned ranges of the memory file that contain dirty pages.
        ranges: Vec<MemRange>,
    },
    /// The request could not be served.
    Error {
        /// Error category.
        kind: MemBackendErrorKind,
        /// Human readable details.
        message: String,
    },
}

/// Messages sent by Firecracker on its own initiative.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemBackendNotification {
    /// A snapshot has been requested through the API. The backend must copy `ranges` out of the
    /// memfd and answer with [`MemBackendReply::SnapshotDone`]. Firecracker processes nothing
    /// else until the reply arrives, so the guest memory is guaranteed not to change in between.
    SnapshotRequest {
        /// Full or Diff.
        snapshot_type: SnapshotType,
        /// The current regions, including their `plugged` state.
        regions: Vec<MemBackendRegion>,
        /// The ranges to copy: all plugged slots for `Full`, the dirty pages for `Diff`.
        ranges: Vec<MemRange>,
    },
}

/// Replies expected by Firecracker to a [`MemBackendNotification`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemBackendReply {
    /// The backend has finished (or failed) handling a `SnapshotRequest`.
    SnapshotDone {
        /// Whether the memory was copied successfully.
        success: bool,
        /// Details, surfaced to the API client on failure.
        message: String,
    },
}

/// Errors of the memory backend connection.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum MemBackendError {
    /// Cannot connect to the memory backend socket: {0}
    Connect(io::Error),
    /// Cannot send the handshake to the memory backend: {0}
    Handshake(vmm_sys_util::errno::Error),
    /// I/O error on the memory backend connection: {0}
    Io(#[from] io::Error),
    /// Cannot serialize a message for the memory backend: {0}
    Serialize(#[from] serde_json::Error),
    /// The memory backend closed the connection
    Disconnected,
    /// Unexpected message from the memory backend: {0}
    Protocol(String),
    /// The memory backend failed to take the snapshot: {0}
    SnapshotFailed(String),
    /// No memory backend is connected
    NotConnected,
}

/// A live connection to a memory backend.
#[derive(Debug)]
pub struct MemBackendConnection {
    stream: UnixStream,
    /// Bytes received but not yet consumed as a full line.
    buf: Vec<u8>,
}

impl MemBackendConnection {
    /// Maximum size of a single message received from the backend.
    const MAX_MESSAGE_LEN: usize = 64 * 1024;

    /// Connects to the backend listening at `path` and sends the handshake with `fds` attached.
    pub fn connect(
        path: &Path,
        handshake: &MemBackendHandshake,
        fds: &[RawFd],
    ) -> Result<Self, MemBackendError> {
        let stream = UnixStream::connect(path).map_err(MemBackendError::Connect)?;
        let mut msg = serde_json::to_vec(handshake)?;
        msg.push(b'\n');
        stream
            .send_with_fds(&[msg.as_slice()], fds)
            .map_err(MemBackendError::Handshake)?;
        Ok(Self {
            stream,
            buf: Vec::new(),
        })
    }

    /// The underlying socket, for registering it with an event loop.
    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }

    /// Sends one newline-delimited JSON message.
    pub fn send<T: Serialize>(&mut self, msg: &T) -> Result<(), MemBackendError> {
        let mut bytes = serde_json::to_vec(msg)?;
        bytes.push(b'\n');
        self.stream.write_all(&bytes)?;
        Ok(())
    }

    /// Performs a single `read()` on the socket, appending to the internal buffer.
    ///
    /// Returns `Ok(false)` if the peer closed the connection.
    pub fn fill(&mut self) -> Result<bool, MemBackendError> {
        let mut chunk = [0u8; 4096];
        let n = self.stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(false);
        }
        self.buf.extend_from_slice(&chunk[..n]);
        if self.buf.len() > Self::MAX_MESSAGE_LEN && !self.buf.contains(&b'\n') {
            return Err(MemBackendError::Protocol(
                "message from memory backend too long".to_string(),
            ));
        }
        Ok(true)
    }

    /// Removes and returns the next complete line from the buffer, without the newline.
    pub fn next_line(&mut self) -> Option<Vec<u8>> {
        let pos = self.buf.iter().position(|&b| b == b'\n')?;
        let mut line = self.buf.drain(..=pos).collect::<Vec<u8>>();
        line.pop();
        Some(line)
    }

    /// Blocks until a complete line has been received.
    pub fn recv_line(&mut self) -> Result<Vec<u8>, MemBackendError> {
        loop {
            if let Some(line) = self.next_line() {
                return Ok(line);
            }
            if !self.fill()? {
                return Err(MemBackendError::Disconnected);
            }
        }
    }

    /// Sends a `SnapshotRequest` and blocks until the backend replies with `SnapshotDone`.
    pub fn snapshot_request(
        &mut self,
        snapshot_type: SnapshotType,
        regions: Vec<MemBackendRegion>,
        ranges: Vec<MemRange>,
    ) -> Result<(), MemBackendError> {
        self.send(&MemBackendNotification::SnapshotRequest {
            snapshot_type,
            regions,
            ranges,
        })?;

        let line = self.recv_line()?;
        match serde_json::from_slice::<MemBackendReply>(&line) {
            Ok(MemBackendReply::SnapshotDone { success: true, .. }) => Ok(()),
            Ok(MemBackendReply::SnapshotDone {
                success: false,
                message,
            }) => Err(MemBackendError::SnapshotFailed(message)),
            Err(_) => Err(MemBackendError::Protocol(
                String::from_utf8_lossy(&line).into_owned(),
            )),
        }
    }
}

/// Errors of the memory-backend snapshot path.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum MemBackendSnapshotError {
    /// Snapshots require KVM
    NotKvm,
    /// Cannot get dirty bitmap: {0}
    DirtyBitmap(#[from] VmError),
    /// Cannot compute the dirty ranges: {0}
    Memory(#[from] MemoryError),
    /// Memory backend error: {0}
    Backend(#[from] MemBackendError),
}

impl Vmm {
    /// Computes the dirty ranges of the guest memory as the memory backend sees them, without
    /// resetting any bitmap. Returns the KVM dirty bitmap as well so the caller can either reset
    /// Firecracker's bitmap on success or fold the KVM bitmap back into it on failure.
    fn mem_backend_dirty_ranges(
        &mut self,
    ) -> Result<(Vec<MemRange>, DirtyBitmap), MemBackendSnapshotError> {
        let kvm_vm = self.vm.as_kvm().ok_or(MemBackendSnapshotError::NotKvm)?;
        // Devices keep running after this (the VM may not even be paused), so make sure that
        // whatever they write later is tracked again after the reset that follows.
        self.device_manager.prepare_dirty_tracking_reset();
        self.device_manager
            .mark_virtio_queue_memory_dirty(kvm_vm.guest_memory());
        let dirty_bitmap = kvm_vm.get_dirty_bitmap()?;
        let ranges = kvm_vm.guest_memory().dirty_ranges(&dirty_bitmap)?;
        Ok((ranges, dirty_bitmap))
    }

    /// Hands the guest memory over to the memory backend for a snapshot of `snapshot_type` and
    /// blocks until the backend reports completion.
    ///
    /// Nothing else runs on the VMM thread in the meantime, so guest memory cannot change while
    /// the backend copies it: vCPUs are paused (a precondition of snapshotting) and device
    /// emulation happens on this thread. This is the same guarantee Firecracker's own memory dump
    /// has today. On success, dirty tracking is reset, as after a Firecracker-made snapshot.
    pub(crate) fn snapshot_memory_to_backend(
        &mut self,
        snapshot_type: SnapshotType,
    ) -> Result<(), MemBackendSnapshotError> {
        if self.mem_backend.is_none() {
            return Err(MemBackendError::NotConnected.into());
        }

        let (ranges, dirty_bitmap) = match snapshot_type {
            SnapshotType::Diff => {
                let (ranges, bitmap) = self.mem_backend_dirty_ranges()?;
                (ranges, Some(bitmap))
            }
            SnapshotType::Full => {
                let kvm_vm = self.vm.as_kvm().ok_or(MemBackendSnapshotError::NotKvm)?;
                (kvm_vm.guest_memory().plugged_ranges(), None)
            }
        };

        let kvm_vm = self.vm.as_kvm().ok_or(MemBackendSnapshotError::NotKvm)?;
        let guest_memory = kvm_vm.guest_memory();
        let regions = guest_memory.describe_for_backend();

        METRICS.mem_backend.snapshot_requests.inc();
        let result = self
            .mem_backend
            .as_mut()
            .expect("checked above")
            .snapshot_request(snapshot_type, regions, ranges);
        if result.is_err() {
            METRICS.mem_backend.snapshot_fails.inc();
        }

        match (&result, dirty_bitmap) {
            // Same bookkeeping as `KvmVm::snapshot_memory_to_file`: a successful snapshot of
            // either type resets dirty tracking, a failed diff keeps the dirty pages.
            (Ok(()), _) => {
                kvm_vm.reset_dirty_bitmap();
                guest_memory.reset_dirty();
            }
            (Err(_), Some(bitmap)) => {
                guest_memory.store_dirty_bitmap(&bitmap, host_page_size());
            }
            (Err(_), None) => {}
        }

        if let Err(MemBackendError::Disconnected) = &result {
            self.mem_backend_disconnected();
        }

        result.map_err(MemBackendSnapshotError::Backend)
    }

    /// Serves a `GetDirtyRanges` request: replies with the dirty ranges and resets dirty
    /// tracking once the reply is out. If the reply cannot be sent, the dirty pages are kept.
    fn serve_get_dirty_ranges(&mut self) -> Result<(), MemBackendError> {
        let (response, bitmap) = match self.mem_backend_dirty_ranges() {
            Ok((ranges, bitmap)) => (MemBackendResponse::DirtyRanges { ranges }, Some(bitmap)),
            Err(err) => {
                METRICS.mem_backend.request_fails.inc();
                (
                    MemBackendResponse::Error {
                        kind: MemBackendErrorKind::Internal,
                        message: err.to_string(),
                    },
                    None,
                )
            }
        };

        let sent = self
            .mem_backend
            .as_mut()
            .ok_or(MemBackendError::NotConnected)?
            .send(&response);

        if let Some(bitmap) = bitmap
            && let Some(kvm_vm) = self.vm.as_kvm()
        {
            if sent.is_ok() {
                kvm_vm.reset_dirty_bitmap();
                kvm_vm.guest_memory().reset_dirty();
            } else {
                kvm_vm
                    .guest_memory()
                    .store_dirty_bitmap(&bitmap, host_page_size());
            }
        }
        sent
    }

    /// Handles one request line received from the memory backend.
    fn serve_mem_backend_request(&mut self, line: &[u8]) -> Result<(), MemBackendError> {
        METRICS.mem_backend.requests.inc();
        match serde_json::from_slice::<MemBackendRequest>(line) {
            Ok(MemBackendRequest::GetDirtyRanges {}) => self.serve_get_dirty_ranges(),
            Err(err) => {
                METRICS.mem_backend.request_fails.inc();
                self.mem_backend
                    .as_mut()
                    .ok_or(MemBackendError::NotConnected)?
                    .send(&MemBackendResponse::Error {
                        kind: MemBackendErrorKind::UnknownRequest,
                        message: err.to_string(),
                    })
            }
        }
    }

    /// Called when the memory backend socket is readable: reads what is available and serves
    /// every complete request. Returns `false` if the backend went away.
    pub(crate) fn process_mem_backend_event(&mut self) -> bool {
        let Some(conn) = self.mem_backend.as_mut() else {
            return false;
        };

        match conn.fill() {
            Ok(true) => {}
            Ok(false) => {
                self.mem_backend_disconnected();
                return false;
            }
            Err(err) => {
                error!("Error reading from the memory backend: {err}");
                self.mem_backend_disconnected();
                return false;
            }
        }

        while let Some(line) = self.mem_backend.as_mut().and_then(|c| c.next_line()) {
            if let Err(err) = self.serve_mem_backend_request(&line) {
                error!("Error serving memory backend request: {err}");
                self.mem_backend_disconnected();
                return false;
            }
        }
        true
    }

    fn mem_backend_disconnected(&mut self) {
        error!(
            "The memory backend disconnected; snapshots are no longer possible for this microVM"
        );
        METRICS.mem_backend.disconnects.inc();
        self.mem_backend = None;
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use vmm_sys_util::tempdir::TempDir;

    use super::*;

    #[test]
    fn test_message_formats() {
        let req: MemBackendRequest = serde_json::from_str(r#"{"GetDirtyRanges":{}}"#).unwrap();
        assert_eq!(req, MemBackendRequest::GetDirtyRanges {});

        let resp = MemBackendResponse::DirtyRanges {
            ranges: vec![MemRange {
                offset: 4096,
                len: 8192,
            }],
        };
        assert_eq!(
            serde_json::to_string(&resp).unwrap(),
            r#"{"DirtyRanges":{"ranges":[{"offset":4096,"len":8192}]}}"#
        );

        let reply: MemBackendReply =
            serde_json::from_str(r#"{"SnapshotDone":{"success":true,"message":""}}"#).unwrap();
        assert_eq!(
            reply,
            MemBackendReply::SnapshotDone {
                success: true,
                message: String::new()
            }
        );
    }

    #[test]
    fn test_connection_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.as_path().join("backend.sock");
        let listener = UnixListener::bind(&path).unwrap();

        let handshake = MemBackendHandshake {
            fds: vec![FD_NAME_MEMFD.to_string()],
            page_size: 4096,
            track_dirty_pages: true,
            total_size: 4096,
            regions: vec![],
        };
        let memfd = memfd::MemfdOptions::default().create("test").unwrap();
        let mut conn = MemBackendConnection::connect(
            &path,
            &handshake,
            &[std::os::fd::AsRawFd::as_raw_fd(memfd.as_file())],
        )
        .unwrap();

        let (backend, _) = listener.accept().unwrap();
        let mut buf = vec![0u8; 4096];
        let (n, file) = backend.recv_with_fd(&mut buf).unwrap();
        assert!(file.is_some());
        assert_eq!(buf[n - 1], b'\n');
        let received: MemBackendHandshake = serde_json::from_slice(&buf[..n - 1]).unwrap();
        assert_eq!(received, handshake);

        // Backend replies to a snapshot request, in two writes to exercise buffering.
        let handle = std::thread::spawn(move || {
            let mut backend = backend;
            let mut buf = vec![0u8; 4096];
            let n = backend.read(&mut buf).unwrap();
            let notification: MemBackendNotification =
                serde_json::from_slice(&buf[..n - 1]).unwrap();
            assert!(matches!(
                notification,
                MemBackendNotification::SnapshotRequest {
                    snapshot_type: SnapshotType::Full,
                    ..
                }
            ));
            let reply = serde_json::to_vec(&MemBackendReply::SnapshotDone {
                success: true,
                message: String::new(),
            })
            .unwrap();
            let (a, b) = reply.split_at(5);
            backend.write_all(a).unwrap();
            backend.write_all(b).unwrap();
            backend.write_all(b"\n").unwrap();
            // Then a failed one.
            let n = backend.read(&mut buf).unwrap();
            assert!(n > 0);
            let reply = serde_json::to_vec(&MemBackendReply::SnapshotDone {
                success: false,
                message: "disk full".to_string(),
            })
            .unwrap();
            backend.write_all(&reply).unwrap();
            backend.write_all(b"\n").unwrap();
        });

        conn.snapshot_request(SnapshotType::Full, vec![], vec![])
            .unwrap();
        let err = conn
            .snapshot_request(SnapshotType::Diff, vec![], vec![])
            .unwrap_err();
        assert!(matches!(err, MemBackendError::SnapshotFailed(m) if m == "disk full"));
        handle.join().unwrap();

        // Peer gone: recv_line reports a disconnect.
        assert!(matches!(
            conn.recv_line().unwrap_err(),
            MemBackendError::Disconnected
        ));
    }
}
