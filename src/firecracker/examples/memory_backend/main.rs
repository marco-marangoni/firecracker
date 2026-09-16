// Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Reference implementation of a Firecracker *memory backend*.
//!
//! A memory backend is a process that receives the memfd backing a microVM's guest memory from
//! Firecracker (see `docs/snapshotting/memory-backend-design.md`) and that is responsible for
//! copying the guest memory out when a snapshot is created. When the microVM is restored from a
//! snapshot, it also receives a userfaultfd and serves page faults from a snapshot memory file,
//! exactly like the UFFD example handlers in `examples/uffd` do.
//!
//! Usage: `memory_backend <socket_path> <output_dir> [<snapshot_mem_file>]`
//!
//! - `socket_path`: UDS to listen on; Firecracker connects to it (`mem_backend.backend_path`).
//! - `output_dir`: every `SnapshotRequest` writes `<output_dir>/mem.<n>` (n = 0, 1, ...). Full
//!   snapshots contain all plugged memory; diff snapshots are sparse files of `total_size` bytes
//!   containing only the dirty pages, i.e. the same format Firecracker writes with
//!   `mem_file_path`, so they can be merged with `rebase-snap`.
//! - `snapshot_mem_file`: when restoring, the memory file page faults are served from.
//!
//! Pre-copy: on `SIGUSR1` the backend asks Firecracker for the pages dirtied since the last
//! request (`GetDirtyRanges`, usable while the microVM runs) and copies them into
//! `<output_dir>/precopy.mem`, then writes `<output_dir>/precopy.<n>.done`. Repeated passes plus
//! a final `Diff` snapshot taken while paused reconstruct the guest memory, which is the source
//! side of a live migration.

#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

#[path = "../uffd/uffd_utils.rs"]
mod uffd_utils;

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use uffd_utils::{GuestRegionUffdMapping, UffdHandler};
use userfaultfd::Uffd;
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

// The protocol types below mirror the ones in `vmm::mem_backend` / `vmm::vstate::memory`; the
// wire format is JSON, one message per line.

#[derive(Debug, Deserialize)]
struct MemBackendRegion {
    guest_addr: u64,
    size: u64,
    file_offset: u64,
    host_virt_addr: u64,
    region_type: String,
    slot_size: u64,
    plugged: Vec<bool>,
}

#[derive(Debug, Deserialize)]
struct MemBackendHandshake {
    fds: Vec<String>,
    page_size: usize,
    track_dirty_pages: bool,
    total_size: u64,
    regions: Vec<MemBackendRegion>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct MemRange {
    offset: u64,
    len: u64,
}

#[derive(Debug, Deserialize)]
enum SnapshotType {
    Full,
    Diff,
}

/// Everything Firecracker may send us: notifications, and responses to our requests.
#[derive(Debug, Deserialize)]
enum Incoming {
    SnapshotRequest {
        snapshot_type: SnapshotType,
        regions: Vec<MemBackendRegion>,
        ranges: Vec<MemRange>,
    },
    DirtyRanges {
        ranges: Vec<MemRange>,
    },
    Error {
        kind: String,
        message: String,
    },
}

#[derive(Debug, Serialize)]
enum MemBackendRequest {
    GetDirtyRanges {},
}

/// Set by the `SIGUSR1` handler; a pre-copy pass is performed at the next loop iteration.
static PRECOPY_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigusr1(_: libc::c_int) {
    PRECOPY_REQUESTED.store(true, Ordering::SeqCst);
}

/// Installs the `SIGUSR1` handler, without `SA_RESTART` so that `poll` is interrupted.
fn install_precopy_signal_handler() {
    // SAFETY: zeroed sigaction is a valid starting point; the handler only touches an atomic.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_sigusr1 as *const () as usize;
        libc::sigemptyset(&raw mut action.sa_mask);
        action.sa_flags = 0;
        assert_eq!(
            libc::sigaction(libc::SIGUSR1, &action, ptr::null_mut()),
            0,
            "cannot install SIGUSR1 handler"
        );
    }
}

#[derive(Debug, Serialize)]
enum MemBackendReply {
    SnapshotDone { success: bool, message: String },
}

/// Receives the handshake message and the fds attached to it.
fn recv_handshake(stream: &UnixStream) -> (MemBackendHandshake, Vec<RawFd>) {
    let mut buf = vec![0u8; 1 << 20];
    let mut fds = [-1 as RawFd; 4];
    let mut iov = [libc::iovec {
        iov_base: buf.as_mut_ptr().cast::<c_void>(),
        iov_len: buf.len(),
    }];
    // SAFETY: `iov` points into `buf`, which is valid for its whole length.
    let (n, nfds) = unsafe { stream.recv_with_fds(&mut iov, &mut fds) }
        .expect("cannot receive handshake from Firecracker");
    buf.truncate(n);
    let line_end = buf
        .iter()
        .position(|&b| b == b'\n')
        .expect("handshake is not newline terminated");
    let handshake: MemBackendHandshake =
        serde_json::from_slice(&buf[..line_end]).expect("cannot parse handshake");
    assert_eq!(
        handshake.fds.len(),
        nfds,
        "received {nfds} fds but the handshake names {}",
        handshake.fds.len()
    );
    (handshake, fds[..nfds].to_vec())
}

