// Copyright 2022 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::undocumented_unsafe_blocks,
    // Not everything is used by both binaries
    dead_code
)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::Duration;

use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use userfaultfd::{Error, Event, Uffd};
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

// This is the same with the one used in src/vmm.
/// This describes the mapping between Firecracker base virtual address and offset in the
/// buffer or file backend for a guest memory region. It is used to tell an external
/// process/thread where to populate the guest memory data for this range.
///
/// E.g. Guest memory contents for a region of `size` bytes can be found in the backend
/// at `offset` bytes from the beginning, and should be copied/populated into `base_host_address`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GuestRegionUffdMapping {
    /// Base host virtual address where the guest memory contents for this region
    /// should be copied/populated.
    pub base_host_virt_addr: u64,
    /// Region size.
    pub size: usize,
    /// Offset in the backend file/buffer where the region contents are.
    pub offset: u64,
    /// The configured page size for this memory region.
    pub page_size: usize,
}

impl GuestRegionUffdMapping {
    fn contains(&self, fault_page_addr: u64) -> bool {
        fault_page_addr >= self.base_host_virt_addr
            && fault_page_addr < self.base_host_virt_addr + self.size as u64
    }
}

/// A page-aligned byte range in guest memory file / memfd offset space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryRange {
    pub offset: u64,
    pub len: u64,
}

// This is the same with the one used in src/vmm (`SnapshotMemoryLayout`).
/// The `memory` object returned by `PUT /snapshot/create` (`Diff`) and
/// `PUT /snapshot/create` (Backend) or `PUT /snapshot/dirty-pages` when a memory backend is attached: which pages of the memory file
/// make up the diff and where their content is. Every page is in one of three classes: *memfd
/// authoritative* (copy it from the memfd), *zero* (make it read as zero), or neither (unchanged,
/// not part of the diff). Full snapshots are written by Firecracker itself.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotMemoryLayout {
    /// Size of a full guest memory file.
    pub total_size: u64,
    /// Granularity of both bitmaps, in bytes (the host page size).
    pub page_size: u64,
    /// Encoding of the bitmaps before base64; only `roaring` exists.
    #[serde(default = "default_bitmap_encoding")]
    pub bitmap_encoding: String,
    /// Standard base64 of a Roaring bitmap (portable serialization) of the page indices (file
    /// offset / `page_size`) to copy from the memfd.
    pub pages_to_copy: String,
    /// Same encoding: the page indices that must read as zero in the memory file.
    pub pages_to_discard: String,
}

fn default_bitmap_encoding() -> String {
    "roaring".to_string()
}

/// A decoded [`SnapshotMemoryLayout`]: both page sets as Roaring bitmaps.
#[derive(Debug, Clone)]
pub struct DecodedLayout {
    pub total_size: u64,
    pub page_size: u64,
    pub authoritative: RoaringBitmap,
    pub zero: RoaringBitmap,
}

/// What to do with one page of the memory file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageClass {
    /// Copy from the memfd.
    Authoritative,
    /// Make it read as zero.
    Zero,
    /// Unchanged: not part of the diff, leave the target alone.
    Unchanged,
}

impl DecodedLayout {
    /// The class of the page at file offset `offset`. Constant time.
    pub fn class(&self, offset: u64) -> PageClass {
        let page = u32::try_from(offset / self.page_size).expect("offset within the file");
        if self.zero.contains(page) {
            PageClass::Zero
        } else if self.authoritative.contains(page) {
            PageClass::Authoritative
        } else {
            PageClass::Unchanged
        }
    }

    /// Runs of consecutive pages of the same class, in file order, `Unchanged` runs omitted.
    /// Walks the two sets run by run (`Iter::next_range`), so the cost is proportional to the
    /// number of runs in the diff, not to the number of pages: a released or unplugged region
    /// of any size is one step.
    pub fn runs(&self) -> Vec<(PageClass, MemoryRange)> {
        let mut runs: Vec<(PageClass, MemoryRange)> = Vec::new();
        for (class, set) in [
            (PageClass::Authoritative, &self.authoritative),
            (PageClass::Zero, &self.zero),
        ] {
            let mut pages = set.iter();
            while let Some(run) = pages.next_range() {
                let first = u64::from(*run.start());
                let count = u64::from(*run.end()) - first + 1;
                runs.push((
                    class,
                    MemoryRange {
                        offset: first * self.page_size,
                        len: count * self.page_size,
                    },
                ));
            }
        }
        // Authoritative runs then zero runs; put them in file order for sequential I/O.
        runs.sort_by_key(|(_, range)| range.offset);
        runs
    }
}

impl SnapshotMemoryLayout {
    /// Decodes both bitmaps (base64, then Roaring) and validates them.
    pub fn decode(&self) -> Result<DecodedLayout, std::io::Error> {
        let invalid = |msg: String| std::io::Error::new(std::io::ErrorKind::InvalidData, msg);
        if self.page_size == 0 {
            return Err(invalid("invalid page_size 0".into()));
        }
        if self.bitmap_encoding != "roaring" {
            return Err(invalid(format!(
                "unsupported bitmap_encoding {:?}",
                self.bitmap_encoding
            )));
        }
        let pages = self.total_size.div_ceil(self.page_size);
        let decode = |name: &str, field: &str| {
            let bytes = base64_decode(field.as_bytes())
                .ok_or_else(|| invalid(format!("invalid base64 in `{name}`")))?;
            let set = RoaringBitmap::deserialize_from(&bytes[..])
                .map_err(|err| invalid(format!("invalid Roaring bitmap in `{name}`: {err}")))?;
            if set.max().is_some_and(|max| u64::from(max) >= pages) {
                return Err(invalid(format!(
                    "`{name}` has page {} but the file has {pages} pages",
                    set.max().unwrap()
                )));
            }
            Ok(set)
        };
        let authoritative = decode("pages_to_copy", &self.pages_to_copy)?;
        let zero = decode("pages_to_discard", &self.pages_to_discard)?;
        if !(&authoritative & &zero).is_empty() {
            return Err(invalid("a page is both authoritative and zero".into()));
        }
        Ok(DecodedLayout {
            total_size: self.total_size,
            page_size: self.page_size,
            authoritative,
            zero,
        })
    }
}

/// Decodes standard base64 with `=` padding (RFC 4648 §4). Whitespace is not accepted. Returns
/// `None` on any malformed input. Written out here so that the example handlers stay free of
/// dependencies beyond what Firecracker's own examples already use.
pub fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    if !input.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    for (i, chunk) in input.chunks(4).enumerate() {
        let last = i == input.len() / 4 - 1;
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut acc = 0u32;
        for &c in &chunk[..4 - pad] {
            acc = (acc << 6) | value(c)?;
        }
        acc <<= 6 * pad as u32;
        let bytes = acc.to_be_bytes();
        out.extend_from_slice(&bytes[1..4 - pad]);
    }
    Some(out)
}

