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

// These are the same with the ones used in src/vmm (`SnapshotMemoryLayout`, `MemoryRange`).
/// A page-aligned byte range in guest memory file / memfd offset space.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct MemoryRange {
    pub offset: u64,
    pub len: u64,
}

/// The `memory` object returned by `PUT /snapshot/create` and `PUT /snapshot/dirty-pages`
/// when a memory backend is attached: which pages of the memfd make up the snapshot.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotMemoryLayout {
    /// Size of a full guest memory file.
    pub total_size: u64,
    /// Granularity of `pages`, in bytes.
    pub page_size: u64,
    /// Standard base64 of a bitmap with one bit per `page_size` bytes of the memory file: byte
    /// `i`, bit `b` (least significant first) is the page at offset `(8 * i + b) * page_size`.
    /// Set pages are to be copied into the memory file at the same offset. The bitmap covers
    /// the file up to the end of the last plugged slot; pages past its end are clear. Absent for
    /// a full snapshot, which consists of every page outside `unplugged`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<String>,
    /// Bytes to zero in the memory file (unplugged virtio-mem slots).
    pub unplugged: Vec<MemoryRange>,
}

impl SnapshotMemoryLayout {
    /// Decodes `pages`, if present. Firecracker emits standard, padded base64 (RFC 4648 §4).
    pub fn decode_pages(&self) -> Result<Option<Vec<u8>>, std::io::Error> {
        self.pages
            .as_ref()
            .map(|pages| {
                base64_decode(pages.as_bytes()).ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "invalid base64 in `pages`",
                    )
                })
            })
            .transpose()
    }

    /// The runs of consecutive pages to copy, as `{offset, len}` byte ranges in file offset
    /// order. For a full snapshot (no `pages`) that is everything outside `unplugged`.
    pub fn set_runs(&self) -> Result<Vec<MemoryRange>, std::io::Error> {
        let Some(bitmap) = self.decode_pages()? else {
            let mut runs = Vec::new();
            let mut cursor = 0u64;
            for hole in &self.unplugged {
                if hole.offset > cursor {
                    runs.push(MemoryRange {
                        offset: cursor,
                        len: hole.offset - cursor,
                    });
                }
                cursor = hole.offset + hole.len;
            }
            if self.total_size > cursor {
                runs.push(MemoryRange {
                    offset: cursor,
                    len: self.total_size - cursor,
                });
            }
            return Ok(runs);
        };
        let num_pages = self.total_size.div_ceil(self.page_size);
        if (bitmap.len() as u64) > num_pages.div_ceil(8) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "`pages` has {} bytes, more than the {} pages of the file need",
                    bitmap.len(),
                    num_pages,
                ),
            ));
        }
        let mut runs: Vec<MemoryRange> = Vec::new();
        // The bitmap ends with the last plugged slot; whatever lies past it is clear.
        for page in 0..num_pages.min(bitmap.len() as u64 * 8) {
            let set = bitmap[(page / 8) as usize] & (1 << (page % 8)) != 0;
            if !set {
                continue;
            }
            let offset = page * self.page_size;
            match runs.last_mut() {
                Some(last) if last.offset + last.len == offset => last.len += self.page_size,
                _ => runs.push(MemoryRange {
                    offset,
                    len: self.page_size,
                }),
            }
        }
        Ok(runs)
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
    /// Copy the set pages of `memory.pages` from the memfd into `mem_path` (created at
    /// `memory.total_size` if it does not exist, merged into otherwise) and zero
    /// `memory.unplugged` in it.
    Copy {
        mem_path: PathBuf,
        memory: SnapshotMemoryLayout,
    },
}

/// Reply to a [`ControlRequest`].
#[derive(Debug, Serialize, Deserialize)]
pub enum ControlResponse {
    Done { success: bool, message: String },
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

/// Which pages of the guest memory memfd hold authoritative content.
///
/// After a restore with a memory backend, the memfd starts out empty: a page only gets content
/// once the handler has served a fault for it (`UFFDIO_COPY`). For every other page the guest
/// memory content is still what the snapshot file says at the same offset. Firecracker's own
/// `dump` reads through the mapping and therefore faults such pages in; a backend copying from
/// the memfd has to source them from the snapshot file itself instead. A range the handler has
/// unregistered (balloon `remove`) is authoritative in the memfd as well: whatever the kernel
/// puts there afterwards (zero pages) is what the guest sees.
///
/// A handler started for a boot has no snapshot file and the memfd is authoritative everywhere.
#[derive(Debug, Clone)]
pub struct PopulatedPages {
    page_size: usize,
    pages: Vec<bool>,
}

impl PopulatedPages {
    pub fn new(total_size: usize, page_size: usize) -> Self {
        Self {
            page_size,
            pages: vec![false; total_size.div_ceil(page_size)],
        }
    }