/// Copies `len` bytes at `offset` from `src` to the same offset of `dst`, kernel-side when
/// possible.
fn copy_range(src: &File, dst: &File, offset: u64, len: u64) {
    let mut remaining = len;
    let mut off_in = offset.cast_signed();
    let mut off_out = offset.cast_signed();
    while remaining > 0 {
        // SAFETY: both fds are valid open files and the offsets are owned local variables.
        let n = unsafe {
            libc::copy_file_range(
                src.as_raw_fd(),
                &mut off_in,
                dst.as_raw_fd(),
                &mut off_out,
                remaining as usize,
                0,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            // Fall back to a userspace copy for filesystems not supporting copy_file_range.
            if err.raw_os_error() == Some(libc::EXDEV) || err.raw_os_error() == Some(libc::EINVAL) {
                use std::os::unix::fs::FileExt;
                let mut chunk = vec![0u8; 1 << 20];
                let mut o = off_in as u64;
                let end = offset + len;
                while o < end {
                    let take = ((end - o) as usize).min(chunk.len());
                    src.read_exact_at(&mut chunk[..take], o)
                        .expect("read memfd");
                    dst.write_all_at(&chunk[..take], o).expect("write snapshot");
                    o += take as u64;
                }
                return;
            }
            panic!("copy_file_range failed: {err}");
        }
        if n == 0 {
            panic!("copy_file_range copied nothing");
        }
        remaining -= n as u64;
    }
}

/// `lseek(fd, offset, whence)`, mapping ENXIO (no more data/holes) to `end`.
fn seek_or_end(fd: &File, offset: u64, whence: libc::c_int, end: u64) -> u64 {
    // SAFETY: valid fd; lseek does not touch memory.
    let r = unsafe { libc::lseek(fd.as_raw_fd(), offset.cast_signed(), whence) };
    if r < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENXIO) {
            return end;
        }
        panic!("lseek failed: {err}");
    }
    (r as u64).min(end)
}

/// Copies a range of guest memory into the snapshot file.
///
/// Pages Firecracker never touched are holes in the memfd. When the microVM was restored through
/// this backend, Firecracker's own memory dump would fault such pages in from the snapshot file we
/// serve page faults from, so to produce the same bytes we fill holes from that file. Without a
/// backing file (fresh boot), holes are zero in the memfd and in Firecracker's dump alike.
fn copy_guest_range(memfd: &File, backing: Option<&File>, dst: &File, offset: u64, len: u64) {
    let end = offset + len;
    let mut pos = offset;
    while pos < end {
        let data_start = seek_or_end(memfd, pos, libc::SEEK_DATA, end);
        if data_start > pos
            && let Some(backing) = backing
        {
            copy_range(backing, dst, pos, data_start - pos);
        }
        if data_start >= end {
            break;
        }
        let data_end = seek_or_end(memfd, data_start, libc::SEEK_HOLE, end);
        copy_range(memfd, dst, data_start, data_end - data_start);
        pos = data_end;
    }
}

struct Backend {
    stream: UnixStream,
    memfd: File,
    /// The snapshot memory file page faults are served from, when restoring.
    backing_file: Option<File>,
    total_size: u64,
    output_dir: PathBuf,
    snapshots_taken: usize,
    /// Image accumulating the pre-copy passes, created on the first pass.
    precopy_file: Option<File>,
    precopy_passes: usize,
    /// Set when restoring: serves page faults from the snapshot memory file.
    uffd_handler: Option<UffdHandler>,
    /// Bytes received on the stream but not yet forming a complete line.
    buf: Vec<u8>,
}