/// Encodes to standard, padded base64. Test helper and convenience for orchestrators built on
/// this module.
pub fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let mut acc = 0u32;
        for (i, &b) in chunk.iter().enumerate() {
            acc |= u32::from(b) << (16 - 8 * i);
        }
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[((acc >> (18 - 6 * i)) & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Requests the orchestrator can send on the handler's control socket. Unrelated to Firecracker:
/// how the `memory` object travels from the API response to the process holding the memfd is
/// entirely up to the orchestrator; this is what the example handlers (and the integration test
/// framework) do.
#[derive(Debug, Serialize, Deserialize)]
pub enum ControlRequest {
    /// Apply `memory` to `mem_path` (created at `memory.total_size` if it does not exist,
    /// merged into otherwise): authoritative pages from the memfd, zero chunks zeroed, the rest
    /// left alone.
    Copy {
        mem_path: PathBuf,
        memory: SnapshotMemoryLayout,
    },
    /// Turn this connection into a page server: after the request, the peer sends
    /// `(offset: u64, len: u64)` little-endian pairs and gets `len` raw bytes of the memfd back
    /// for each, until it closes the connection. Used by a post-copy destination to fetch the
    /// pages its pre-copy target does not have.
    Serve,
    /// Post-copy: serve this microVM's faults from the pre-copy target for pages it has, from
    /// the source backend at `source_sock` (a [`ControlRequest::Serve`] peer) for the pages of
    /// `memory.pages_to_copy`, and as zero for `memory.pages_to_discard`. The pending pages are
    /// also pulled in the background while no fault is waiting, and written into the target, so
    /// that it ends up a complete snapshot and the source can be released.
    PostCopy {
        memory: SnapshotMemoryLayout,
        source_sock: PathBuf,
    },
}

/// Reply to a [`ControlRequest`].
#[derive(Debug, Serialize, Deserialize)]
pub enum ControlResponse {
    Done {
        success: bool,
        message: String,
        /// Bytes zeroed in the target without reading guest memory (`pages_to_discard`). Lets an
        /// orchestrator observe the saving.
        zeroed_bytes: u64,
        /// Bytes copied from the memfd (`pages_to_copy`).
        copied_bytes: u64,
    },
}

/// One handshake message from Firecracker: the region mappings plus the fds that came with
/// them. Which fds are present depends on how the microVM was started:
///
/// | Situation                                | fds             |
/// | :--------------------------------------- | :-------------- |
/// | restore, `backend_type: Uffd`            | `[uffd]`        |
/// | restore, `backend_type: SharedMemfd`   | `[uffd, memfd]` |
/// | boot, `machine-config.mem_backend`       | `[memfd]`       |
///
/// The uffd, when present, is always first; the memfd is always last.
#[derive(Debug)]
pub struct Handshake {
    pub mappings: Vec<GuestRegionUffdMapping>,
    pub uffd: Option<Uffd>,
    pub memfd: Option<File>,
}

impl Handshake {
    /// Maximum number of fds a handshake can carry today.
    const MAX_FDS: usize = 2;

    fn try_receive(stream: &UnixStream) -> Result<(String, Vec<File>), std::io::Error> {
        let mut message_buf = vec![0u8; 1024];
        let mut fds = [-1 as RawFd; Self::MAX_FDS];
        let mut iovecs = [libc::iovec {
            iov_base: message_buf.as_mut_ptr().cast::<c_void>(),
            iov_len: message_buf.len(),
        }];
        // SAFETY: `iovecs` points into `message_buf`, which we own and which is safe to write
        // arbitrary bytes into.
        let (bytes_read, fd_count) = unsafe { stream.recv_with_fds(&mut iovecs, &mut fds)? };
        message_buf.resize(bytes_read, 0);
        // SAFETY: the first `fd_count` entries are fds we now own and nobody else closes.
        let files = fds[..fd_count]
            .iter()
            .map(|&fd| unsafe { File::from_raw_fd(fd) })
            .collect();

        // We do not expect to receive non-UTF-8 data from Firecracker, so this is probably
        // an error we can't recover from. Just immediately abort
        let body = String::from_utf8(message_buf.clone()).unwrap_or_else(|_| {
            panic!(
                "Received body is not a utf-8 valid string. Raw bytes received: {message_buf:#?}"
            )
        });
        Ok((body, files))
    }

    /// Receives the handshake from `stream`, retrying a few times if no fd came along.
    pub fn receive(stream: &UnixStream) -> Self {
        // Sometimes, reading from the stream succeeds but we don't receive any
        // descriptor. We don't really have a good understanding why this is
        // happening, but let's try to be a bit more robust and retry a few times
        // before we declare defeat.
        for _ in 1..=5 {
            match Self::try_receive(stream) {
                Ok((body, files)) if !files.is_empty() => {
                    return Self::from_message(&body, files);
                }
                Ok((body, _)) => {
                    println!(
                        "Didn't receive any fd over socket. We received: '{body}'. Retrying..."
                    );
                }
                Err(err) => {
                    println!("Could not get fds and mapping from Firecracker: {err}. Retrying...");
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        panic!("Could not get fds and mappings after 5 retries");
    }

    fn from_message(body: &str, mut files: Vec<File>) -> Self {
        let mappings =
            serde_json::from_str::<Vec<GuestRegionUffdMapping>>(body).unwrap_or_else(|_| {
                panic!("Cannot deserialize memory mappings. Received body: {body}")
            });
        assert!(
            !mappings.is_empty(),
            "Cannot get the first mapping. Mappings size is 0. Received body: {body}"
        );

        // The uffd is first and the memfd is last. With a single fd, look at what it is: a
        // handler knows what it was started for, but checking costs nothing.
        let (uffd, memfd) = match files.len() {
            2 => {
                let memfd = files.pop().unwrap();
                (Some(files.pop().unwrap()), Some(memfd))
            }
            1 => {
                let file = files.pop().unwrap();
                if is_memfd(&file) {
                    (None, Some(file))
                } else {
                    (Some(file), None)
                }
            }
            n => panic!("Unexpected number of fds in handshake: {n}"),
        };

        if let Some(memfd) = &memfd {
            let memsize: u64 = mappings.iter().map(|r| r.size as u64).sum();
            let memfd_size = memfd.metadata().expect("cannot stat memfd").len();
            assert_eq!(
                memsize, memfd_size,
                "memfd size does not match the sum of the region sizes"
            );
        }

        Self {
            mappings,
            uffd: uffd.map(|file| unsafe { Uffd::from_raw_fd(file.into_raw_fd()) }),
            memfd,
        }
    }
}

/// Whether `file` is a memfd rather than a uffd. A memfd is a regular file (on tmpfs or
/// hugetlbfs); a uffd is an anonymous inode without a file type. `/proc/self/fd` would tell the
/// two apart as well, but the handler usually runs in a chroot without `/proc`.
fn is_memfd(file: &File) -> bool {
    file.metadata()
        .map(|meta| meta.file_type().is_file())
        .unwrap_or(false)
}

#[derive(Debug)]
pub struct UffdHandler {
    pub mem_regions: Vec<GuestRegionUffdMapping>,
    pub page_size: usize,
    backing_buffer: *const u8,
    uffd: Uffd,
    post_copy: Option<PostCopy>,
}

/// Post-copy state of a destination handler (see [`ControlRequest::PostCopy`]).
#[derive(Debug)]
pub struct PostCopy {
    /// Pages of the layout still to be fetched from the source.
    pending: RoaringBitmap,
    /// Pages that read as zero.
    zero: RoaringBitmap,
    /// Connection to the source backend, in `Serve` mode.
    source: UnixStream,
    /// The pre-copy target, written as pages arrive.
    target: File,
    /// The layout's page (the host page); a fault covers `fault_size / page_size` of them.
    page_size: u64,
    fault_size: u64,
    started: std::time::Instant,
    total_pages: u64,
    on_demand_pages: u64,
    background_pages: u64,
    buf: Vec<u8>,
    zeros: Vec<u8>,
}

impl PostCopy {
    /// Pages pulled per background step while no fault is waiting.
    const BACKGROUND_PAGES: u64 = 256;

    fn new(
        layout: DecodedLayout,
        source_sock: &Path,
        target: File,
    ) -> Result<Self, std::io::Error> {
        let mut source = UnixStream::connect(source_sock)?;
        set_socket_buffers(&source);
        source.write_all(&serde_json::to_vec(&ControlRequest::Serve).unwrap())?;
        let total_pages = layout.authoritative.len();
        Ok(Self {
            pending: layout.authoritative,
            zero: layout.zero,
            source,
            target,
            page_size: layout.page_size,
            fault_size: 0,
            started: std::time::Instant::now(),
            total_pages,
            on_demand_pages: 0,
            background_pages: 0,
            buf: Vec::new(),
            zeros: Vec::new(),
        })
    }

    /// Binds the state to a mapping whose faults are `fault_size` bytes (a multiple of the
    /// layout page: the backing page, 2 MiB on hugetlbfs).
    fn attach(&mut self, fault_size: u64) -> Result<(), String> {
        if fault_size % self.page_size != 0 {
            return Err(format!(
                "layout page size {} does not divide the mapping's {fault_size}",
                self.page_size
            ));
        }
        self.fault_size = fault_size;
        self.buf = vec![0; (Self::BACKGROUND_PAGES * self.page_size).max(fault_size) as usize];
        self.zeros = vec![0; fault_size as usize];
        Ok(())
    }

    /// The layout pages a fault at file offset `offset` covers.
    fn fault_pages(&self, offset: u64) -> std::ops::Range<u32> {
        let first = u32::try_from(offset / self.page_size).unwrap();
        first..first + u32::try_from(self.fault_size / self.page_size).unwrap()
    }

    /// Fetches `pages` pages at page index `first` from the source into `buf` and the target.
    /// `on_demand` is whether a fault asked for them (for the accounting; the pending ones among
    /// them count, a fault fetches its whole block).
    fn fetch(&mut self, first: u32, pages: u64, on_demand: bool) -> Result<&[u8], std::io::Error> {
        let range = first..first + pages as u32;
        let newly = self.pending.range_cardinality(range.clone());
        if on_demand {
            self.on_demand_pages += newly;
        } else {
            self.background_pages += newly;
        }
        let offset = u64::from(first) * self.page_size;
        let len = pages * self.page_size;
        let mut request = [0u8; 16];
        request[..8].copy_from_slice(&offset.to_le_bytes());
        request[8..].copy_from_slice(&len.to_le_bytes());
        self.source.write_all(&request)?;
        let buf = &mut self.buf[..len as usize];
        self.source.read_exact(buf)?;
        self.target.write_all_at(buf, offset)?;
        self.pending.remove_range(range);
        if self.pending.is_empty() {
            println!(
                "Post-copy complete: {} pages, {} on demand, {} in the background, {} ms",
                self.total_pages,
                self.on_demand_pages,
                self.background_pages,
                self.started.elapsed().as_millis()
            );
        }
        Ok(buf)
    }

    /// One background step: the first pending run, bounded.
    fn pull_some(&mut self) {
        let Some(run) = self.pending.iter().next_range() else {
            return;
        };
        let first = *run.start();
        let pages = (u64::from(*run.end()) - u64::from(first) + 1).min(Self::BACKGROUND_PAGES);
        self.fetch(first, pages, false)
            .expect("fetch from the source backend failed");
    }
}

impl UffdHandler {
    /// Builds a page fault handler from a handshake that carried a uffd, populating faults from
    /// the `size`-byte buffer at `backing_buffer` (the mapped snapshot memory file).
    pub fn new(
        mappings: Vec<GuestRegionUffdMapping>,
        uffd: Uffd,
        backing_buffer: *const u8,
        size: usize,
    ) -> Self {
        let memsize: usize = mappings.iter().map(|r| r.size).sum();
        // Page size is the same for all memory regions, so just grab the first one
        let page_size = mappings.first().unwrap().page_size;

        // Make sure memory size matches backing data size.
        assert_eq!(memsize, size);
        assert!(page_size.is_power_of_two());

        Self {
            mem_regions: mappings,
            page_size,
            backing_buffer,
            uffd,
            post_copy: None,
        }
    }

    /// Whether post-copy is on and pages are still pending.
    pub fn post_copy_pending(&self) -> bool {
        self.post_copy
            .as_ref()
            .is_some_and(|pc| !pc.pending.is_empty())
    }

    /// Pulls a few pending pages from the source; called while no fault is waiting.
    pub fn post_copy_step(&mut self) {
        if let Some(pc) = self.post_copy.as_mut() {
            pc.pull_some();
        }
    }

    pub fn read_event(&mut self) -> Result<Option<Event>, Error> {
        self.uffd.read_event()
    }

    /// Handles a uffd `remove` event for `[start, end)`: Firecracker has discarded the range
    /// (balloon, virtio-mem), so it now reads as zero. The range is unregistered, after which the
    /// kernel serves zero pages for it without us. Nothing else to record: the discard is in the
    /// next snapshot response as `pages_to_discard`, and the registration itself is the record of
    /// which holes still read as the snapshot file (registered: never served) and which as zero
    /// (unregistered), should a backend ever need to tell them apart.
    ///
    /// The event is page (4 KiB) granular even for hugetlbfs-backed memory, while only whole
    /// backing pages can be punched out or unregistered. Like `hugetlbfs_punch_hole`, round
    /// inward: partially covered backing pages were not freed and keep their state.
    pub fn unregister_range(&mut self, start: *mut c_void, end: *mut c_void) {
        assert!(end > start);
        let start = (start as usize).next_multiple_of(self.page_size);
        let end = (end as usize) & !(self.page_size - 1);
        if start >= end {
            return;
        }
        let len = end - start;
        self.uffd
            .unregister(start as *mut c_void, len)
            .expect("range should be valid");
    }

    pub fn serve_pf(&mut self, addr: *mut u8, len: usize) -> bool {
        // Find the start of the page that the current faulting address belongs to.
        let dst = (addr as usize & !(self.page_size - 1)) as *mut libc::c_void;
        let fault_page_addr = dst as u64;

        if let Some(region) = self
            .mem_regions
            .iter()
            .find(|region| region.contains(fault_page_addr))
            .cloned()
        {
            if self.post_copy.is_some() {
                return self.populate_post_copy(&region, fault_page_addr, len);
            }
            return self.populate_from_file(&region, fault_page_addr, len);
        }

        panic!(
            "Could not find addr: {:?} within guest region mappings.",
            addr
        );
    }

    /// Post-copy: zero for a discarded page, a fetch from the source for a page the pre-copy
    /// target does not have yet, the target otherwise. A fault covers one backing page, which
    /// on hugetlbfs is many layout pages; residency, hence the zero class, is uniform across
    /// them, while only the pending pages of the block are fetched: they land in the target,
    /// and the block is then populated from the target like any other.
    fn populate_post_copy(
        &mut self,
        region: &GuestRegionUffdMapping,
        dst: u64,
        len: usize,
    ) -> bool {
        let pc = self.post_copy.as_mut().unwrap();
        let file_offset = region.offset + (dst - region.base_host_virt_addr);
        let pages = pc.fault_pages(file_offset);
        if pc.zero.contains(pages.start) {
            debug_assert!(pc.zero.contains_range(pages.clone()));
            let src = pc.zeros.as_ptr();
            return self.copy_into(src, dst, len);
        }
        let mut runs = Vec::new();
        let mut pending = pc.pending.range(pages);
        while let Some(run) = pending.next_range() {
            runs.push((*run.start(), u64::from(*run.end() - *run.start()) + 1));
        }
        for (first, count) in runs {
            pc.fetch(first, count, true)
                .expect("fetch from the source backend failed");
        }
        self.populate_from_file(region, dst, len)
    }

    fn populate_from_file(
        &mut self,
        region: &GuestRegionUffdMapping,
        dst: u64,
        len: usize,
    ) -> bool {
        let offset = dst - region.base_host_virt_addr;
        let src = self.backing_buffer as u64 + region.offset + offset;
        self.copy_into(src as *const u8, dst, len)
    }

    /// `UFFDIO_COPY` of `len` bytes from `src` to the faulting page at `dst`. Returns `false` on
    /// `EAGAIN` (a `remove` event is pending; retry later).
    fn copy_into(&mut self, src: *const u8, dst: u64, len: usize) -> bool {
        unsafe {
            match self.uffd.copy(src as *const _, dst as *mut _, len, true) {
                // Make sure the UFFD copied some bytes.
                Ok(value) => assert!(value > 0),
                // Catch EAGAIN errors, which occur when a `remove` event lands in the UFFD
                // queue while we're processing `pagefault` events.
                // The weird cast is because the `bytes_copied` field is based on the
                // `uffdio_copy->copy` field, which is a signed 64 bit integer, and if something
                // goes wrong, it gets set to a -errno code. However, uffd-rs always casts this
                // value to an unsigned `usize`, which scrambled the errno.
                Err(Error::PartiallyCopied(bytes_copied))
                    if bytes_copied == 0 || bytes_copied == (-libc::EAGAIN) as usize =>
                {
                    return false;
                }
                Err(Error::CopyFailed(errno))
                    if std::io::Error::from(errno).raw_os_error().unwrap() == libc::EEXIST => {}
                Err(e) => {
                    panic!("Uffd copy failed: {e:?}");
                }
            }
        };

        true
    }
}

/// What [`copy_pages`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CopyStats {
    /// Runs copied.
    pub copies: usize,
    /// Bytes copied from the memfd.
    pub copied_bytes: u64,
    /// Runs zeroed.
    pub zeroed: usize,
    /// Bytes zeroed.
    pub zeroed_bytes: u64,
}

/// Applies `layout` to the file at `mem_path`: authoritative pages are copied from `memfd` at
/// the same offset, zero chunks are zeroed, unchanged pages are left as they are. The file is
/// created at `layout.total_size` if it does not exist (a diff against a zero base: a booted
/// microVM); an existing file (the snapshot the diff is merged into) is left at its size.
///
/// This is what turns the `memory` object of a `PUT /snapshot/create` response into the memory
/// file Firecracker's own diff would have produced, byte for byte.
pub fn copy_pages(
    memfd: &File,
    mem_path: &Path,
    layout: &SnapshotMemoryLayout,
) -> Result<CopyStats, std::io::Error> {
    let target = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(mem_path)?;
    if target.metadata()?.len() < layout.total_size {
        target.set_len(layout.total_size)?;
    }
    let layout = layout.decode()?;

    let mut stats = CopyStats::default();
    for (class, run) in layout.runs() {
        match class {
            PageClass::Authoritative => {
                copy_range(memfd, &target, &run)?;
                stats.copies += 1;
                stats.copied_bytes += run.len;
            }
            PageClass::Zero => {
                write_zeros(&target, &run)?;
                stats.zeroed += 1;
                stats.zeroed_bytes += run.len;
            }
            PageClass::Unchanged => unreachable!("runs() omits unchanged pages"),
        }
    }
    Ok(stats)
}

/// `copy_file_range` from `src` to `dst` at the same offset, falling back to a read/write loop
/// where the kernel does not support it for this pair of files (e.g. hugetlbfs sources).
fn copy_range(src: &File, dst: &File, range: &MemoryRange) -> Result<(), std::io::Error> {
    let mut off_in: libc::loff_t = range.offset.cast_signed();
    let mut off_out: libc::loff_t = range.offset.cast_signed();
    let mut remaining = range.len as usize;

    while remaining > 0 {
        // SAFETY: both fds are valid and the offset pointers point to live locals.
        let copied = unsafe {
            libc::copy_file_range(
                src.as_raw_fd(),
                &raw mut off_in,
                dst.as_raw_fd(),
                &raw mut off_out,
                remaining,
                0,
            )
        };
        if copied < 0 {
            let err = std::io::Error::last_os_error();
            return match err.raw_os_error() {
                Some(libc::EXDEV | libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP) => {
                    copy_range_rw(
                        src,
                        dst,
                        &MemoryRange {
                            offset: off_in as u64,
                            len: remaining as u64,
                        },
                    )
                }
                _ => Err(err),
            };
        }
        if copied == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "copy_file_range copied 0 bytes",
            ));
        }
        remaining -= copied as usize;
    }
    Ok(())
}

fn copy_range_rw(src: &File, dst: &File, range: &MemoryRange) -> Result<(), std::io::Error> {
    const CHUNK: usize = 1 << 20;
    let mut buf = vec![0u8; CHUNK];
    let mut offset = range.offset;
    let end = range.offset + range.len;
    while offset < end {
        let len = ((end - offset) as usize).min(CHUNK);
        src.read_exact_at(&mut buf[..len], offset)?;
        dst.write_all_at(&buf[..len], offset)?;
        offset += len as u64;
    }
    Ok(())
}

/// Writes zeros over `range` in `file`. This never punches a hole: in a diff file a hole means
/// "page not in the diff" to `rebase-snap`, whereas a zero page *is* in the diff and must be
/// applied over the base's old bytes, exactly as Firecracker's own `dump_dirty` writes explicit
/// zeros for released pages.
fn write_zeros(file: &File, range: &MemoryRange) -> Result<(), std::io::Error> {
    const CHUNK: usize = 1 << 20;
    let zeros = vec![0u8; CHUNK];
    let mut offset = range.offset;
    let end = range.offset + range.len;
    while offset < end {
        let len = ((end - offset) as usize).min(CHUNK);
        file.write_all_at(&zeros[..len], offset)?;
        offset += len as u64;
    }
    Ok(())
}

#[derive(Debug)]
pub struct Runtime {
    /// Firecracker's connection, once it has connected.
    stream: Option<UnixStream>,
    /// Where Firecracker connects, until it has. Control requests are served meanwhile.
    listener: Option<UnixListener>,
    /// Whether the panic hook (which kills Firecracker) was requested before it connected.
    panic_hook_wanted: bool,
    /// The snapshot memory file page faults are populated from, if any. A handler started for a
    /// boot has none: there are no faults to serve.
    backing_file: Option<File>,
    backing_memory: *mut u8,
    backing_memory_size: usize,
    uffds: HashMap<i32, UffdHandler>,
    /// The guest memory memfd received in the handshake, if Firecracker shared it, and the
    /// backing page size of the regions in it.
    memfd: Option<(File, usize)>,
    /// Socket on which the orchestrator sends [`ControlRequest`]s.
    control: Option<UnixListener>,
    /// A `PostCopy` request received before Firecracker's handshake; attached to the uffd then.
    pending_post_copy: Option<PostCopy>,
}

/// Sizes a page-serving socket's buffers for multi-MiB transfers (the default is ~200 KiB,
/// which costs a wakeup every few dozen pages).
fn set_socket_buffers(sock: &UnixStream) {
    let size: libc::c_int = 8 << 20;
    for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        // SAFETY: `size` outlives the call and its length is passed.
        unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                opt,
                (&raw const size).cast::<c_void>(),
                size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
}

/// Maps (and populates) the snapshot memory file page faults are served from. Returns the
/// mapping and its size; a null pointer and 0 without a file.
fn map_backing_file(backing_file: Option<&File>) -> (*mut u8, usize) {
    let Some(backing_file) = backing_file else {
        return (ptr::null_mut(), 0);
    };
    let file_meta = backing_file
        .metadata()
        .expect("can not get backing file metadata");
    let backing_memory_size = file_meta.len() as usize;
    // # Safety:
    // File size and fd are valid
    let ret = unsafe {
        libc::mmap(
            ptr::null_mut(),
            backing_memory_size,
            libc::PROT_READ,
            libc::MAP_PRIVATE | libc::MAP_POPULATE,
            backing_file.as_raw_fd(),
            0,
        )
    };
    if ret == libc::MAP_FAILED {
        panic!("mmap on backing file failed");
    }
    (ret.cast::<u8>(), backing_memory_size)
}

impl Runtime {
    /// Creates a runtime serving faults for the connection `stream` from `backing_file`, if
    /// given, and answering control requests on `control`, if given.
    pub fn new(
        stream: UnixStream,
        backing_file: Option<File>,
        control: Option<UnixListener>,
    ) -> Self {
        let mapping = map_backing_file(backing_file.as_ref());
        Self::with_mapping(Some(stream), None, backing_file, mapping, control)
    }

    /// `new`, with the memory file already mapped by [`map_backing_file`], and either a
    /// connection from Firecracker or the listener it will connect to.
    fn with_mapping(
        stream: Option<UnixStream>,
        listener: Option<UnixListener>,
        backing_file: Option<File>,
        (backing_memory, backing_memory_size): (*mut u8, usize),
        control: Option<UnixListener>,
    ) -> Self {
        Self {
            stream,
            listener,
            panic_hook_wanted: false,
            backing_file,
            backing_memory,
            backing_memory_size,
            uffds: HashMap::default(),
            memfd: None,
            control,
            pending_post_copy: None,
        }
    }

    fn peer_process_credentials(&self) -> libc::ucred {
        let mut creds: libc::ucred = libc::ucred {
            pid: 0,
            gid: 0,
            uid: 0,
        };
        let mut creds_size = size_of::<libc::ucred>() as u32;
        let ret = unsafe {
            libc::getsockopt(
                self.stream
                    .as_ref()
                    .expect("Firecracker has not connected yet")
                    .as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut creds).cast::<c_void>(),
                &raw mut creds_size,
            )
        };
        if ret != 0 {
            panic!("Failed to get peer process credentials");
        }
        creds
    }

    /// Kills Firecracker if the handler panics. Deferred until Firecracker connects if it has
    /// not yet.
    pub fn install_panic_hook(&mut self) {
        if self.stream.is_none() {
            self.panic_hook_wanted = true;
            return;
        }
        let peer_creds = self.peer_process_credentials();

        let default_panic_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            let r = unsafe { libc::kill(peer_creds.pid, libc::SIGKILL) };

            if r != 0 {
                eprintln!("Failed to kill Firecracker process from panic hook");
            }

            default_panic_hook(panic_info);
        }));
    }

    /// Handles one handshake from Firecracker: keeps the memfd if one was shared and starts
    /// serving faults if a uffd came along. Returns the uffd's fd to add to the poll set.
    fn handle_handshake(&mut self) -> Option<RawFd> {
        let handshake = Handshake::receive(self.stream.as_ref().unwrap());
        if let Some(memfd) = handshake.memfd {
            println!(
                "Received guest memory memfd ({} bytes)",
                memfd.metadata().unwrap().len()
            );
            self.memfd = Some((memfd, handshake.mappings[0].page_size));
        }
        let uffd = handshake.uffd?;
        assert!(
            self.backing_file.is_some(),
            "Received a uffd but no snapshot memory file was given to populate faults from"
        );
        let mut handler = UffdHandler::new(
            handshake.mappings,
            uffd,
            self.backing_memory,
            self.backing_memory_size,
        );
        if let Some(mut pc) = self.pending_post_copy.take() {
            pc.attach(handler.page_size as u64)
                .expect("post-copy layout does not fit the mapping");
            handler.post_copy = Some(pc);
        }
        let fd = handler.uffd.as_raw_fd();
        self.uffds.insert(fd, handler);
        Some(fd)
    }

    /// Serves one connection on the control socket: reads a request, answers, closes. A `Serve`
    /// request keeps the connection as a page server until the peer closes it.
    fn handle_control_connection(&mut self) {
        let listener = self.control.as_ref().unwrap();
        let (mut conn, _) = match listener.accept() {
            Ok(conn) => conn,
            Err(err) => {
                eprintln!("Failed to accept control connection: {err}");
                return;
            }
        };
        // Exactly one JSON value, without waiting for EOF: a `Serve` peer keeps writing, and
        // keeps using the same buffered reader for its requests (unbuffered, serde reads the
        // body a byte per syscall).
        let mut reader = std::io::BufReader::new(&conn);
        let request =
            ControlRequest::deserialize(&mut serde_json::Deserializer::from_reader(&mut reader));
        let response = match request {
            Ok(ControlRequest::Serve) => {
                self.serve_pages(&mut reader, &conn);
                return;
            }
            Ok(request) => self.handle_control_request(request),
            Err(err) => ControlResponse::Done {
                success: false,
                message: format!("invalid control request: {err}"),
                zeroed_bytes: 0,
                copied_bytes: 0,
            },
        };
        let response = serde_json::to_vec(&response).unwrap();
        if let Err(err) = conn.write_all(&response) {
            eprintln!("Failed to write control response: {err}");
        }
    }

    /// `Serve`: answers `(offset, len)` requests read from `requests` with raw bytes of the memfd
    /// on `conn`, until EOF.
    fn serve_pages(&mut self, requests: &mut impl Read, mut conn: &UnixStream) {
        let Some((memfd, _)) = &self.memfd else {
            eprintln!("Serve requested but no memfd received from Firecracker");
            return;
        };
        set_socket_buffers(conn);
        let mut request = [0u8; 16];
        let mut buf = Vec::new();
        let mut served = 0u64;
        loop {
            match requests.read_exact(&mut request) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(err) => {
                    eprintln!("Serve: failed to read a request: {err}");
                    break;
                }
            }
            let offset = u64::from_le_bytes(request[..8].try_into().unwrap());
            let len = u64::from_le_bytes(request[8..].try_into().unwrap());
            buf.resize(len as usize, 0);
            // A hole reads as zero, which is what the page is.
            memfd
                .read_exact_at(&mut buf, offset)
                .expect("read from the memfd failed");
            if let Err(err) = conn.write_all(&buf) {
                eprintln!("Serve: failed to write a reply: {err}");
                break;
            }
            served += len;
        }
        println!("Served {served} bytes from the memfd");
    }

    fn handle_control_request(&mut self, request: ControlRequest) -> ControlResponse {
        let failed = |message: String| ControlResponse::Done {
            success: false,
            message,
            zeroed_bytes: 0,
            copied_bytes: 0,
        };
        match request {
            ControlRequest::Serve => unreachable!("handled by the connection"),
            ControlRequest::PostCopy {
                memory,
                source_sock,
            } => {
                let Some(target) = self.backing_file.as_ref() else {
                    return failed("no memory file to serve from".to_string());
                };
                let layout = match memory.decode() {
                    Ok(layout) => layout,
                    Err(err) => return failed(format!("invalid layout: {err}")),
                };
                let target = match target.try_clone() {
                    Ok(target) => target,
                    Err(err) => return failed(format!("cannot reopen the memory file: {err}")),
                };
                match PostCopy::new(layout, &source_sock, target) {
                    Ok(mut pc) => {
                        let pending = pc.pending.len();
                        // Before Firecracker connects the state waits for the handshake:
                        // Firecracker's own accesses at restore (kvmclock, vmgenid) must
                        // already be served post-copy, or their blocks are populated stale.
                        match self.uffds.values_mut().next() {
                            Some(handler) => {
                                if let Err(err) = pc.attach(handler.page_size as u64) {
                                    return failed(err);
                                }
                                handler.post_copy = Some(pc);
                            }
                            None => self.pending_post_copy = Some(pc),
                        }
                        ControlResponse::Done {
                            success: true,
                            message: format!("post-copy: {pending} pages to fetch"),
                            zeroed_bytes: 0,
                            copied_bytes: 0,
                        }
                    }
                    Err(err) => failed(format!(
                        "cannot connect to the source at {}: {err}",
                        source_sock.display()
                    )),
                }
            }
            ControlRequest::Copy { mem_path, memory } => {
                let Some((memfd, _page_size)) = &self.memfd else {
                    return failed("no memfd received from Firecracker".to_string());
                };
                match copy_pages(memfd, &mem_path, &memory) {
                    Ok(stats) => ControlResponse::Done {
                        success: true,
                        message: format!(
                            "copied {} runs ({} bytes from the memfd) and zeroed {} runs ({} \
                             bytes) into {}",
                            stats.copies,
                            stats.copied_bytes,
                            stats.zeroed,
                            stats.zeroed_bytes,
                            mem_path.display()
                        ),
                        zeroed_bytes: stats.zeroed_bytes,
                        copied_bytes: stats.copied_bytes,
                    },
                    Err(err) => failed(format!("copy into {} failed: {err}", mem_path.display())),
                }
            }
        }
    }

    /// Polls the `UnixStream`, the control socket and the UFFD fds in a loop.
    /// When stream is polled, a new handshake is received (a uffd to serve
    /// and/or the guest memory memfd). When the control socket is polled, an
    /// orchestrator request is served. When a uffd is polled, the page fault
    /// is handled by calling `pf_event_dispatch` with the corresponding
    /// uffd object passed in.
    pub fn run(&mut self, pf_event_dispatch: impl Fn(&mut UffdHandler)) {
        let mut pollfds = vec![];

        // Poll the stream for incoming handshakes, or the listener until Firecracker connects.
        let firecracker_fd = match (&self.stream, &self.listener) {
            (Some(stream), _) => stream.as_raw_fd(),
            (None, Some(listener)) => listener.as_raw_fd(),
            (None, None) => panic!("no connection from Firecracker and nowhere to accept one"),
        };
        pollfds.push(libc::pollfd {
            fd: firecracker_fd,
            events: libc::POLLIN,
            revents: 0,
        });
        let control_fd = self.control.as_ref().map(|listener| listener.as_raw_fd());
        if let Some(fd) = control_fd {
            pollfds.push(libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }

        loop {
            let pollfd_ptr = pollfds.as_mut_ptr();
            let pollfd_size = pollfds.len() as u64;
            // With post-copy pages pending, do not block: pull some whenever nothing is waiting.
            let pending = self.uffds.values().any(UffdHandler::post_copy_pending);
            let timeout = if pending { 0 } else { -1 };

            // # Safety:
            // Pollfds vector is valid
            let mut nready = unsafe { libc::poll(pollfd_ptr, pollfd_size, timeout) };

            if nready == -1 {
                panic!("Could not poll for events!")
            }
            if nready == 0 {
                for handler in self.uffds.values_mut() {
                    handler.post_copy_step();
                }
                continue;
            }

            for i in 0..pollfds.len() {
                if nready == 0 {
                    break;
                }
                if pollfds[i].revents & libc::POLLIN != 0 {
                    nready -= 1;
                    if self.stream.is_none()
                        && Some(pollfds[i].fd) == self.listener.as_ref().map(|l| l.as_raw_fd())
                    {
                        let (stream, _) = self
                            .listener
                            .as_ref()
                            .unwrap()
                            .accept()
                            .expect("Cannot listen on UDS socket");
                        pollfds[i].fd = stream.as_raw_fd();
                        self.stream = Some(stream);
                        if self.panic_hook_wanted {
                            self.install_panic_hook();
                        }
                    } else if Some(pollfds[i].fd) == self.stream.as_ref().map(|s| s.as_raw_fd()) {
                        if let Some(uffd_fd) = self.handle_handshake() {
                            pollfds.push(libc::pollfd {
                                fd: uffd_fd,
                                events: libc::POLLIN,
                                revents: 0,
                            });
                        }
                    } else if Some(pollfds[i].fd) == control_fd {
                        self.handle_control_connection();
                    } else {
                        // Handle one of uffd page faults
                        pf_event_dispatch(self.uffds.get_mut(&pollfds[i].fd).unwrap());
                    }
                }
            }
            // If connection is closed, we can skip the socket from being polled.
            pollfds.retain(|pollfd| pollfd.revents & (libc::POLLRDHUP | libc::POLLHUP) == 0);
        }
    }
}

