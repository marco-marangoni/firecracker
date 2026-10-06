// Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A model of diff snapshots with a memory backend, and a proof that they are correct.
//!
//! The question: when a memory backend applies the layout returned by `PUT /snapshot/create`
//! (`Diff`) to its copy of the previous memory file, is the result what the guest sees? The
//! real code is spread over KVM's dirty log, Firecracker's bitmaps, `mincore`, `madvise`, the
//! UFFD handler and the backend; this module reduces it to one page-indexed state machine small
//! enough to verify exhaustively with [Kani](https://model-checking.github.io/kani/), and
//! checks with a unit test (exhaustive over the model's inputs) that the model's classification
//! is the one `SnapshotMemoryLayout` implements.
//!
//! # The model
//!
//! Guest memory is `PAGES` pages. Per page, the state is:
//!
//! - the **memfd**: either present with some content (resident in Firecracker's mapping, which is
//!   what `mincore` reports) or a hole. A hole reads as the *base* snapshot's content if the UFFD
//!   handler still serves the page (never populated since a restore), or as zero if it does not
//!   (discarded by the balloon or virtio-mem, or a booted microVM). What the guest sees is
//!   derived from this: the memfd content if present, the base or zero if a hole.
//! - the **marks**: KVM's dirty log (set by guest writes, read-and-reset when the layout is
//!   computed) and Firecracker's bitmap (set by device writes, discards and unplugs, reset after
//!   `mincore`). A device may mark a page *ahead* of writing it (virtqueue rings, virtio-net RX
//!   buffers); such a page is **armed**, and an unmarked write to it may follow.
//! - the **backend's file**: the previous memory file, into which the backend merges each diff.
//!   For a restored microVM it starts as the base; for a booted one, as zeros.
//!
//! Operations: a guest write (faults the page in if it is a hole, then writes and marks in KVM's
//! log), a device marking ahead, a device write to an armed page (not marked, by definition), a
//! discard (punches the page to a zero hole and marks it), a discard of a page that cannot be
//! punched on its own (a partially covered hugetlbfs page: `discard_range` writes zeros to it
//! instead, which faults it in; whether the write happens is one of the two bugs below), unplug
//! and plug of a virtio-mem page, and the snapshot itself: read the
//! marks, read residency (a guest write may race between the two and between the second and the
//! reset, modelling `incremental` on a running guest), classify, reset, re-arm the virtqueue
//! pages, and have the backend apply the layout.
//!
//! The backend applies a returned layout whenever it gets to it, not inside the API call: layouts
//! queue up ([`State::pending`]) and are applied oldest first, while the guest keeps writing.
//!
//! Memory is backed by single pages or, with [`State::huge`], by huge pages of [`HUGE`] model
//! pages. A fault populates a whole backing page, a discard frees a whole
//! one or zero-writes the pages it cannot free, virtio-mem slots are whole backing pages, and
//! `mincore` reports per backing page. The lemma `H` records that residency is uniform within a
//! huge page and `Z` that a huge page with a zero page in a pending layout holds nothing but
//! zeros, marks, and pages of later layouts. Together they let a chunked backend on hugetlbfs
//! apply a layout with no copy of its own ([`State::apply_layout_chunked`]: read a chunk with a
//! layout page whole from the memfd, or zero-fill one with a zero page), which
//! `hugetlbfs_chunked_apply_preserves_invariant` proves correct for chunks no larger than a huge
//! page and `chunked_apply_across_backing_pages_breaks` shows wrong otherwise.
//!
//! `Full` snapshots are not the backend's: `full_from_the_memfd_alone_is_impossible` and
//! `firecracker_cannot_tell_base_holes_from_zero_holes` show why no response could make them so
//! after a lazy restore, `firecracker_full_is_correct` that Firecracker's `dump` is right,
//! `backend_full_is_correct` that a backend wanting a full file without populating the memfd has
//! what it needs on its own, and `applying_a_response_from_before_a_full_breaks` that a `Full`
//! ends the lineage of the responses before it.
//!
//! # The property
//!
//! After a snapshot of a paused guest, once the backend has applied every layout it was handed,
//! `file[p] == guest[p]` for every page. The proof is inductive over the invariant `I`: *once the
//! backend has applied every layout it holds, in order, a page whose file content differs from
//! what the guest sees is marked*, together with the structural lemma `L`: *a marked hole is a zero hole* (and an
//! armed page is marked, and an unplugged page is an unarmed zero hole). `I ∧ L` holds in both initial states, is preserved by every operation
//! including a racy snapshot, and together with the classification implies the property for a
//! paused snapshot: a dirty resident page gets the memfd content, which is what the guest sees; a
//! dirty hole is a zero hole by `L`, so writing zero is right; a clean page is already right by
//! `I`.
//!
//! `L` is exactly what the two fixes found during development buy: faulting in a page when it is
//! marked ahead of a write ([`FAULT_IN_ON_MARK`]), without which a restored virtqueue page that
//! was marked but not yet written would be a marked *base* hole, reported zero; and never marking
//! a page a discard left untouched ([`DISCARD_ZEROES_UNPUNCHABLE_EDGES`]: `discard_range` writes
//! zeros to the partial huge pages it cannot free, so every page it marks is a zero hole or
//! resident zeros), without which a partially covered huge page would be a marked base hole too.
//! The order of the two reads
//! ([`RESIDENCY_AFTER_DIRTY`]) is what makes a racy `incremental` pass preserve `I`. The
//! `should_panic` harnesses show that dropping any of the three breaks the proof.

#![allow(dead_code)]

use roaring::RoaringBitmap;

/// Pages in the model. Four is enough for every interaction between pages to appear (the
/// operations are per page and the invariant is per page).
pub const PAGES: usize = 4;

/// Fix 1: a device that marks a page dirty ahead of writing it faults the page in first
/// (`fault_in_marked_range`).
pub const FAULT_IN_ON_MARK: bool = true;
/// Fix 2: a discard writes zeros to the pages it cannot free (the partial huge pages at either
/// end of a hugetlbfs range), so that the whole range it marks reads as zero. Without it such a
/// page would be marked while still holding its old content, a base hole if never populated.
pub const DISCARD_ZEROES_UNPUNCHABLE_EDGES: bool = true;
/// Rule 3: residency is read after the dirty state, never before, so that a page written between
/// the two reads is seen as resident (and reported in the next set) rather than as a dirty hole.
pub const RESIDENCY_AFTER_DIRTY: bool = true;

/// What a hole in the memfd reads as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoleReads {
    /// The UFFD handler serves the base snapshot's content on a fault.
    Base,
    /// The kernel serves a zero page: the range was discarded (and unregistered), or the microVM
    /// was booted and the page never touched.
    Zero,
}

/// One page of the memfd, as Firecracker's mapping sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Memfd {
    /// Not resident; what it reads as depends on who serves the fault.
    Hole(HoleReads),
    /// Resident, with this content.
    Present(u8),
}