impl Backend {
    fn new(stream: UnixStream, output_dir: PathBuf, backing_file: Option<&Path>) -> Self {
        let (handshake, fds) = recv_handshake(&stream);
        println!(
            "memory backend: handshake received: fds={:?} total_size={} page_size={} \
             track_dirty_pages={} regions={}",
            handshake.fds,
            handshake.total_size,
            handshake.page_size,
            handshake.track_dirty_pages,
            handshake.regions.len()
        );
        for r in &handshake.regions {
            println!(
                "  region {} guest_addr={:#x} size={:#x} file_offset={:#x} host={:#x} \
                 slot_size={:#x} plugged={:?}",
                r.region_type,
                r.guest_addr,
                r.size,
                r.file_offset,
                r.host_virt_addr,
                r.slot_size,
                r.plugged
            );
        }

        let mut memfd = None;
        let mut uffd = None;
        for (name, fd) in handshake.fds.iter().zip(fds) {
            match name.as_str() {
                // SAFETY: the fd was just received via SCM_RIGHTS and is owned by us.
                "memfd" => memfd = Some(unsafe { File::from_raw_fd(fd) }),
                // SAFETY: as above.
                "uffd" => uffd = Some(unsafe { Uffd::from_raw_fd(fd) }),
                other => panic!("unknown fd {other} in handshake"),
            }
        }
        let memfd = memfd.expect("handshake did not carry the memfd");
        assert_eq!(
            memfd.metadata().unwrap().len(),
            handshake.total_size,
            "memfd size does not match total_size"
        );

        let backing_file =
            backing_file.map(|path| File::open(path).expect("cannot open snapshot memory file"));
        let uffd_handler = uffd.map(|uffd| {
            let file = backing_file
                .as_ref()
                .expect("a snapshot memory file is required to serve page faults");
            let size = file.metadata().unwrap().len() as usize;
            // SAFETY: valid fd and size; the mapping lives as long as the process.
            let backing = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    size,
                    libc::PROT_READ,
                    libc::MAP_PRIVATE | libc::MAP_POPULATE,
                    file.as_raw_fd(),
                    0,
                )
            };
            assert_ne!(
                backing,
                libc::MAP_FAILED,
                "mmap of snapshot memory file failed"
            );
            let mappings = handshake
                .regions
                .iter()
                .map(|r| GuestRegionUffdMapping {
                    base_host_virt_addr: r.host_virt_addr,
                    size: r.size as usize,
                    offset: r.file_offset,
                    page_size: handshake.page_size,
                })
                .collect();
            UffdHandler::new(mappings, uffd, backing.cast(), size)
        });

        Self {
            stream,
            memfd,
            backing_file,
            total_size: handshake.total_size,
            output_dir,
            snapshots_taken: 0,
            precopy_file: None,
            precopy_passes: 0,
            uffd_handler,
            buf: Vec::new(),
        }
    }

    fn send<T: Serialize>(&mut self, msg: &T) {
        let mut line = serde_json::to_vec(msg).unwrap();
        line.push(b'\n');
        self.stream
            .write_all(&line)
            .expect("cannot write to Firecracker");
    }

    /// Reads from the stream; returns false on EOF.
    fn fill(&mut self) -> bool {
        let mut chunk = [0u8; 4096];
        let n = self
            .stream
            .read(&mut chunk)
            .expect("cannot read from Firecracker");
        self.buf.extend_from_slice(&chunk[..n]);
        n > 0
    }

    fn next_line(&mut self) -> Option<Vec<u8>> {
        let pos = self.buf.iter().position(|&b| b == b'\n')?;
        let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
        line.pop();
        Some(line)
    }

    fn handle_snapshot_request(
        &mut self,
        snapshot_type: SnapshotType,
        regions: Vec<MemBackendRegion>,
        ranges: Vec<MemRange>,
    ) {
        let path = self
            .output_dir
            .join(format!("mem.{}", self.snapshots_taken));
        self.snapshots_taken += 1;
        let copied: u64 = ranges.iter().map(|r| r.len).sum();
        println!(
            "memory backend: {snapshot_type:?} snapshot requested, {} ranges / {copied} bytes -> {}",
            ranges.len(),
            path.display()
        );

        let result = (|| -> std::io::Result<()> {
            // Like Firecracker with `mem_file_path`: a fresh sparse file of the full memory size,
            // with only the requested ranges populated. Unplugged slots stay zero.
            let out = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)?;
            out.set_len(self.total_size)?;
            for range in &ranges {
                assert!(range.offset + range.len <= self.total_size);
                copy_guest_range(
                    &self.memfd,
                    self.backing_file.as_ref(),
                    &out,
                    range.offset,
                    range.len,
                );
            }
            out.sync_all()?;
            // Sanity check the layout we were told about against what we copied.
            let described: u64 = regions.iter().map(|r| r.size).sum();
            assert_eq!(described, self.total_size);
            Ok(())
        })();

        match result {
            Ok(()) => self.send(&MemBackendReply::SnapshotDone {
                success: true,
                message: String::new(),
            }),
            Err(err) => self.send(&MemBackendReply::SnapshotDone {
                success: false,
                message: err.to_string(),
            }),
        }
    }

    /// Applies the dirty ranges of a pre-copy pass to the pre-copy image.
    fn handle_precopy_ranges(&mut self, ranges: Vec<MemRange>) {
        if self.precopy_file.is_none() {
            let path = self.output_dir.join("precopy.mem");
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .expect("cannot create pre-copy image");
            file.set_len(self.total_size)
                .expect("cannot size pre-copy image");
            self.precopy_file = Some(file);
        }
        let out = self.precopy_file.as_ref().unwrap();
        let copied: u64 = ranges.iter().map(|r| r.len).sum();
        for range in &ranges {
            assert!(range.offset + range.len <= self.total_size);
            copy_guest_range(
                &self.memfd,
                self.backing_file.as_ref(),
                out,
                range.offset,
                range.len,
            );
        }
        out.sync_all().expect("cannot sync pre-copy image");

        self.precopy_passes += 1;
        println!(
            "memory backend: pre-copy pass {} copied {} ranges / {copied} bytes",
            self.precopy_passes,
            ranges.len()
        );
        // Marker for whoever requested the pass; contains the amount copied.
        std::fs::write(
            self.output_dir
                .join(format!("precopy.{}.done", self.precopy_passes)),
            format!("{} {copied}\n", ranges.len()),
        )
        .expect("cannot write pre-copy marker");
    }

    fn handle_line(&mut self, line: &[u8]) {
        match serde_json::from_slice::<Incoming>(line) {
            Ok(Incoming::SnapshotRequest {
                snapshot_type,
                regions,
                ranges,
            }) => self.handle_snapshot_request(snapshot_type, regions, ranges),
            Ok(Incoming::DirtyRanges { ranges }) => self.handle_precopy_ranges(ranges),
            Ok(Incoming::Error { kind, message }) => {
                panic!("Firecracker rejected our request: {kind}: {message}")
            }
            Err(err) => panic!(
                "unexpected message from Firecracker: {err}: {}",
                String::from_utf8_lossy(line)
            ),
        }
    }

    /// Serves all pending page faults (see `examples/uffd/on_demand_handler.rs` for the caveats
    /// around `remove` events, which this simplified loop shares).
    fn serve_page_faults(&mut self) {
        let handler = self.uffd_handler.as_mut().unwrap();
        let mut deferred = Vec::new();
        loop {
            let mut events = std::mem::take(&mut deferred);
            while let Some(event) = handler.read_event().expect("failed to read uffd event") {
                events.push(event);
            }
            for event in events {
                match event {
                    userfaultfd::Event::Pagefault { addr, .. } => {
                        if !handler.serve_pf(addr.cast(), handler.page_size) {
                            deferred.push(event);
                        }
                    }
                    userfaultfd::Event::Remove { start, end } => {
                        handler.unregister_range(start, end)
                    }
                    _ => panic!("unexpected event on userfaultfd"),
                }
            }
            if deferred.is_empty() {
                break;
            }
        }
    }

    fn run(&mut self) {
        let mut pollfds = vec![libc::pollfd {
            fd: self.stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        if let Some(handler) = &self.uffd_handler {
            pollfds.push(libc::pollfd {
                fd: handler.uffd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }

        loop {
            if PRECOPY_REQUESTED.swap(false, Ordering::SeqCst) {
                println!("memory backend: requesting dirty ranges for a pre-copy pass");
                self.send(&MemBackendRequest::GetDirtyRanges {});
            }

            // SAFETY: `pollfds` is a valid array of the given length.
            let nready = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as u64, -1) };
            if nready < 0 {
                let err = std::io::Error::last_os_error();
                // Interrupted by SIGUSR1: go back to check the pre-copy flag.
                assert_eq!(err.raw_os_error(), Some(libc::EINTR), "poll failed: {err}");
                continue;
            }

            if pollfds[0].revents & libc::POLLIN != 0 {
                if !self.fill() {
                    println!("memory backend: Firecracker closed the connection, exiting");
                    return;
                }
                while let Some(line) = self.next_line() {
                    self.handle_line(&line);
                }
            }
            if pollfds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                println!("memory backend: Firecracker went away, exiting");
                return;
            }
            if pollfds.len() > 1 && pollfds[1].revents & libc::POLLIN != 0 {
                self.serve_page_faults();
            }
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let socket_path = args
        .next()
        .expect("usage: memory_backend <socket> <output_dir> [<mem_file>]");
    let output_dir = PathBuf::from(args.next().expect("no output directory given"));
    let backing_file = args.next().map(PathBuf::from);

    std::fs::create_dir_all(&output_dir).expect("cannot create output directory");
    let listener = UnixListener::bind(&socket_path).expect("cannot bind to socket path");
    let (stream, _) = listener.accept().expect("cannot accept connection");

    let mut backend = Backend::new(stream, output_dir, backing_file.as_deref());
    install_precopy_signal_handler();
    backend.run();
}