/// Command line shared by the example handlers:
///
/// `handler <uffd_socket_path> [<mem_file_path>] [--control-sock <path>]`
///
/// `mem_file_path` is the snapshot memory file page faults are populated from; it is not
/// needed when the handler is started for a boot with `machine-config.mem_backend`, where no
/// uffd is handed over. `--control-sock` makes the handler listen for [`ControlRequest`]s.
#[derive(Debug)]
pub struct Args {
    pub uffd_sock_path: PathBuf,
    pub mem_file_path: Option<PathBuf>,
    pub control_sock_path: Option<PathBuf>,
}

impl Args {
    pub fn parse() -> Self {
        let mut args = std::env::args().skip(1);
        let uffd_sock_path = PathBuf::from(args.next().expect("No socket path given"));
        let mut mem_file_path = None;
        let mut control_sock_path = None;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--control-sock" => {
                    control_sock_path = Some(PathBuf::from(
                        args.next().expect("--control-sock needs a path"),
                    ));
                }
                _ if mem_file_path.is_none() => mem_file_path = Some(PathBuf::from(arg)),
                _ => panic!("Unexpected argument: {arg}"),
            }
        }
        Self {
            uffd_sock_path,
            mem_file_path,
            control_sock_path,
        }
    }

    /// Opens the memory file and binds the control socket (if given), then waits for
    /// Firecracker to connect and returns a runtime for that connection.
    pub fn into_runtime(self) -> Runtime {
        // Read-write: a post-copy destination writes fetched pages into it.
        let backing_file = self.mem_file_path.map(|path| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .expect("Cannot open memfile")
        });
        let control = self
            .control_sock_path
            .map(|path| UnixListener::bind(path).expect("Cannot bind to control socket path"));

        // Firecracker connects to the uffd socket for the handshake (a uffd to handle PFs for
        // and/or the memfd to copy snapshots from); the runtime accepts it from its loop, so
        // that control requests are served before as well (a post-copy destination gets its
        // layout before Firecracker restores). The memory file is mapped and populated here, so
        // that a handler started ahead of the restore has done that work by the time
        // Firecracker connects.
        let listener = UnixListener::bind(self.uffd_sock_path).expect("Cannot bind to socket path");
        let mapping = map_backing_file(backing_file.as_ref());

        Runtime::with_mapping(None, Some(listener), backing_file, mapping, control)
    }
}