/// Everything the model tracks about one page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Page {
    /// The memfd page.
    pub memfd: Memfd,
    /// The base snapshot's content for this page (0 for a booted microVM).
    pub base: u8,
    /// The backend's file after the last diff was applied.
    pub file: u8,
    /// KVM's dirty log.
    pub kvm_marked: bool,
    /// Firecracker's own dirty bitmap (device writes; gated by `track_dirty_pages`).
    pub fc_marked: bool,
    /// The discard record. Modelled per host page, but written and read at *block* granularity
    /// (`discard_block`): every host page of a block is set or cleared together. This is the
    /// `dirty_huge_pages` bitmap on hugetlbfs (one bit per 2 MiB), where it is a separate,
    /// always-on bitmap independent of `track_dirty_pages`; on tmpfs the block is one host page
    /// and the discard record is simply folded into `dirty_host_pages` (`fc_marked`), so no second
    /// host-page bitmap is carried. Set by a balloon release, a virtio-mem unplug, or a
    /// zero-written hugetlbfs edge.
    pub discarded_block: bool,
    /// The guest kernel freed this page (balloon, free page reporting) or it is unplugged, and
    /// has not written it since. A well-behaved Linux guest will zero-initialize such a page on
    /// its next allocation before reading it (`__GFP_ZERO`), so its content before that write is
    /// don't-care *to the guest*. Set by [`State::discard`]/`discard_edge`/`unplug`, cleared by
    /// any guest or device write to the page and by `plug`. Only used to state the guarantee a
    /// backend that keeps old content in discarded ranges still gives
    /// ([`State::file_matches_guest_reads`]); not something Firecracker observes.
    pub guest_discarded: bool,
    /// Marked ahead of a write by a device; an unmarked write may follow.
    pub armed: bool,
    /// A virtqueue page: re-marked (and faulted in) after every snapshot.
    pub ring: bool,
    /// An unplugged virtio-mem page: inaccessible to the guest, reads as zero.
    pub unplugged: bool,
}

/// The whole model: guest memory, the memfd, the marks and the backend's file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    /// Per-page state.
    pub pages: [Page; PAGES],
    /// Layouts Firecracker has returned that the backend has not applied to its file yet, oldest
    /// first. The backend applies them in this order, whenever it gets to it.
    pub pending: [Option<Layout>; MAX_PENDING],
    /// Whether memory is backed by huge pages of [`HUGE`] model pages rather than by single pages.
    /// Residency is per backing page (a fault populates all of it, a discard frees all of it or
    /// none), and so is what `mincore` reports.
    pub huge: bool,
    /// Whether KVM's dirty log and Firecracker's bitmap are consulted. When off, the only signal
    /// is `mincore`, and a diff degrades to `dirty := resident` (see [`Self::read_dirty`]): the
    /// zero set is then always empty, which is why this mode is unsound once anything is
    /// discarded (`dirty_tracking_off_breaks_after_discard`).
    pub dirty_tracking: bool,
}

/// Model pages per huge page when [`State::huge`] is set. A power of two dividing [`PAGES`].
pub const HUGE: usize = 2;

/// How many returned layouts the backend may leave unapplied at once. Two is enough to express
/// every ordering question (apply in order, out of order, drop one).
pub const MAX_PENDING: usize = 2;

/// The layout of a diff, as the model computes it; [`SnapshotMemoryLayout::classify`] computes
/// the same thing from Roaring bitmaps (see `test_model_classification_matches_real`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// Pages to copy from the memfd.
    pub authoritative: [bool; PAGES],
    /// Pages that must read as zero.
    pub zero: [bool; PAGES],
}

/// Guest writes that may race with an `incremental` request on a running guest: one between
/// reading the dirty state and reading residency, one between residency and the reset of
/// Firecracker's bitmap. Device writes cannot race: they run on the thread that computes the
/// layout. `None` is a paused guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Race {
    /// A guest write between the dirty state and residency being read.
    pub after_dirty: Option<(usize, u8)>,
    /// A guest write between residency being read and the marks being reset.
    pub after_resident: Option<(usize, u8)>,
}

impl Page {
    /// What the guest reads from this page.
    pub fn guest(&self) -> u8 {
        if self.unplugged {
            return 0;
        }
        match self.memfd {
            Memfd::Present(v) => v,
            Memfd::Hole(HoleReads::Base) => self.base,
            Memfd::Hole(HoleReads::Zero) => 0,
        }
    }

    /// Whether the page is in Firecracker's mapping (what `mincore` reports).
    pub fn resident(&self) -> bool {
        matches!(self.memfd, Memfd::Present(_))
    }

    /// Whether the page is dirty in KVM's log, Firecracker's bitmap, or the discard bitmap.
    pub fn marked(&self) -> bool {
        self.kvm_marked || self.fc_marked || self.discarded_block
    }

    /// A read or write through Firecracker's mapping of a hole: the page becomes present with
    /// what the hole read as (the handler's `UFFDIO_COPY` of the base, or the kernel's zero page).
    fn fault_in(&mut self) {
        if let Memfd::Hole(reads) = self.memfd {
            self.memfd = Memfd::Present(match reads {
                HoleReads::Base => self.base,
                HoleReads::Zero => 0,
            });
        }
    }
}

impl State {
    /// Whether `p` and `q` are in the same backing page. (Loops over the backing page are written
    /// as `for q in 0..PAGES` filtered by this, so that every loop has a constant trip count,
    /// which Kani needs.)
    fn same_backing_page(&self, p: usize, q: usize) -> bool {
        if self.huge {
            p & !(HUGE - 1) == q & !(HUGE - 1)
        } else {
            p == q
        }
    }

    /// A read or write through Firecracker's mapping of page `p` when it is a hole: the whole
    /// backing page becomes present (the handler's `UFFDIO_COPY` or the kernel's zero page is
    /// backing-page sized).
    fn fault_in(&mut self, p: usize) {
        for q in 0..PAGES {
            if self.same_backing_page(p, q) {
                self.pages[q].fault_in();
            }
        }
    }

    /// A microVM restored from a base snapshot through the backend: every page is a hole the
    /// handler serves from the base, and the backend's file is the base.
    pub fn restored(base: [u8; PAGES], ring: [bool; PAGES], huge: bool) -> Self {
        let mut s = Self::booted(ring, huge);
        for (p, page) in s.pages.iter_mut().enumerate() {
            page.memfd = Memfd::Hole(HoleReads::Base);
            page.base = base[p];
            page.file = base[p];
        }
        s.arm_rings();
        s
    }

    /// A booted microVM: zero holes, no base, an empty (zero) file.
    pub fn booted(ring: [bool; PAGES], huge: bool) -> Self {
        let mut s = Self {
            pending: [None; MAX_PENDING],
            huge,
            dirty_tracking: true,
            pages: [Page {
                memfd: Memfd::Hole(HoleReads::Zero),
                base: 0,
                file: 0,
                kvm_marked: false,
                fc_marked: false,
                discarded_block: false,
                guest_discarded: false,
                armed: false,
                ring: false,
                unplugged: false,
            }; PAGES],
        };
        for (p, page) in s.pages.iter_mut().enumerate() {
            page.ring = ring[p];
        }
        s.arm_rings();
        s
    }

    /// `mark_virtio_queue_memory_dirty` (at activation and after every snapshot) and the RX
    /// buffer marking in `IoVecBufferMut`: mark ahead of writing.
    fn mark_ahead(&mut self, p: usize) {
        self.pages[p].fc_marked = true;
        self.pages[p].armed = true;
        if FAULT_IN_ON_MARK {
            self.fault_in(p);
        }
    }

    fn arm_rings(&mut self) {
        for p in 0..PAGES {
            if self.pages[p].ring && !self.pages[p].unplugged {
                self.mark_ahead(p);
            }
        }
    }