    /// Marks `[offset, offset + len)` (memfd offsets) as populated.
    pub fn mark(&mut self, offset: u64, len: usize) {
        let first = offset as usize / self.page_size;
        let last = (offset as usize + len).div_ceil(self.page_size);
        for page in first..last.min(self.pages.len()) {
            self.pages[page] = true;
        }
    }

    /// Whether the page containing memfd `offset` is populated.
    pub fn contains(&self, offset: u64) -> bool {
        self.pages
            .get(offset as usize / self.page_size)
            .copied()
            .unwrap_or(false)
    }
}

#[derive(Debug)]
pub struct UffdHandler {
    pub mem_regions: Vec<GuestRegionUffdMapping>,
    pub page_size: usize,
    backing_buffer: *const u8,
    uffd: Uffd,
    /// Pages of the memfd this handler has populated (or given up on).
    pub populated: PopulatedPages,
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
            populated: PopulatedPages::new(memsize, page_size),
            mem_regions: mappings,
            page_size,
            backing_buffer,
            uffd,
        }
    }

    /// Memfd offset of the host virtual address `addr`, if it belongs to a region.
    fn memfd_offset(&self, addr: u64) -> Option<u64> {
        self.mem_regions
            .iter()
            .find(|region| region.contains(addr))
            .map(|region| region.offset + (addr - region.base_host_virt_addr))
    }

    pub fn read_event(&mut self) -> Result<Option<Event>, Error> {
        self.uffd.read_event()
    }

    /// Handles a uffd `remove` event for `[start, end)`: Firecracker has discarded the range
    /// (balloon, virtio-mem), so it now reads as zero. The range is unregistered, after which the
    /// kernel serves zero pages for it without us, and marked as populated: the memfd is
    /// authoritative for it from now on.
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
        // From now on the kernel serves this range on its own; the memfd is authoritative for it.
        if let Some(offset) = self.memfd_offset(start as u64) {
            self.populated.mark(offset, len);
        }
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
            return self.populate_from_file(&region, fault_page_addr, len);
        }

        panic!(
            "Could not find addr: {:?} within guest region mappings.",
            addr
        );
    }

    fn populate_from_file(
        &mut self,
        region: &GuestRegionUffdMapping,
        dst: u64,
        len: usize,
    ) -> bool {
        let offset = dst - region.base_host_virt_addr;
        let src = self.backing_buffer as u64 + region.offset + offset;

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
        self.populated.mark(region.offset + offset, len);

        true
    }
}

/// Where the bytes of a snapshot range come from.
#[derive(Debug, Clone, Copy)]
pub struct MemorySource<'a> {
    /// The guest memory memfd received from Firecracker.
    pub memfd: &'a File,
    /// Backing page size, used to walk [`PopulatedPages`].
    pub page_size: usize,
    /// The snapshot file the handler populates faults from, and which pages of the memfd it has
    /// populated so far. `None` for a booted microVM: the memfd is authoritative everywhere.
    pub backing: Option<(&'a File, &'a [&'a PopulatedPages])>,
}

impl MemorySource<'_> {
    /// Whether the page at memfd `offset` has to be read from the memfd (`true`) or from the
    /// snapshot file (`false`).
    fn in_memfd(&self, offset: u64) -> bool {
        match self.backing {
            None => true,
            Some((_, populated)) => populated.iter().any(|p| p.contains(offset)),
        }
    }
}