#[cfg(test)]
mod tests {
    use std::mem::MaybeUninit;
    use std::os::unix::net::UnixListener;

    use vmm_sys_util::tempdir::TempDir;
    use vmm_sys_util::tempfile::TempFile;

    use super::*;

    unsafe impl Send for Runtime {}

    /// An anonymous-inode fd standing in for a uffd: like a uffd, it is not a regular file.
    fn fake_uffd() -> File {
        let fd = unsafe { libc::eventfd(0, 0) };
        assert!(fd >= 0);
        unsafe { File::from_raw_fd(fd) }
    }

    #[test]
    fn test_runtime() {
        let tmp_dir = TempDir::new().unwrap();
        let dummy_socket_path = tmp_dir.as_path().join("dummy_socket");
        let dummy_socket_path_clone = dummy_socket_path.clone();

        let mut uninit_runtime = Box::new(MaybeUninit::<Runtime>::uninit());
        // We will use this pointer to bypass a bunch of Rust Safety
        // for the sake of convenience.
        let runtime_ptr = uninit_runtime.as_ptr().cast::<Runtime>();

        let runtime_thread = std::thread::spawn(move || {
            let tmp_file = TempFile::new().unwrap();
            tmp_file.as_file().set_len(0x1000).unwrap();
            let dummy_mem_path = tmp_file.as_path();

            let file = File::open(dummy_mem_path).expect("Cannot open memfile");
            let listener =
                UnixListener::bind(dummy_socket_path).expect("Cannot bind to socket path");
            let (stream, _) = listener.accept().expect("Cannot listen on UDS socket");
            // Update runtime with actual runtime
            let runtime = uninit_runtime.write(Runtime::new(stream, Some(file), None));
            runtime.run(|_: &mut UffdHandler| {});
        });

        // wait for runtime thread to initialize itself
        std::thread::sleep(std::time::Duration::from_millis(100));

        let stream =
            UnixStream::connect(dummy_socket_path_clone).expect("Cannot connect to the socket");

        let dummy_memory_region = vec![GuestRegionUffdMapping {
            base_host_virt_addr: 0,
            size: 0x1000,
            offset: 0,
            page_size: 4096,
        }];
        let dummy_memory_region_json = serde_json::to_string(&dummy_memory_region).unwrap();

        let dummy_file_1 = fake_uffd();
        let dummy_fd_1 = dummy_file_1.as_raw_fd();
        stream
            .send_with_fd(dummy_memory_region_json.as_bytes(), dummy_fd_1)
            .unwrap();
        // wait for the runtime thread to process message
        std::thread::sleep(std::time::Duration::from_millis(100));
        unsafe {
            assert_eq!((*runtime_ptr).uffds.len(), 1);
        }

        let dummy_file_2 = fake_uffd();
        let dummy_fd_2 = dummy_file_2.as_raw_fd();
        stream
            .send_with_fd(dummy_memory_region_json.as_bytes(), dummy_fd_2)
            .unwrap();
        // wait for the runtime thread to process message
        std::thread::sleep(std::time::Duration::from_millis(100));
        unsafe {
            assert_eq!((*runtime_ptr).uffds.len(), 2);
        }

        // there is no way to properly stop runtime, so
        // we send a message with an incorrect memory region
        // to cause runtime thread to panic
        let error_memory_region = vec![GuestRegionUffdMapping {
            base_host_virt_addr: 0,
            size: 0,
            offset: 0,
            page_size: 4096,
        }];
        let error_memory_region_json = serde_json::to_string(&error_memory_region).unwrap();
        stream
            .send_with_fd(error_memory_region_json.as_bytes(), dummy_fd_2)
            .unwrap();

        runtime_thread.join().unwrap_err();
    }