    /// The guest (a vCPU) writes `v` to page `p`. KVM logs it.
    pub fn guest_write(&mut self, p: usize, v: u8) {
        if self.pages[p].unplugged {
            return;
        }
        self.fault_in(p);
        self.pages[p].memfd = Memfd::Present(v);
        self.pages[p].kvm_marked = true;
        self.pages[p].guest_discarded = false;
    }

    /// A device marks page `p` dirty before it writes it (virtqueue ring, RX buffer).
    pub fn device_mark_ahead(&mut self, p: usize) {
        if self.pages[p].unplugged {
            return;
        }
        self.mark_ahead(p);
    }

    /// A device writes `v` to the armed page `p` through a raw pointer: no mark.
    pub fn device_write_armed(&mut self, p: usize, v: u8) {
        if !self.pages[p].armed || self.pages[p].unplugged {
            return;
        }
        self.fault_in(p);
        self.pages[p].memfd = Memfd::Present(v);
        self.pages[p].guest_discarded = false;
    }

    /// A device writes `v` to page `p` and marks it afterwards (block I/O completion).
    pub fn device_write_then_mark(&mut self, p: usize, v: u8) {
        if self.pages[p].unplugged {
            return;
        }
        self.fault_in(p);
        self.pages[p].memfd = Memfd::Present(v);
        self.pages[p].fc_marked = true;
        self.pages[p].guest_discarded = false;
    }

    /// The balloon or free page reporting releases the backing page containing `p`:
    /// `MADV_REMOVE` punches it whole (the handler gets a `remove` event and stops serving it),
    /// and it is marked. Only whole backing pages can be freed.
    pub fn discard(&mut self, p: usize) {
        if self.pages[p].unplugged {
            return;
        }
        for q in 0..PAGES {
            if self.same_backing_page(p, q) {
                self.pages[q].memfd = Memfd::Hole(HoleReads::Zero);
                self.pages[q].discarded_block = true;
                self.pages[q].guest_discarded = true;
            }
        }
    }

    /// A discard of a page the kernel cannot free on its own (part of a hugetlbfs huge page the
    /// range does not cover entirely): `discard_range` writes zeros to it through the mapping
    /// instead, which faults the backing page in, and marks it. The guest reads zero either way.
    pub fn discard_edge(&mut self, p: usize) {
        if self.pages[p].unplugged {
            return;
        }
        if DISCARD_ZEROES_UNPUNCHABLE_EDGES {
            self.fault_in(p);
            self.pages[p].memfd = Memfd::Present(0);
        }
        self.pages[p].discarded_block = true;
    }

    /// virtio-mem unplugs the slot containing `p` (slots are whole backing pages): discarded,
    /// marked, inaccessible.
    pub fn unplug(&mut self, p: usize) {
        for q in 0..PAGES {
            if self.same_backing_page(p, q) {
                let page = &mut self.pages[q];
                page.memfd = Memfd::Hole(HoleReads::Zero);
                page.discarded_block = true;
                page.guest_discarded = true;
                page.armed = false;
                page.unplugged = true;
            }
        }
    }

    /// virtio-mem plugs the slot containing `p` back: accessible again, still a zero hole. The
    /// page stays "guest will initialize before reading": freshly plugged virtio-mem memory is
    /// onlined and zeroed by the guest before use, exactly like reallocated discarded memory, so
    /// its file content remains don't-care until a real write clears `guest_discarded`.
    pub fn plug(&mut self, p: usize) {
        for q in 0..PAGES {
            if self.same_backing_page(p, q) {
                self.pages[q].unplugged = false;
            }
        }
    }

    /// `snapshot_layout`: the dirty state, residency, classification and reset, with the backend
    /// applying the result to its file. `race` injects guest writes at the two points where a
    /// running guest can interleave.
    pub fn snapshot(&mut self, race: Race) -> Layout {
        self.snapshot_ordered(race, RESIDENCY_AFTER_DIRTY)
    }

    fn read_dirty(&mut self) -> [bool; PAGES] {
        if !self.dirty_tracking {
            // No KVM log and no device bitmap, but the discard bitmap is kept regardless (it does
            // not need `track_dirty_pages`). So a changed page shows up only via `mincore`
            // (resident), and a discarded page via the discard bitmap even though it is not
            // resident. The discard bitmap is consumed like the others; nothing else resets.
            let mut dirty = [false; PAGES];
            for (page, dirty) in self.pages.iter_mut().zip(&mut dirty) {
                *dirty = page.resident() || page.discarded_block || page.unplugged;
                page.discarded_block = false;
            }
            return dirty;
        }
        // KVM's log (which resets it) ORed with Firecracker's bitmap and the discard bitmap;
        // unplugged pages are dirty.
        let mut dirty = [false; PAGES];
        for (page, dirty) in self.pages.iter_mut().zip(&mut dirty) {
            *dirty = page.marked() || page.unplugged;
            page.kvm_marked = false;
            page.discarded_block = false;
        }
        dirty
    }

    fn read_resident(&self) -> [bool; PAGES] {
        // `mincore` is not run on unplugged slots: they are reported not resident.
        std::array::from_fn(|p| !self.pages[p].unplugged && self.pages[p].resident())
    }

    /// The misuse the two-bitmap design must avoid: treating a set bit in the huge-page discard
    /// bitmap as "zero the whole 2 MiB", skipping the per-host-page `mincore`. A page re-faulted
    /// after the discard (resident, real content) is then wrongly zeroed. `apply_misclassified`
    /// drives this directly against the backend's file.
    pub fn snapshot_zeroing_whole_discarded_block(&mut self) -> Layout {
        let resident = self.read_resident();
        let dirty = self.read_dirty();
        let zero = std::array::from_fn(|p| {
            // Any page of the block discarded -> zero the whole block, mincore ignored.
            (0..PAGES).any(|q| self.same_backing_page(p, q) && dirty[q] && !resident[q])
        });
        let layout = Layout {
            authoritative: std::array::from_fn(|p| dirty[p] && resident[p] && !zero[p]),
            zero,
        };
        for page in &mut self.pages {
            page.fc_marked = false;
            page.armed = false;
        }
        self.arm_rings();
        if self.pending[MAX_PENDING - 1].is_some() {
            self.apply_oldest();
        }
        let slot = self.pending.iter().position(Option::is_none).unwrap();
        self.pending[slot] = Some(layout);
        layout
    }

    /// [`Self::snapshot`] with the order of the two reads as a parameter, to show that the order
    /// matters.
    pub fn snapshot_ordered(&mut self, race: Race, residency_after_dirty: bool) -> Layout {
        // 1 and 2. The dirty state and residency, in one order or the other, with a possible
        //    guest write in between.
        let (dirty, resident) = if residency_after_dirty {
            let dirty = self.read_dirty();
            if let Some((p, v)) = race.after_dirty {
                self.guest_write(p, v);
            }
            (dirty, self.read_resident())
        } else {
            let resident = self.read_resident();
            if let Some((p, v)) = race.after_dirty {
                self.guest_write(p, v);
            }
            (self.read_dirty(), resident)
        };
        if let Some((p, v)) = race.after_resident {
            self.guest_write(p, v);
        }
        // 3. Classify and reset Firecracker's bitmap. A device that marked ahead has its
        //    unwritten buffers returned to the guest before the reset
        //    (`prepare_dirty_tracking_reset`, `prepare_save`): nothing stays armed.
        let layout = Layout {
            authoritative: std::array::from_fn(|p| dirty[p] && resident[p]),
            zero: std::array::from_fn(|p| dirty[p] && !resident[p]),
        };
        for page in &mut self.pages {
            page.fc_marked = false;
            page.armed = false;
        }
        // (`discard_marked` is reset inside `read_dirty`, which runs before this.)
        // 4. Virtqueue pages are marked again so that they are part of the next diff.
        self.arm_rings();
        // 5. The layout is handed to the backend, which applies it later (`apply_oldest`). If it
        //    already holds `MAX_PENDING` layouts it applies the oldest first; the model is about
        //    order, not queue depth.
        if self.pending[MAX_PENDING - 1].is_some() {
            self.apply_oldest();
        }
        let slot = self.pending.iter().position(Option::is_none).unwrap();
        self.pending[slot] = Some(layout);
        layout
    }