/// Copies the set pages of `layout.pages` from `memfd` into the file at `mem_path` at the same
/// offsets and zeroes `layout.unplugged` there. The file is created at `layout.total_size` if it
/// does not exist; an existing file (a full snapshot a diff is merged into) is left at its size.
///
/// This is what turns the `memory` object of a `PUT /snapshot/create` response into a memory
/// file byte-for-byte identical to the one Firecracker would have written. Returns the number of
/// copy operations performed.
pub fn copy_pages(
    source: MemorySource<'_>,
    mem_path: &Path,
    layout: &SnapshotMemoryLayout,
) -> Result<usize, std::io::Error> {
    let target = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(mem_path)?;
    if target.metadata()?.len() < layout.total_size {
        target.set_len(layout.total_size)?;
    }

    let mut copies = 0;
    for range in layout.set_runs()? {
        // Split the run into pieces that come from the same file. `pages` is host-page (4 KiB)
        // granular even when the backing page is larger, so walk backing page *boundaries* rather
        // than stepping `page_size` from the run start.
        let page_size = source.page_size as u64;
        let end = range.offset + range.len;
        let mut start = range.offset;
        while start < end {
            let from_memfd = source.in_memfd(start);
            let mut run_end = (start / page_size + 1) * page_size;
            while run_end < end && source.in_memfd(run_end) == from_memfd {
                run_end += page_size;
            }
            let run_end = run_end.min(end);
            let run = MemoryRange {
                offset: start,
                len: run_end - start,
            };
            let src = if from_memfd {
                source.memfd
            } else {
                source.backing.unwrap().0
            };
            copy_range(src, &target, &run)?;
            copies += 1;
            start = run_end;
        }
    }
    // Firecracker never sets bits inside `unplugged`; zeroing last keeps the result right even
    // for a peer-made layout that does.
    for range in &layout.unplugged {
        zero_range(&target, range)?;
    }
    Ok(copies)
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

/// Zeroes `range` in `file`, by punching a hole where the file system supports it and by
/// writing zeros otherwise.
fn zero_range(file: &File, range: &MemoryRange) -> Result<(), std::io::Error> {
    // SAFETY: the fd is valid; fallocate does not touch our memory.
    let ret = unsafe {
        libc::fallocate(
            file.as_raw_fd(),
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            range.offset.cast_signed(),
            range.len.cast_signed(),
        )
    };
    if ret == 0 {
        return Ok(());
    }
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
    stream: UnixStream,
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
}

impl Runtime {
    /// Creates a runtime serving faults for the connection `stream` from `backing_file`, if
    /// given, and answering control requests on `control`, if given.
    pub fn new(
        stream: UnixStream,
        backing_file: Option<File>,
        control: Option<UnixListener>,
    ) -> Self {
        let (backing_memory, backing_memory_size) = match &backing_file {
            Some(backing_file) => {
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
            None => (ptr::null_mut(), 0),
        };

        Self {
            stream,
            backing_file,
            backing_memory,
            backing_memory_size,
            uffds: HashMap::default(),
            memfd: None,
            control,
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
                self.stream.as_raw_fd(),
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

    pub fn install_panic_hook(&self) {
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
        let handshake = Handshake::receive(&self.stream);
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
        let handler = UffdHandler::new(
            handshake.mappings,
            uffd,
            self.backing_memory,
            self.backing_memory_size,
        );
        let fd = handler.uffd.as_raw_fd();
        self.uffds.insert(fd, handler);
        Some(fd)
    }

    /// Serves one connection on the control socket: reads a request until EOF, answers, closes.
    fn handle_control_connection(&mut self) {
        let listener = self.control.as_ref().unwrap();
        let (mut conn, _) = match listener.accept() {
            Ok(conn) => conn,
            Err(err) => {
                eprintln!("Failed to accept control connection: {err}");
                return;
            }
        };
        let mut raw = Vec::new();
        if let Err(err) = conn.read_to_end(&mut raw) {
            eprintln!("Failed to read control request: {err}");
            return;
        }
        let response = match serde_json::from_slice::<ControlRequest>(&raw) {
            Ok(request) => self.handle_control_request(request),
            Err(err) => ControlResponse::Done {
                success: false,
                message: format!("invalid control request: {err}"),
            },
        };
        let response = serde_json::to_vec(&response).unwrap();
        if let Err(err) = conn.write_all(&response) {
            eprintln!("Failed to write control response: {err}");
        }
    }

    fn handle_control_request(&mut self, request: ControlRequest) -> ControlResponse {
        match request {
            ControlRequest::Copy { mem_path, memory } => {
                let Some((memfd, page_size)) = &self.memfd else {
                    return ControlResponse::Done {
                        success: false,
                        message: "no memfd received from Firecracker".to_string(),
                    };
                };
                let populated: Vec<&PopulatedPages> =
                    self.uffds.values().map(|h| &h.populated).collect();
                let source = MemorySource {
                    memfd,
                    page_size: *page_size,
                    backing: self
                        .backing_file
                        .as_ref()
                        .map(|file| (file, populated.as_slice())),
                };
                match copy_pages(source, &mem_path, &memory) {
                    Ok(copies) => ControlResponse::Done {
                        success: true,
                        message: format!(
                            "copied {copies} runs and zeroed {} ranges into {}",
                            memory.unplugged.len(),
                            mem_path.display()
                        ),
                    },
                    Err(err) => ControlResponse::Done {
                        success: false,
                        message: format!("copy into {} failed: {err}", mem_path.display()),
                    },
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

        // Poll the stream for incoming handshakes
        pollfds.push(libc::pollfd {
            fd: self.stream.as_raw_fd(),
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

            // # Safety:
            // Pollfds vector is valid
            let mut nready = unsafe { libc::poll(pollfd_ptr, pollfd_size, -1) };

            if nready == -1 {
                panic!("Could not poll for events!")
            }

            for i in 0..pollfds.len() {
                if nready == 0 {
                    break;
                }
                if pollfds[i].revents & libc::POLLIN != 0 {
                    nready -= 1;
                    if pollfds[i].fd == self.stream.as_raw_fd() {
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
        let backing_file = self
            .mem_file_path
            .map(|path| File::open(path).expect("Cannot open memfile"));
        let control = self
            .control_sock_path
            .map(|path| UnixListener::bind(path).expect("Cannot bind to control socket path"));

        // Get the handshake from UDS. We'll use the uffd to handle PFs for Firecracker and/or
        // keep the memfd to copy snapshots from.
        let listener = UnixListener::bind(self.uffd_sock_path).expect("Cannot bind to socket path");
        let (stream, _) = listener.accept().expect("Cannot listen on UDS socket");

        Runtime::new(stream, backing_file, control)
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

    /// A layout for `total_pages` 4 KiB pages with the given page indices set.
    fn layout(total_pages: u64, set: &[u64], unplugged: Vec<MemoryRange>) -> SnapshotMemoryLayout {
        layout_with_page_size(4096, total_pages, set, unplugged)
    }

    fn layout_with_page_size(
        page_size: u64,
        total_pages: u64,
        set: &[u64],
        unplugged: Vec<MemoryRange>,
    ) -> SnapshotMemoryLayout {
        let mut bitmap = vec![0u8; total_pages.div_ceil(8) as usize];
        for &page in set {
            bitmap[(page / 8) as usize] |= 1 << (page % 8);
        }
        SnapshotMemoryLayout {
            total_size: total_pages * page_size,
            page_size,
            pages: Some(base64_encode(&bitmap)),
            unplugged,
        }
    }

    /// A full layout for `total_pages` 4 KiB pages: no bitmap.
    fn full_layout(total_pages: u64, unplugged: Vec<MemoryRange>) -> SnapshotMemoryLayout {
        SnapshotMemoryLayout {
            total_size: total_pages * 4096,
            page_size: 4096,
            pages: None,
            unplugged,
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
    fn test_set_runs() {
        let runs = |l: &SnapshotMemoryLayout| {
            l.set_runs()
                .unwrap()
                .iter()
                .map(|r| (r.offset, r.len))
                .collect::<Vec<_>>()
        };
        // 64 KiB guest, pages 0, 1 and 12 set: the documentation example.
        let doc = SnapshotMemoryLayout {
            total_size: 65536,
            page_size: 4096,
            pages: Some("AxA=".to_string()),
            unplugged: vec![],
        };
        assert_eq!(runs(&doc), vec![(0, 8192), (49152, 4096)]);
        // Page count not a multiple of 8, last page set.
        let l = layout(13, &[0, 12], vec![]);
        assert_eq!(runs(&l), vec![(0, 4096), (12 * 4096, 4096)]);
        // Empty.
        assert!(layout(13, &[], vec![]).set_runs().unwrap().is_empty());
        // A bitmap shorter than the file (it ends with the last plugged slot): pages past its
        // end are clear.
        let short = SnapshotMemoryLayout {
            total_size: 65536,
            page_size: 4096,
            pages: Some("Aw==".to_string()),
            unplugged: vec![],
        };
        assert_eq!(runs(&short), vec![(0, 8192)]);
        let empty = SnapshotMemoryLayout {
            total_size: 65536,
            page_size: 4096,
            pages: Some(String::new()),
            unplugged: vec![],
        };
        assert!(empty.set_runs().unwrap().is_empty());
        // A bitmap longer than the file is rejected.
        let long = SnapshotMemoryLayout {
            total_size: 4096,
            page_size: 4096,
            pages: Some("AxA=".to_string()),
            unplugged: vec![],
        };
        long.set_runs().unwrap_err();

        // Full: everything outside `unplugged`, as runs.
        assert_eq!(runs(&full_layout(4, vec![])), vec![(0, 4 * 4096)]);
        let hole = |page: u64, pages: u64| MemoryRange {
            offset: page * 4096,
            len: pages * 4096,
        };
        assert_eq!(
            runs(&full_layout(8, vec![hole(2, 1), hole(5, 3)])),
            vec![(0, 2 * 4096), (3 * 4096, 2 * 4096)]
        );
        assert_eq!(
            runs(&full_layout(8, vec![hole(0, 2)])),
            vec![(2 * 4096, 6 * 4096)]
        );
        assert!(
            full_layout(8, vec![hole(0, 8)])
                .set_runs()
                .unwrap()
                .is_empty()
        );
        // `pages` absent in JSON is a full layout; present is a diff.
        let l: SnapshotMemoryLayout = serde_json::from_str(
            r#"{"total_size":16384,"page_size":4096,"unplugged":[{"offset":12288,"len":4096}]}"#,
        )
        .unwrap();
        assert!(l.pages.is_none());
        assert_eq!(runs(&l), vec![(0, 3 * 4096)]);
    }

    #[test]
    fn test_copy_pages_full_then_diff() {
        let memfd = memfd_with_pattern(4);
        let tmp_dir = TempDir::new().unwrap();
        let mem_path = tmp_dir.as_path().join("mem");

        // Full: pages 0..2 plugged, page 3 unplugged (must be zero even though the memfd has data).
        let full = full_layout(
            4,
            vec![MemoryRange {
                offset: 3 * 4096,
                len: 4096,
            }],
        );
        let source = MemorySource {
            memfd: &memfd,
            page_size: 4096,
            backing: None,
        };
        assert_eq!(copy_pages(source, &mem_path, &full).unwrap(), 1);
        let mut expected = Vec::new();
        for page in 0..3u8 {
            expected.extend(std::iter::repeat_n(page + 1, 4096));
        }
        expected.extend(std::iter::repeat_n(0, 4096));
        assert_eq!(std::fs::read(&mem_path).unwrap(), expected);

        // Modify page 1 in the "guest" and merge a diff that contains only page 1.
        memfd.write_all_at(&[0xAA; 4096], 4096).unwrap();
        let diff = layout(4, &[1], vec![]);
        copy_pages(source, &mem_path, &diff).unwrap();
        expected[4096..2 * 4096].fill(0xAA);
        assert_eq!(std::fs::read(&mem_path).unwrap(), expected);

        // A diff into a fresh file yields a sparse file of total_size with only that page.
        let diff_path = tmp_dir.as_path().join("diff");
        copy_pages(source, &diff_path, &diff).unwrap();
        let mut sparse = vec![0u8; 4 * 4096];
        sparse[4096..2 * 4096].fill(0xAA);
        assert_eq!(std::fs::read(&diff_path).unwrap(), sparse);

        // `unplugged` wins over a set bit in the same range (Firecracker never produces that,
        // but zeroing last makes the copy robust to it).
        let diff = layout(
            4,
            &[2],
            vec![MemoryRange {
                offset: 2 * 4096,
                len: 2 * 4096,
            }],
        );
        copy_pages(source, &mem_path, &diff).unwrap();
        expected[2 * 4096..].fill(0);
        assert_eq!(std::fs::read(&mem_path).unwrap(), expected);
    }

    #[test]
    fn test_copy_pages_unpopulated_pages_come_from_snapshot_file() {
        // After a restore, only the pages the handler has faulted in are in the memfd; the
        // rest of the guest memory is still in the snapshot file.
        let tmp_dir = TempDir::new().unwrap();
        let snapshot_path = tmp_dir.as_path().join("snapshot_mem");
        let mut snapshot = vec![0u8; 4 * 4096];
        for page in 0..4 {
            snapshot[page * 4096..(page + 1) * 4096].fill(0x10 + page as u8);
        }
        std::fs::write(&snapshot_path, &snapshot).unwrap();
        let snapshot_file = File::open(&snapshot_path).unwrap();

        // The memfd has pages 1 and 2 populated (page 2 modified by the guest), 0 and 3 not.
        let name = std::ffi::CString::new("test").unwrap();
        let fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
        let memfd = unsafe { File::from_raw_fd(fd) };
        memfd.set_len(4 * 4096).unwrap();
        memfd.write_all_at(&snapshot[4096..2 * 4096], 4096).unwrap();
        memfd.write_all_at(&[0xEE; 4096], 2 * 4096).unwrap();
        let mut populated = PopulatedPages::new(4 * 4096, 4096);
        populated.mark(4096, 2 * 4096);

        let populated_refs = [&populated];
        let source = MemorySource {
            memfd: &memfd,
            page_size: 4096,
            backing: Some((&snapshot_file, &populated_refs)),
        };
        let mem_path = tmp_dir.as_path().join("mem");
        let full = full_layout(4, vec![]);
        // Pages 0 and 3 from the snapshot file, 1 and 2 from the memfd: three runs.
        assert_eq!(copy_pages(source, &mem_path, &full).unwrap(), 3);

        let mut expected = snapshot.clone();
        expected[2 * 4096..3 * 4096].fill(0xEE);
        assert_eq!(std::fs::read(&mem_path).unwrap(), expected);

        // Without a backing file (boot), the memfd is taken as is: holes are zeros.
        let boot_source = MemorySource {
            memfd: &memfd,
            page_size: 4096,
            backing: None,
        };
        let boot_path = tmp_dir.as_path().join("mem_boot");
        assert_eq!(copy_pages(boot_source, &boot_path, &full).unwrap(), 1);
        let mut expected = vec![0u8; 4 * 4096];
        expected[4096..2 * 4096].copy_from_slice(&snapshot[4096..2 * 4096]);
        expected[2 * 4096..3 * 4096].fill(0xEE);
        assert_eq!(std::fs::read(&boot_path).unwrap(), expected);
    }

    #[test]
    fn test_copy_pages_unaligned_run_across_backing_pages() {
        // Dirty ranges are 4 KiB granular while the backing page (hugetlbfs) may be larger:
        // a range starting in the middle of a backing page must still switch source at the
        // next backing page boundary.
        const PAGE: usize = 8192;
        let tmp_dir = TempDir::new().unwrap();
        let snapshot_path = tmp_dir.as_path().join("snapshot_mem");
        let mut snapshot = vec![0u8; 3 * PAGE];
        for page in 0..3 {
            snapshot[page * PAGE..(page + 1) * PAGE].fill(0x10 + page as u8);
        }
        std::fs::write(&snapshot_path, &snapshot).unwrap();
        let snapshot_file = File::open(&snapshot_path).unwrap();

        // Only the middle backing page is populated, with different content.
        let name = std::ffi::CString::new("test").unwrap();
        let fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
        let memfd = unsafe { File::from_raw_fd(fd) };
        memfd.set_len((3 * PAGE) as u64).unwrap();
        memfd.write_all_at(&[0xEE; PAGE], PAGE as u64).unwrap();
        let mut populated = PopulatedPages::new(3 * PAGE, PAGE);
        populated.mark(PAGE as u64, PAGE);
        let populated_refs = [&populated];
        let source = MemorySource {
            memfd: &memfd,
            page_size: PAGE,
            backing: Some((&snapshot_file, &populated_refs)),
        };

        // One 4 KiB granular run from the middle of page 0 to the middle of page 2: 4 KiB pages
        // 1..=4 of the six.
        let mem_path = tmp_dir.as_path().join("mem");
        let layout = layout(6, &[1, 2, 3, 4], vec![]);
        assert_eq!(layout.page_size, 4096);
        assert_eq!(copy_pages(source, &mem_path, &layout).unwrap(), 3);

        let mut expected = vec![0u8; 3 * PAGE];
        expected[4096..PAGE].copy_from_slice(&snapshot[4096..PAGE]);
        expected[PAGE..2 * PAGE].fill(0xEE);
        expected[2 * PAGE..2 * PAGE + 4096].copy_from_slice(&snapshot[2 * PAGE..2 * PAGE + 4096]);
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
            memory: layout(2, &[0, 1], vec![]),
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

        let ControlResponse::Done { success, message } = send(&request);
        assert!(success, "{message}");
        let mut expected = vec![1u8; 4096];
        expected.extend(std::iter::repeat_n(2u8, 4096));
        assert_eq!(std::fs::read(&mem_path).unwrap(), expected);
    }
}