    fn memfd_with_pattern(pages: usize) -> File {
        let name = std::ffi::CString::new("test").unwrap();
        let fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
        assert!(fd >= 0);
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len((pages * 4096) as u64).unwrap();
        for page in 0..pages {
            file.write_all_at(&vec![page as u8 + 1; 4096], (page * 4096) as u64)
                .unwrap();
        }
        file
    }

    #[test]
    fn test_handshake_classifies_fds() {
        let memfd = memfd_with_pattern(2);
        let body = serde_json::to_string(&vec![GuestRegionUffdMapping {
            base_host_virt_addr: 0,
            size: 0x2000,
            offset: 0,
            page_size: 4096,
        }])
        .unwrap();

        // A single memfd (boot): no uffd.
        let handshake = Handshake::from_message(&body, vec![memfd.try_clone().unwrap()]);
        assert!(handshake.uffd.is_none());
        assert!(handshake.memfd.is_some());

        // A single non-memfd (plain UFFD restore) is taken to be the uffd.
        let plain = fake_uffd();
        let handshake = Handshake::from_message(&body, vec![plain.try_clone().unwrap()]);
        assert!(handshake.uffd.is_some());
        assert!(handshake.memfd.is_none());

        // Two fds: uffd first, memfd last.
        let handshake = Handshake::from_message(
            &body,
            vec![plain.try_clone().unwrap(), memfd.try_clone().unwrap()],
        );
        assert!(handshake.uffd.is_some());
        assert!(handshake.memfd.is_some());
    }