    /// Everything Firecracker can know about page `p` when asked for a snapshot: the dirty state,
    /// `mincore`, the device and virtio-mem bookkeeping. Not what a hole would read as: that is in
    /// the backend's snapshot file and its fault handler's history, which Firecracker never sees.
    pub fn observable(&self, p: usize) -> (bool, bool, bool, bool, bool, bool, bool) {
        let page = &self.pages[p];
        (
            page.kvm_marked,
            page.fc_marked,
            page.discarded_block,
            page.resident(),
            page.unplugged,
            page.armed,
            page.ring,
        )
    }

    /// Firecracker's `Full` with a backend attached, which is a `Full` without one: `dump` reads
    /// every page through Firecracker's mapping, faulting in every hole (the handler serves the
    /// base, the kernel serves zeros), and resets the dirty state. The file is Firecracker's,
    /// not the backend's; the model writes it to `file` to ask what the backend may still do with
    /// the responses it has not applied (`keep_pending`).
    pub fn firecracker_full(&mut self, keep_pending: bool) {
        for p in 0..PAGES {
            if !self.pages[p].unplugged {
                self.fault_in(p);
            }
            self.pages[p].file = self.memfd_read(p);
        }
        for page in &mut self.pages {
            page.kvm_marked = false;
            page.fc_marked = false;
            page.discarded_block = false;
            page.guest_discarded = false;
            page.armed = false;
        }
        self.arm_rings();
        if !keep_pending {
            self.pending = [None; MAX_PENDING];
        }
    }

    /// The backend's own `Full` of a paused microVM, with nothing from Firecracker: `pread` the
    /// memfd (holes read as zero, nothing is faulted in) and, for the pages it has never
    /// populated through UFFD since the restore, the base's content. The handler knows which
    /// those are: a base hole fills only through its `UFFDIO_COPY`, and `MADV_REMOVE` with the
    /// remove-event feature blocks until it has read the event.
    pub fn backend_full(&self) -> [u8; PAGES] {
        std::array::from_fn(|p| match self.pages[p].memfd {
            Memfd::Hole(HoleReads::Base) => self.pages[p].base,
            _ => self.memfd_read(p),
        })
    }

    /// The backend applies one layout to its file: authoritative pages are copied from the memfd
    /// as it is *now*, zero pages are zeroed.
    pub fn apply_layout(&mut self, layout: Layout) {
        for p in 0..PAGES {
            if layout.authoritative[p] {
                if let Memfd::Present(v) = self.pages[p].memfd {
                    self.pages[p].file = v;
                } else {
                    // A page reported authoritative is resident, and nothing but a discard makes
                    // a resident page a hole again; a discard marks it and the next layout zeroes
                    // it, so reading the hole as zero here is right too.
                    self.pages[p].file = 0;
                }
            } else if layout.zero[p] {
                self.pages[p].file = 0;
            }
        }
    }

    /// The backend applies one layout but, instead of zeroing the `zero` pages, leaves their
    /// previous file content in place (a backend that treats `pages_to_discard` as "don't care"
    /// rather than "must be zero"). Authoritative pages are still copied.
    pub fn apply_layout_keep_discarded(&mut self, layout: Layout) {
        for p in 0..PAGES {
            if layout.authoritative[p] {
                self.pages[p].file = match self.pages[p].memfd {
                    Memfd::Present(v) => v,
                    Memfd::Hole(_) => 0,
                };
            }
            // `zero` pages: left as-is on purpose.
        }
    }

    /// The guest cannot tell the difference: `file` matches guest memory on every page the guest
    /// may read before writing. A page the guest discarded (balloon, free page reporting) or that
    /// is unplugged is excluded — a well-behaved Linux guest zero-initializes it on its next
    /// allocation before reading, so whatever stale content the file holds there is never
    /// observed. This is the guarantee a backend keeps when it leaves old content in
    /// `pages_to_discard` rather than zeroing. Note it is strictly weaker than
    /// [`Self::file_matches_guest`]: the files are *not* byte-identical, so a consumer that reads
    /// those bytes directly (`rebase-snap`, a diff merge) is not covered.
    pub fn file_matches_guest_reads(&self) -> bool {
        self.pages
            .iter()
            .all(|page| page.file == page.guest() || page.guest_discarded)
    }

    /// What `pread` on the memfd, or the backend's own mapping of it, returns for page `p`: the
    /// content if present, zero for a hole, whatever the hole would read as through
    /// Firecracker's mapping.
    pub fn memfd_read(&self, p: usize) -> u8 {
        match self.pages[p].memfd {
            Memfd::Present(v) => v,
            Memfd::Hole(_) => 0,
        }
    }

    /// A backend that works in chunks (of one page, or of [`HUGE`] pages with `huge_chunks`)
    /// applies a layout chunk by chunk with no previous copy of its own: a chunk with a zero page
    /// is zero-filled (`zero_fill`) or read whole from the memfd, a chunk with an authoritative
    /// page is read whole from the memfd, any other chunk is left alone. Correct only when a chunk
    /// never contains an unchanged page that is a base hole: on hugetlbfs with chunks no larger
    /// than the huge page, or for a booted microVM.
    pub fn apply_layout_chunked(&mut self, layout: Layout, huge_chunks: bool, zero_fill: bool) {
        for p in 0..PAGES {
            let in_chunk = |q: usize| {
                if huge_chunks {
                    p & !(HUGE - 1) == q & !(HUGE - 1)
                } else {
                    p == q
                }
            };
            if zero_fill && (0..PAGES).any(|q| in_chunk(q) && layout.zero[q]) {
                self.pages[p].file = 0;
            } else if (0..PAGES).any(|q| in_chunk(q) && (layout.authoritative[q] || layout.zero[q]))
            {
                self.pages[p].file = self.memfd_read(p);
            }
        }
    }

    /// The backend applies the oldest pending layout, chunked.
    pub fn apply_oldest_chunked(&mut self, huge_chunks: bool, zero_fill: bool) -> bool {
        let Some(layout) = self.pending[0] else {
            return false;
        };
        self.pending.rotate_left(1);
        self.pending[MAX_PENDING - 1] = None;
        self.apply_layout_chunked(layout, huge_chunks, zero_fill);
        true
    }

    /// The backend catches up with everything it was handed, chunked.
    pub fn apply_all_chunked(&mut self, huge_chunks: bool, zero_fill: bool) {
        while self.apply_oldest_chunked(huge_chunks, zero_fill) {}
    }

    /// The backend applies the oldest pending layout.
    pub fn apply_oldest(&mut self) -> bool {
        let Some(layout) = self.pending[0] else {
            return false;
        };
        self.pending.rotate_left(1);
        self.pending[MAX_PENDING - 1] = None;
        self.apply_layout(layout);
        true
    }

    /// The backend applies the *newest* pending layout first: the mistake of applying responses
    /// out of order. Returns `false` if fewer than two were pending.
    pub fn apply_newest_first(&mut self) -> bool {
        let (Some(oldest), Some(newest)) = (self.pending[0], self.pending[1]) else {
            return false;
        };
        self.pending = [None; MAX_PENDING];
        self.apply_layout(newest);
        self.apply_layout(oldest);
        true
    }

    /// A response is lost: the oldest pending layout is dropped without being applied.
    pub fn drop_oldest(&mut self) -> bool {
        if self.pending[0].is_none() {
            return false;
        }
        self.pending.rotate_left(1);
        self.pending[MAX_PENDING - 1] = None;
        true
    }

    /// The backend catches up with everything it was handed.
    pub fn apply_all(&mut self) {
        while self.apply_oldest() {}
    }

    /// Catch up with everything, but leave old content in the discarded (`zero`) pages instead of
    /// zeroing them (a backend that treats `pages_to_discard` as don't-care).
    pub fn apply_all_keep_discarded(&mut self) {
        while let Some(layout) = self.pending[0] {
            self.pending.rotate_left(1);
            self.pending[MAX_PENDING - 1] = None;
            self.apply_layout_keep_discarded(layout);
        }
    }

    /// Whether some pending layout classifies page `p` (as authoritative or zero).
    fn pending_covers(&self, p: usize) -> bool {
        self.pending
            .iter()
            .flatten()
            .any(|l| l.authoritative[p] || l.zero[p])
    }

    /// The backend's file as it would be once it has applied every pending layout, in order,
    /// against the memfd as it is now.
    pub fn effective_file(&self) -> [u8; PAGES] {
        let mut caught_up = *self;
        caught_up.apply_all();
        std::array::from_fn(|p| caught_up.pages[p].file)
    }

    /// `I`: a page the caught-up file has wrong is marked. (An unplugged page reads as zero;
    /// unplugging marks it, and a snapshot taken while it is unplugged reports it zero, so
    /// plugging it back never exposes a stale file page.)
    pub fn invariant(&self) -> bool {
        let effective = self.effective_file();
        (0..PAGES).all(|p| effective[p] == self.pages[p].guest() || self.pages[p].marked())
    }

    /// `L`: a marked hole reads as zero (a marked page is in the memfd or zero: the property
    /// Firecracker promises the backend); an armed page is marked; an unplugged page is a zero
    /// hole and not armed (unplugging discards the slot and nothing writes it afterwards); and a
    /// page a pending layout classifies is not a base hole (it was resident or a zero hole when
    /// classified, and only a discard or an unplug makes a hole of it afterwards, a zero one).
    pub fn lemma(&self) -> bool {
        (0..PAGES).all(|p| {
            let page = &self.pages[p];
            (page.resident() || !page.marked() || page.memfd == Memfd::Hole(HoleReads::Zero))
                && (!page.armed || page.fc_marked)
                && (!page.unplugged || (page.memfd == Memfd::Hole(HoleReads::Zero) && !page.armed))
                && (!self.pending_covers(p) || page.memfd != Memfd::Hole(HoleReads::Base))
        }) && self.backing_pages_are_uniform()
            && self.zero_pages_have_zero_huge_pages()
    }

    /// `Z` (huge backing only): if a pending layout classifies `p` zero, every page `q` of `p`'s
    /// huge page either reads zero to the guest, or is marked, or is in a *later* pending layout.
    /// (The huge page was a hole when `p` was classified, and every write to it since is
    /// accounted for.) What lets a chunked backend zero-fill a chunk that has a zero page.
    pub fn zero_pages_have_zero_huge_pages(&self) -> bool {
        if !self.huge {
            return true;
        }
        (0..MAX_PENDING).all(|k| {
            let Some(layout) = self.pending[k] else {
                return true;
            };
            (0..PAGES).all(|p| {
                !layout.zero[p]
                    || (0..PAGES).all(|q| {
                        !self.same_backing_page(p, q)
                            || self.pages[q].guest() == 0
                            || self.pages[q].marked()
                            || (k + 1..MAX_PENDING).any(|j| {
                                self.pending[j].is_some_and(|l| l.authoritative[q] || l.zero[q])
                            })
                    })
            })
        })
    }

    /// `H`: residency and plug state are per backing page. Every page of a backing page is
    /// present, or every page is a base hole, or every page is a zero hole; and all are plugged
    /// or all unplugged. Trivial for 4 KiB backing.
    pub fn backing_pages_are_uniform(&self) -> bool {
        if !self.huge {
            return true;
        }
        let kind = |page: &Page| match page.memfd {
            Memfd::Present(_) => 0,
            Memfd::Hole(HoleReads::Base) => 1,
            Memfd::Hole(HoleReads::Zero) => 2,
        };
        (0..PAGES).all(|q| {
            let first = &self.pages[q & !(HUGE - 1)];
            kind(&self.pages[q]) == kind(first) && self.pages[q].unplugged == first.unplugged
        })
    }

    /// The property: the file is what the guest sees.
    pub fn file_matches_guest(&self) -> bool {
        self.pages.iter().all(|page| page.file == page.guest())
    }
}