    fn roaring_b64(pages: impl IntoIterator<Item = u64>) -> String {
        let set: RoaringBitmap = pages
            .into_iter()
            .map(|p| u32::try_from(p).unwrap())
            .collect();
        let mut bytes = Vec::new();
        set.serialize_into(&mut bytes).unwrap();
        base64_encode(&bytes)
    }

    /// A layout for `total_pages` 4 KiB pages: the given pages authoritative, the given pages
    /// zero, encoded as Firecracker would.
    fn layout(total_pages: u64, authoritative: &[u64], zero: &[u64]) -> SnapshotMemoryLayout {
        SnapshotMemoryLayout {
            total_size: total_pages * 4096,
            page_size: 4096,
            bitmap_encoding: "roaring".to_string(),
            pages_to_copy: roaring_b64(authoritative.iter().copied()),
            pages_to_discard: roaring_b64(zero.iter().copied()),
        }
    }

    #[test]
    fn test_base64_round_trip() {
        for input in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
            &[0x03, 0x01],
            &[0xff; 33],
        ] {
            let encoded = base64_encode(input);
            assert_eq!(encoded.len() % 4, 0);
            assert_eq!(
                base64_decode(encoded.as_bytes()).unwrap(),
                input,
                "{encoded}"
            );
        }
        // RFC 4648 test vectors and the documentation example.
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(&[0x03, 0x10]), "AxA=");
        assert_eq!(base64_decode(b"AxA=").unwrap(), [0x03, 0x10]);
        // Malformed input.
        for bad in ["A", "AwE", "Aw=E", "!!!!", "A===", "Aw==Aw=="] {
            assert!(base64_decode(bad.as_bytes()).is_none(), "{bad}");
        }
    }

    #[test]
    fn test_decode_and_runs() {
        let runs = |l: &SnapshotMemoryLayout| {
            l.decode()
                .unwrap()
                .runs()
                .iter()
                .map(|(c, r)| (*c, r.offset / 4096, r.len / 4096))
                .collect::<Vec<_>>()
        };
        use PageClass::*;
        // The documentation example: 64 KiB guest, pages 0, 1 and 12 authoritative, pages 8..12
        // zero, as Firecracker's unit test serialises it.
        let doc = SnapshotMemoryLayout {
            total_size: 65536,
            page_size: 4096,
            bitmap_encoding: "roaring".to_string(),
            pages_to_copy: "OjAAAAEAAAAAAAIAEAAAAAAAAQAMAA==".to_string(),
            pages_to_discard: "OjAAAAEAAAAAAAMAEAAAAAgACQAKAAsA".to_string(),
        };
        assert_eq!(
            runs(&doc),
            vec![(Authoritative, 0, 2), (Zero, 8, 4), (Authoritative, 12, 1)]
        );
        let decoded = doc.decode().unwrap();
        assert_eq!(decoded.class(0), Authoritative);
        assert_eq!(decoded.class(2 * 4096), Unchanged);
        assert_eq!(decoded.class(9 * 4096), Zero);
        assert_eq!(decoded.class(15 * 4096), Unchanged);
        // The encoding field defaults when absent.
        let no_encoding: SnapshotMemoryLayout = serde_json::from_str(
            r#"{"total_size":65536,"page_size":4096,"pages_to_copy":"OjAAAAEAAAAAAAIAEAAAAAAAAQAMAA==","pages_to_discard":"OjAAAAEAAAAAAAMAEAAAAAgACQAKAAsA"}"#,
        )
        .unwrap();
        assert_eq!(runs(&no_encoding), runs(&doc));

        // Odd page counts, empty layouts, runs across 65536-page container boundaries.
        assert_eq!(
            runs(&layout(13, &[0, 12], &[])),
            vec![(Authoritative, 0, 1), (Authoritative, 12, 1)]
        );
        assert!(runs(&layout(13, &[], &[])).is_empty());
        assert_eq!(
            runs(&layout(20, &[], &(0..20).collect::<Vec<_>>())),
            vec![(Zero, 0, 20)]
        );
        assert_eq!(
            runs(&layout(20, &(3..19).collect::<Vec<_>>(), &[0, 19])),
            vec![(Zero, 0, 1), (Authoritative, 3, 16), (Zero, 19, 1)]
        );
        // Adjacent runs of different classes stay separate.
        assert_eq!(
            runs(&layout(4, &[0, 1], &[2, 3])),
            vec![(Authoritative, 0, 2), (Zero, 2, 2)]
        );
        // A large unplugged tail (512 MiB here, two containers) is one run.
        let big = layout(1 << 18, &[0], &(1 << 17..1 << 18).collect::<Vec<_>>());
        assert_eq!(
            runs(&big),
            vec![(Authoritative, 0, 1), (Zero, 1 << 17, 1 << 17)]
        );

        // Rejected: pages past the end of the file, overlapping classes, an unknown encoding,
        // bad base64, bad Roaring.
        let mut bad = layout(1, &[0], &[]);
        bad.pages_to_copy = roaring_b64([1]);
        bad.decode().unwrap_err();
        let mut bad = layout(4, &[0], &[]);
        bad.pages_to_discard = roaring_b64([0]);
        bad.decode().unwrap_err();
        let mut bad = layout(4, &[0], &[]);
        bad.bitmap_encoding = "packbits".to_string();
        bad.decode().unwrap_err();
        let mut bad = layout(4, &[0], &[]);
        bad.pages_to_discard = "!!".to_string();
        bad.decode().unwrap_err();
        let mut bad = layout(4, &[0], &[]);
        bad.pages_to_discard = base64_encode(&[1, 2, 3]);
        bad.decode().unwrap_err();
    }

    #[test]
    fn test_copy_pages_diffs() {
        // A booted microVM: the first diff into a fresh file is against a zero base.
        // Authoritative pages from the memfd, zero chunks zeroed, unchanged pages left zero.
        let memfd = memfd_with_pattern(4);
        let tmp_dir = TempDir::new().unwrap();
        let mem_path = tmp_dir.as_path().join("mem");

        // Pages 0..2 authoritative, page 3 a zero chunk (unplugged, say; the memfd has data
        // there that the guest can never see), page 2 untouched-since-boot: unchanged.
        let diff = layout(4, &[0, 1], &[3]);
        let stats = copy_pages(&memfd, &mem_path, &diff).unwrap();
        assert_eq!(stats.copies, 1);
        assert_eq!(stats.copied_bytes, 2 * 4096);
        assert_eq!(stats.zeroed_bytes, 4096);
        let mut expected = vec![1u8; 4096];
        expected.extend(std::iter::repeat_n(2u8, 4096));
        expected.extend(std::iter::repeat_n(0u8, 2 * 4096));
        assert_eq!(std::fs::read(&mem_path).unwrap(), expected);

        // Modify page 1 in the "guest" and merge a diff that contains only page 1.
        memfd.write_all_at(&[0xAA; 4096], 4096).unwrap();
        let diff = layout(4, &[1], &[]);
        copy_pages(&memfd, &mem_path, &diff).unwrap();
        expected[4096..2 * 4096].fill(0xAA);
        assert_eq!(std::fs::read(&mem_path).unwrap(), expected);

        // A diff into a fresh file yields a sparse file of total_size with only that page.
        let diff_path = tmp_dir.as_path().join("diff");
        copy_pages(&memfd, &diff_path, &diff).unwrap();
        let mut sparse = vec![0u8; 4 * 4096];
        sparse[4096..2 * 4096].fill(0xAA);
        assert_eq!(std::fs::read(&diff_path).unwrap(), sparse);

        // A diff with a zero chunk (balloon released page 0) writes zeros over the old content.
        let diff = layout(4, &[], &[0]);
        let stats = copy_pages(&memfd, &mem_path, &diff).unwrap();
        assert_eq!((stats.copies, stats.zeroed), (0, 1));
        expected[..4096].fill(0);
        assert_eq!(std::fs::read(&mem_path).unwrap(), expected);

        // Merged into a garbage-filled target, only the pages in the diff are touched; an empty
        // layout touches nothing.
        let merged = tmp_dir.as_path().join("merged");
        std::fs::write(&merged, vec![0xCC; 4 * 4096]).unwrap();
        let diff = layout(4, &[2], &[0]);
        let stats = copy_pages(&memfd, &merged, &diff).unwrap();
        assert_eq!((stats.copies, stats.zeroed), (1, 1));
        let mut expected = vec![0xCCu8; 4 * 4096];
        expected[..4096].fill(0);
        expected[2 * 4096..3 * 4096].fill(3);
        assert_eq!(std::fs::read(&merged).unwrap(), expected);
        let stats = copy_pages(&memfd, &merged, &layout(4, &[], &[])).unwrap();
        assert_eq!(stats, CopyStats::default());
        assert_eq!(std::fs::read(&merged).unwrap(), expected);
    }

    #[test]
    fn test_copy_pages_interleaved_runs() {
        // 24 pages: pages 8..16 (one whole bitmap byte) zero, pages 3 and 20 authoritative. The
        // zero run and the authoritative runs interleave.
        let memfd = memfd_with_pattern(24);
        let tmp_dir = TempDir::new().unwrap();
        let mem_path = tmp_dir.as_path().join("mem");
        std::fs::write(&mem_path, vec![0xCC; 24 * 4096]).unwrap();
        let diff = layout(24, &[3, 20], &(8..16).collect::<Vec<_>>());
        let stats = copy_pages(&memfd, &mem_path, &diff).unwrap();
        assert_eq!(
            (stats.copies, stats.zeroed, stats.zeroed_bytes),
            (2, 1, 8 * 4096)
        );
        let mut expected = vec![0xCCu8; 24 * 4096];
        expected[3 * 4096..4 * 4096].fill(4);
        expected[8 * 4096..16 * 4096].fill(0);
        expected[20 * 4096..21 * 4096].fill(21);
        assert_eq!(std::fs::read(&mem_path).unwrap(), expected);
    }

    #[test]
    fn test_control_socket_copy() {
        let tmp_dir = TempDir::new().unwrap();
        let fc_sock = tmp_dir.as_path().join("fc.sock");
        let control_sock = tmp_dir.as_path().join("control.sock");
        let mem_path = tmp_dir.as_path().join("mem");

        let memfd = memfd_with_pattern(2);
        let body = serde_json::to_string(&vec![GuestRegionUffdMapping {
            base_host_virt_addr: 0,
            size: 0x2000,
            offset: 0,
            page_size: 4096,
        }])
        .unwrap();

        // A handler started for a boot: no memory file, control socket, only a memfd arrives.
        let fc_listener = UnixListener::bind(&fc_sock).unwrap();
        let control_listener = UnixListener::bind(&control_sock).unwrap();
        let fc_stream = UnixStream::connect(&fc_sock).unwrap();
        let (stream, _) = fc_listener.accept().unwrap();
        std::thread::spawn(move || {
            let mut runtime = Runtime::new(stream, None, Some(control_listener));
            runtime.run(|_: &mut UffdHandler| panic!("no faults expected"));
        });

        // Before the handshake, a copy must fail cleanly.
        let request = serde_json::to_vec(&ControlRequest::Copy {
            mem_path: mem_path.clone(),
            memory: layout(2, &[0, 1], &[]),
        })
        .unwrap();
        let send = |request: &[u8]| {
            let mut conn = UnixStream::connect(&control_sock).unwrap();
            conn.write_all(request).unwrap();
            conn.shutdown(std::net::Shutdown::Write).unwrap();
            let mut raw = Vec::new();
            conn.read_to_end(&mut raw).unwrap();
            serde_json::from_slice::<ControlResponse>(&raw).unwrap()
        };
        let ControlResponse::Done { success, .. } = send(&request);
        assert!(!success);

        fc_stream
            .send_with_fd(body.as_bytes(), memfd.as_raw_fd())
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let ControlResponse::Done {
            success,
            message,
            zeroed_bytes,
            copied_bytes,
        } = send(&request);
        assert!(success, "{message}");
        assert_eq!(zeroed_bytes, 0);
        assert_eq!(copied_bytes, 2 * 4096);
        let mut expected = vec![1u8; 4096];
        expected.extend(std::iter::repeat_n(2u8, 4096));
        assert_eq!(std::fs::read(&mem_path).unwrap(), expected);
    }
}