/// The real classification on the model's inputs.
pub fn real_layout(dirty: &[bool; PAGES], resident: &[bool; PAGES]) -> Layout {
    let set = |bits: &[bool; PAGES]| -> RoaringBitmap {
        bits.iter()
            .enumerate()
            .filter(|(_, b)| **b)
            .map(|(p, _)| u32::try_from(p).unwrap())
            .collect()
    };
    let layout = crate::vmm_config::snapshot::SnapshotMemoryLayout::classify(
        (PAGES * 4096) as u64,
        4096,
        &set(dirty),
        &set(resident),
    );
    Layout {
        authoritative: std::array::from_fn(|p| layout.page_is_authoritative(p as u64 * 4096)),
        zero: std::array::from_fn(|p| layout.page_is_discarded(p as u64 * 4096)),
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    fn any_page() -> Page {
        let memfd = match kani::any::<u8>() % 3 {
            0 => Memfd::Hole(HoleReads::Base),
            1 => Memfd::Hole(HoleReads::Zero),
            _ => Memfd::Present(kani::any()),
        };
        Page {
            memfd,
            base: kani::any(),
            file: kani::any(),
            kvm_marked: kani::any(),
            fc_marked: kani::any(),
            discarded_block: kani::any(),
            guest_discarded: kani::any(),
            armed: kani::any(),
            ring: kani::any(),
            unplugged: kani::any(),
        }
    }

    fn any_layout() -> Option<Layout> {
        if kani::any() {
            let authoritative: [bool; PAGES] = kani::any();
            let zero: [bool; PAGES] = kani::any();
            // Layouts Firecracker produces are disjoint and have no authoritative hole.
            for p in 0..PAGES {
                kani::assume(!(authoritative[p] && zero[p]));
            }
            Some(Layout {
                authoritative,
                zero,
            })
        } else {
            None
        }
    }

    /// Any state satisfying `I ∧ L`, with any (gap-free) queue of pending layouts, for either
    /// backing page size.
    fn any_state() -> State {
        any_state_with_backing(kani::any())
    }

    fn any_state_with_backing(huge: bool) -> State {
        let s = State {
            pages: std::array::from_fn(|_| any_page()),
            pending: std::array::from_fn(|_| any_layout()),
            huge,
            dirty_tracking: true,
        };
        kani::assume(s.pending[1].is_none() || s.pending[0].is_some());
        kani::assume(s.invariant());
        kani::assume(s.lemma());
        s
    }

    fn any_index() -> usize {
        let p: usize = kani::any();
        kani::assume(p < PAGES);
        p
    }

    fn any_race() -> Race {
        Race {
            after_dirty: if kani::any() {
                Some((any_index(), kani::any()))
            } else {
                None
            },
            after_resident: if kani::any() {
                Some((any_index(), kani::any()))
            } else {
                None
            },
        }
    }

    /// Applies one arbitrary operation.
    fn any_step(s: &mut State) {
        let p = any_index();
        match kani::any::<u8>() % 11 {
            0 => s.guest_write(p, kani::any()),
            1 => s.device_mark_ahead(p),
            2 => s.device_write_armed(p, kani::any()),
            3 => s.device_write_then_mark(p, kani::any()),
            4 => s.discard(p),
            5 => s.discard_edge(p),
            6 => s.unplug(p),
            7 => s.plug(p),
            8 => {
                s.apply_oldest();
            }
            9 => s.firecracker_full(false),
            _ => {
                s.snapshot(any_race());
            }
        }
    }

    /// Base case: both initial states satisfy `I ∧ L`, for either backing page size.
    #[kani::proof]
    fn initial_states_satisfy_invariant() {
        let ring: [bool; PAGES] = kani::any();
        let huge: bool = kani::any();
        let s = State::restored(kani::any(), ring, huge);
        assert!(s.invariant() && s.lemma());
        let s = State::booted(ring, huge);
        assert!(s.invariant() && s.lemma());
    }

    /// Inductive step: from any state satisfying `I ∧ L`, any operation (including a racy
    /// snapshot) preserves `I ∧ L`.
    #[kani::proof]
    #[kani::unwind(5)]
    fn operations_preserve_invariant() {
        let mut s = any_state();
        any_step(&mut s);
        assert!(s.invariant());
        assert!(s.lemma());
    }

    /// Conclusion: from any state satisfying `I ∧ L`, a snapshot of a paused guest followed by
    /// the backend applying everything it was handed, in order, leaves the backend's file equal
    /// to guest memory; and the layout is well formed.
    #[kani::proof]
    #[kani::unwind(5)]
    fn paused_snapshot_is_correct() {
        let mut s = any_state();
        let layout = s.snapshot(Race::default());
        s.apply_all();
        assert!(s.file_matches_guest());
        for p in 0..PAGES {
            assert!(!(layout.authoritative[p] && layout.zero[p]));
        }
    }

    /// A paused snapshot is also correct after a racy `incremental` pass whose layout the backend
    /// applies only later: what the running guest wrote in between is in the next set.
    #[kani::proof]
    #[kani::unwind(5)]
    fn racy_then_paused_snapshot_is_correct() {
        let mut s = any_state();
        s.snapshot(any_race());
        s.snapshot(Race::default());
        s.apply_all();
        assert!(s.file_matches_guest());
    }

    /// A chunked backend on hugetlbfs, with chunks no larger than a huge page, needs no previous
    /// copy: reading every chunk that has a page in the layout whole from the memfd, or
    /// zero-filling those that have a zero page, preserves `I ∧ L` from any state, however late
    /// the layout is applied.
    #[kani::proof]
    #[kani::unwind(5)]
    fn hugetlbfs_chunked_apply_preserves_invariant() {
        let mut s = any_state_with_backing(true);
        s.apply_oldest_chunked(kani::any(), kani::any());
        assert!(s.invariant());
        assert!(s.lemma());
    }

    /// And the result of a paused snapshot applied that way is guest memory.
    #[kani::proof]
    #[kani::unwind(5)]
    fn hugetlbfs_chunked_paused_snapshot_is_correct() {
        let mut s = any_state_with_backing(true);
        s.snapshot(Race::default());
        s.apply_all_chunked(kani::any(), kani::any());
        assert!(s.file_matches_guest());
    }

    /// The same strategy with a chunk larger than the backing page (here: 4 KiB backing, 2-page
    /// chunks) breaks: a never-faulted unchanged page next to an authoritative one is read from
    /// the memfd as zero while the guest reads the base.
    #[kani::proof]
    #[kani::unwind(5)]
    #[kani::should_panic]
    fn chunked_apply_across_backing_pages_breaks() {
        let mut s = any_state_with_backing(false);
        kani::assume(s.apply_oldest_chunked(true, false));
        assert!(s.invariant());
    }

    /// Without the discard bitmap, dirty tracking off is unsound after a discard: a diff
    /// degrades to `dirty := resident`, whose zero set is empty, so a discarded page (non-resident,
    /// stale content in the file) is in neither set and never corrected. `without_discard_bitmap`
    /// turns the always-on discard bitmap off to model the earlier design, and the proof fails as
    /// it must. (Here: a restored page the guest wrote and the balloon then released.)
    #[kani::proof]
    #[kani::unwind(5)]
    #[kani::should_panic]
    fn dirty_tracking_off_without_discard_bitmap_breaks_after_discard() {
        let base: [u8; PAGES] = kani::any();
        kani::assume(base[0] != 0);
        let mut s = State::restored(base, [false; PAGES], false);
        s.dirty_tracking = false;
        for p in 0..PAGES {
            s.pages[p].file = s.pages[p].base;
        }
        s.guest_write(0, kani::any());
        s.discard(0);
        s.pages[0].discarded_block = false; // the old design had no discard record
        s.snapshot(Race::default());
        s.apply_all();
        assert!(s.file_matches_guest());
    }

    /// Keeping a discard bitmap that does not need `track_dirty_pages` fixes it: with dirty
    /// tracking off, `dirty := resident ∨ discarded`, so a discarded page lands in `pages_to_discard`
    /// and is corrected, while every changed page is resident and copied. From a restored state
    /// after an arbitrary run of guest writes, device writes, discards and unplugs, a paused
    /// snapshot applied by the backend yields guest memory. The write record still needs KVM (a
    /// guest write to a page that stays resident is caught by `mincore`; the model has no silent
    /// non-resident write), but the discard record does not.
    #[kani::proof]
    #[kani::unwind(5)]
    fn dirty_tracking_off_with_discard_bitmap_is_correct() {
        let base: [u8; PAGES] = kani::any();
        let mut s = State::restored(base, kani::any(), kani::any());
        s.dirty_tracking = false;
        for p in 0..PAGES {
            s.pages[p].file = s.pages[p].base;
        }
        for _ in 0..3 {
            let p = any_index();
            match kani::any::<u8>() % 5 {
                0 => s.guest_write(p, kani::any()),
                1 => s.device_mark_ahead(p),
                2 => s.device_write_armed(p, kani::any()),
                3 => s.discard(p),
                _ => s.unplug(p),
            }
        }
        s.snapshot(Race::default());
        s.apply_all();
        assert!(s.file_matches_guest());
    }

    /// The two-bitmap split on hugetlbfs: writes in a host-page bitmap, discards in a *huge-page*
    /// bitmap (one bit per 2 MiB). Correct for the same reason the chunked backend is: residency
    /// is uniform per huge page, so a discarded block whose bit is set is either still a hole
    /// throughout (all pages `zero`) or has been re-faulted throughout (all pages resident, all
    /// `copy`) — the per-page `mincore` the classification runs resolves which, and the coarse
    /// bit never over-zeroes. Proven over writes, discards, unplugs and re-plugs, with and without
    /// dirty tracking.
    #[kani::proof]
    #[kani::unwind(5)]
    fn hugetlbfs_two_bitmaps_classification_is_correct() {
        let base: [u8; PAGES] = kani::any();
        let mut s = State::restored(base, kani::any(), true);
        s.dirty_tracking = kani::any();
        for p in 0..PAGES {
            s.pages[p].file = s.pages[p].base;
        }
        for _ in 0..3 {
            let p = any_index();
            match kani::any::<u8>() % 6 {
                0 => s.guest_write(p, kani::any()),
                1 => s.device_mark_ahead(p),
                2 => s.device_write_armed(p, kani::any()),
                3 => s.discard(p),
                4 => s.unplug(p),
                _ => s.plug(p),
            }
        }
        s.snapshot(Race::default());
        s.apply_all();
        assert!(s.file_matches_guest());
    }

    /// Zeroing a whole discarded 2 MiB block *without* consulting per-host-page `mincore` is in
    /// fact also correct on hugetlbfs — not because it is a good idea, but because residency is
    /// uniform per huge page (`H`): a block cannot hold a resident, re-written page next to a
    /// non-resident one, so the case where coarse zeroing would lose data is unreachable. The
    /// harness proves this (no `should_panic`): the hazard that makes coarse zeroing wrong exists
    /// only if residency can be finer than the discard block, which hugetlbfs does not allow.
    /// A real implementation should still expand through `mincore` so the same code is correct on
    /// tmpfs, where residency *is* per host page.
    #[kani::proof]
    #[kani::unwind(5)]
    fn hugetlbfs_zeroing_whole_discarded_block_is_also_correct() {
        let base: [u8; PAGES] = kani::any();
        let mut s = State::restored(base, kani::any(), true);
        s.dirty_tracking = kani::any();
        for p in 0..PAGES {
            s.pages[p].file = s.pages[p].base;
        }
        for _ in 0..3 {
            let p = any_index();
            match kani::any::<u8>() % 5 {
                0 => s.guest_write(p, kani::any()),
                1 => s.device_write_armed(p, kani::any()),
                2 => s.discard(p),
                3 => s.unplug(p),
                _ => s.plug(p),
            }
        }
        s.snapshot_zeroing_whole_discarded_block();
        s.apply_all();
        assert!(s.file_matches_guest());
    }

    /// A backend that leaves old content in `pages_to_discard` instead of zeroing still gives the
    /// guest a correct view: after a paused snapshot applied that way, `file` matches guest memory
    /// on every page the guest may read before writing. The only pages that may differ are ones
    /// the guest discarded (balloon, free page reporting) or unplugged — which a well-behaved
    /// Linux guest zero-initializes before reading on its next allocation. Proven over an
    /// arbitrary run of guest writes, device writes, discards, unplugs and re-plugs, with and
    /// without dirty tracking.
    #[kani::proof]
    #[kani::unwind(5)]
    fn keeping_discarded_content_is_invisible_to_the_guest() {
        let base: [u8; PAGES] = kani::any();
        let mut s = State::restored(base, kani::any(), kani::any());
        s.dirty_tracking = kani::any();
        for p in 0..PAGES {
            s.pages[p].file = s.pages[p].base;
        }
        for _ in 0..3 {
            let p = any_index();
            match kani::any::<u8>() % 6 {
                0 => s.guest_write(p, kani::any()),
                1 => s.device_mark_ahead(p),
                2 => s.device_write_armed(p, kani::any()),
                3 => s.discard(p),
                4 => s.unplug(p),
                _ => s.plug(p),
            }
        }
        s.snapshot(Race::default());
        s.apply_all_keep_discarded();
        assert!(s.file_matches_guest_reads());
    }

    /// The honest companion: leaving old content does *not* make the file byte-identical to guest
    /// memory. There is a reachable state where a discarded page the guest reads as zero holds
    /// non-zero content in a keep-discarded file — so this is safe only for a guest that
    /// re-initializes, never for `rebase-snap` or a diff merge that reads the bytes.
    #[kani::proof]
    #[kani::unwind(5)]
    #[kani::should_panic]
    fn keeping_discarded_content_differs_from_guest_memory() {
        let base: [u8; PAGES] = kani::any();
        let mut s = State::restored(base, [false; PAGES], false);
        for p in 0..PAGES {
            s.pages[p].file = s.pages[p].base;
        }
        s.guest_write(0, 7); // page 0 now holds 7, in the file after the first diff
        s.snapshot(Race::default());
        s.apply_all();
        s.discard(0); // the guest frees page 0: reads zero, but the file still holds 7
        s.snapshot(Race::default());
        s.apply_all_keep_discarded();
        assert!(s.file_matches_guest()); // fails: file[0] == 7, guest reads 0
    }

    /// Why Firecracker writes `Full` snapshots itself. First, a `Full` assembled from the memfd
    /// alone is impossible after a lazy restore: whatever class any page is given (copy from the
    /// memfd, or zero; "unchanged" has no meaning for a `Full`, there is no previous file), a page
    /// the guest has never touched since the restore reads as the base through the fault handler
    /// and as zero from the memfd. No response Firecracker could return changes that.
    #[kani::proof]
    #[kani::unwind(5)]
    fn full_from_the_memfd_alone_is_impossible() {
        let s = any_state();
        let p = any_index();
        let page = &s.pages[p];
        kani::assume(
            page.memfd == Memfd::Hole(HoleReads::Base) && page.base != 0 && !page.unplugged,
        );
        let copy: bool = kani::any();
        let full = if copy { s.memfd_read(p) } else { 0 };
        assert_ne!(full, page.guest());
    }

    /// Second, Firecracker cannot even say which pages those are: once the diff that recorded a
    /// discard is consumed, a base hole and a zero hole look the same to it. Two reachable
    /// states, identical in everything Firecracker observes for every page and with nothing
    /// pending, whose guests read differently.
    #[kani::proof]
    #[kani::unwind(5)]
    fn firecracker_cannot_tell_base_holes_from_zero_holes() {
        let base: [u8; PAGES] = kani::any();
        let ring: [bool; PAGES] = kani::any();
        kani::assume(base[0] != 0 && !ring[0]);
        let a = State::restored(base, ring, false);
        let mut b = State::restored(base, ring, false);
        b.guest_write(0, kani::any());
        b.discard(0);
        b.snapshot(Race::default());
        b.apply_all();
        assert!(a.invariant() && a.lemma() && b.invariant() && b.lemma());
        for p in 0..PAGES {
            assert_eq!(a.observable(p), b.observable(p));
        }
        assert_eq!(a.pending, b.pending);
        assert_ne!(a.pages[0].guest(), b.pages[0].guest());
    }

    /// What does work: Firecracker's `dump`, which faults everything in.
    #[kani::proof]
    #[kani::unwind(5)]
    fn firecracker_full_is_correct() {
        let mut s = any_state();
        s.firecracker_full(false);
        assert!(s.file_matches_guest());
        assert!(s.invariant() && s.lemma());
    }

    /// And the backend's own `Full`, from the memfd plus what its fault handler knows, with
    /// nothing from Firecracker and without populating the memfd. (In the model this is the
    /// definition of what the guest reads; the content of the proof is the modelling assumption
    /// that the handler sees every population and every removal, stated on
    /// [`State::backend_full`].)
    #[kani::proof]
    #[kani::unwind(5)]
    fn backend_full_is_correct() {
        let s = any_state();
        let full = s.backend_full();
        for p in 0..PAGES {
            assert_eq!(full[p], s.pages[p].guest());
        }
    }

    /// A `Full` starts over: a response received before it must not be applied to the `Full`'s
    /// lineage. A page zeroed by that response and written by the guest afterwards is in the
    /// `Full`; the mark that recorded the write is consumed by the `Full`; applying the response
    /// zeroes the page with nothing left to repair it.
    #[kani::proof]
    #[kani::unwind(5)]
    #[kani::should_panic]
    fn applying_a_response_from_before_a_full_breaks() {
        let mut s = any_state();
        s.firecracker_full(true);
        kani::assume(s.apply_oldest());
        assert!(s.invariant());
    }

    /// Applying two responses in the wrong order breaks the invariant: a page zero in the older
    /// one and authoritative in the newer one ends up zero with no mark left to repair it.
    #[kani::proof]
    #[kani::unwind(5)]
    #[kani::should_panic]
    fn applying_responses_out_of_order_breaks() {
        let mut s = any_state();
        kani::assume(s.apply_newest_first());
        assert!(s.invariant());
    }

    /// Losing a response breaks the invariant: the pages it classified were consumed from the
    /// marks and are now in nobody's hands. (The remedy is a `Full`, which Firecracker writes.)
    #[kani::proof]
    #[kani::unwind(5)]
    #[kani::should_panic]
    fn losing_a_response_breaks() {
        let mut s = any_state();
        kani::assume(s.drop_oldest());
        assert!(s.invariant());
    }

    /// Without faulting in on mark-ahead, a restored virtqueue page that was marked but not yet
    /// written is a marked base hole: classified zero, while the guest reads the base.
    #[kani::proof]
    #[kani::unwind(5)]
    #[kani::should_panic]
    fn without_fault_in_on_mark_the_invariant_breaks() {
        let mut s = any_state();
        let p = any_index();
        // The buggy operation, inlined: mark ahead without faulting in.
        kani::assume(!s.pages[p].unplugged);
        s.pages[p].fc_marked = true;
        s.pages[p].armed = true;
        assert!(s.lemma());
    }

    /// Without zero-writing the unpunchable edges, a partially covered huge page is marked but
    /// neither punched nor written: a marked base hole if the handler never populated it.
    #[kani::proof]
    #[kani::unwind(5)]
    #[kani::should_panic]
    fn without_zeroing_discard_edges_the_invariant_breaks() {
        let mut s = any_state();
        let p = any_index();
        kani::assume(!s.pages[p].unplugged);
        s.pages[p].fc_marked = true;
        assert!(s.lemma());
    }

    /// With residency read before the dirty state, a guest write in between leaves a page that is
    /// dirty in this set but was not resident when looked at: reported zero, consumed, and the
    /// guest's value is in neither the file nor the next set.
    #[kani::proof]
    #[kani::unwind(5)]
    #[kani::should_panic]
    fn without_residency_after_dirty_the_invariant_breaks() {
        let mut s = any_state();
        let race = Race {
            after_dirty: Some((any_index(), kani::any())),
            after_resident: None,
        };
        s.snapshot_ordered(race, false);
        assert!(s.invariant());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic xorshift, so the tests are reproducible.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn page(&mut self) -> usize {
            usize::try_from(self.next() % PAGES as u64).unwrap()
        }
        fn byte(&mut self) -> u8 {
            (self.next() & 0xff) as u8
        }
        fn bools(&mut self) -> [bool; PAGES] {
            std::array::from_fn(|_| self.next() & 1 == 1)
        }
        fn step(&mut self, s: &mut State) {
            let p = self.page();
            match self.next() % 10 {
                0 => s.guest_write(p, self.byte()),
                1 => s.device_mark_ahead(p),
                2 => s.device_write_armed(p, self.byte()),
                3 => s.device_write_then_mark(p, self.byte()),
                4 => s.discard(p),
                5 => s.discard_edge(p),
                6 => s.unplug(p),
                7 => s.plug(p),
                8 => {
                    s.apply_oldest();
                }
                _ => {
                    let race = Race {
                        after_dirty: (self.next() & 1 == 1).then(|| (self.page(), self.byte())),
                        after_resident: (self.next() & 1 == 1).then(|| (self.page(), self.byte())),
                    };
                    s.snapshot(race);
                }
            }
        }
    }

    /// Random walks from both initial states: the invariant holds throughout and every paused
    /// snapshot leaves the file equal to guest memory. (The Kani harnesses prove this for every
    /// state; this test keeps the model honest when Kani is not run.)
    #[test]
    fn test_random_walks_keep_file_consistent() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for round in 0..2000 {
            let ring = rng.bools();
            let huge = round % 4 >= 2;
            let mut s = if round % 2 == 0 {
                State::restored(std::array::from_fn(|_| rng.byte()), ring, huge)
            } else {
                State::booted(ring, huge)
            };
            assert!(s.invariant() && s.lemma());
            for _ in 0..(rng.next() % 12) {
                rng.step(&mut s);
                assert!(s.invariant(), "{s:?}");
                assert!(s.lemma(), "{s:?}");
            }
            s.snapshot(Race::default());
            if huge && rng.next() & 1 == 1 {
                s.apply_all_chunked(true, rng.next() & 2 == 2);
            } else {
                s.apply_all();
            }
            assert!(s.file_matches_guest(), "{s:?}");
        }
    }

    /// The two bugs the model encodes, reproduced on the scenarios that found them.
    #[test]
    fn test_known_bugs_are_caught_by_the_lemma() {
        // A restored virtqueue page: marked at activation, not written before the snapshot.
        let mut s = State::restored([7; PAGES], [false; PAGES], false);
        s.pages[0].fc_marked = true; // mark without the fault-in
        assert!(!s.lemma());
        // What the layout would have done: the page is a dirty hole, so "zero".
        let mut buggy = s;
        buggy.pages[0].file = 0;
        assert_ne!(buggy.pages[0].file, buggy.pages[0].guest());
        // With the fault-in, the mark is safe.
        let mut s = State::restored([7; PAGES], [false; PAGES], false);
        s.device_mark_ahead(0);
        assert!(s.lemma());
        s.snapshot(Race::default());
        s.apply_all();
        assert!(s.file_matches_guest());

        // A partial hugetlbfs discard on a never-populated restored page.
        let mut s = State::restored([9; PAGES], [false; PAGES], true);
        s.pages[1].fc_marked = true; // marked though neither punched nor zero-written
        assert!(!s.lemma());
        let mut s = State::restored([9; PAGES], [false; PAGES], true);
        s.discard_edge(1);
        assert!(s.lemma());
        assert_eq!(s.pages[1].guest(), 0);
        s.snapshot(Race::default());
        s.apply_all();
        assert!(s.file_matches_guest());
    }

    /// The model's classification is `SnapshotMemoryLayout::classify`, for all 256 inputs.
    #[test]
    fn test_model_classification_matches_real() {
        for bits in 0..(1u32 << (2 * PAGES)) {
            let dirty: [bool; PAGES] = std::array::from_fn(|p| bits >> p & 1 == 1);
            let resident: [bool; PAGES] = std::array::from_fn(|p| bits >> (PAGES + p) & 1 == 1);
            let model = Layout {
                authoritative: std::array::from_fn(|p| dirty[p] && resident[p]),
                zero: std::array::from_fn(|p| dirty[p] && !resident[p]),
            };
            assert_eq!(
                real_layout(&dirty, &resident),
                model,
                "{dirty:?} {resident:?}"
            );
        }
    }
}
