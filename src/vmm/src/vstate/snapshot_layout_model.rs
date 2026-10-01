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
//! discard (punches the page to a zero hole and marks it), an ineffective discard (what
//! `MADV_REMOVE` does to a partially covered hugetlbfs page: nothing; whether it marks is one of
//! the two bugs below), unplug and plug of a virtio-mem page, and the snapshot itself: read the
//! marks, read residency (a guest write may race between the two and between the second and the
//! reset, modelling `dirty-pages` on a running guest), classify, reset, re-arm the virtqueue
//! pages, and have the backend apply the layout.
//!
//! The backend applies a returned layout whenever it gets to it, not inside the API call: layouts
//! queue up ([`State::pending`]) and are applied oldest first, while the guest keeps writing.
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
//! was marked but not yet written would be a marked *base* hole, reported zero; and marking only
//! what a discard actually punched ([`DISCARD_MARKS_ONLY_IF_EFFECTIVE`]), without which a
//! partially covered huge page would be a marked base hole too. The order of the two reads
//! ([`RESIDENCY_AFTER_DIRTY`]) is what makes a racy `dirty-pages` pass preserve `I`. The
//! `should_panic` harnesses show that dropping any of the three breaks the proof.

#![allow(dead_code)]

use roaring::RoaringBitmap;

/// Pages in the model. Four is enough for every interaction between pages to appear (the
/// operations are per page and the invariant is per page).
pub const PAGES: usize = 4;

/// Fix 1: a device that marks a page dirty ahead of writing it faults the page in first
/// (`fault_in_marked_range`).
pub const FAULT_IN_ON_MARK: bool = true;
/// Fix 2: a discard marks a page dirty only if it actually punched it (`discard_range` rounds a
/// hugetlbfs range inward before marking).
pub const DISCARD_MARKS_ONLY_IF_EFFECTIVE: bool = true;
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
    /// Firecracker's own dirty bitmap.
    pub fc_marked: bool,
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
}

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

/// Guest writes that may race with a `dirty-pages` request on a running guest: one between
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

    /// Whether the page is dirty in KVM's log or Firecracker's bitmap.
    pub fn marked(&self) -> bool {
        self.kvm_marked || self.fc_marked
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
    /// A microVM restored from a base snapshot through the backend: every page is a hole the
    /// handler serves from the base, and the backend's file is the base.
    pub fn restored(base: [u8; PAGES], ring: [bool; PAGES]) -> Self {
        let mut s = Self::booted(ring);
        for (p, page) in s.pages.iter_mut().enumerate() {
            page.memfd = Memfd::Hole(HoleReads::Base);
            page.base = base[p];
            page.file = base[p];
        }
        s.arm_rings();
        s
    }

    /// A booted microVM: zero holes, no base, an empty (zero) file.
    pub fn booted(ring: [bool; PAGES]) -> Self {
        let mut s = Self {
            pending: [None; MAX_PENDING],
            pages: [Page {
                memfd: Memfd::Hole(HoleReads::Zero),
                base: 0,
                file: 0,
                kvm_marked: false,
                fc_marked: false,
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
        let page = &mut self.pages[p];
        page.fc_marked = true;
        page.armed = true;
        if FAULT_IN_ON_MARK {
            page.fault_in();
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
        let page = &mut self.pages[p];
        if page.unplugged {
            return;
        }
        page.fault_in();
        page.memfd = Memfd::Present(v);
        page.kvm_marked = true;
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
        let page = &mut self.pages[p];
        if !page.armed || page.unplugged {
            return;
        }
        page.fault_in();
        page.memfd = Memfd::Present(v);
    }

    /// A device writes `v` to page `p` and marks it afterwards (block I/O completion).
    pub fn device_write_then_mark(&mut self, p: usize, v: u8) {
        let page = &mut self.pages[p];
        if page.unplugged {
            return;
        }
        page.fault_in();
        page.memfd = Memfd::Present(v);
        page.fc_marked = true;
    }

    /// The balloon or free page reporting releases page `p`: `MADV_REMOVE` punches it (the
    /// handler gets a `remove` event and stops serving it), and it is marked.
    pub fn discard(&mut self, p: usize) {
        let page = &mut self.pages[p];
        if page.unplugged {
            return;
        }
        page.memfd = Memfd::Hole(HoleReads::Zero);
        page.fc_marked = true;
    }

    /// A discard that the kernel did not carry out: `MADV_REMOVE` on a range that does not cover
    /// a whole hugetlbfs page. Nothing changes for the guest.
    pub fn discard_ineffective(&mut self, p: usize) {
        let page = &mut self.pages[p];
        if page.unplugged {
            return;
        }
        if !DISCARD_MARKS_ONLY_IF_EFFECTIVE {
            page.fc_marked = true;
        }
    }

    /// virtio-mem unplugs page `p`: discarded, marked, inaccessible.
    pub fn unplug(&mut self, p: usize) {
        let page = &mut self.pages[p];
        page.memfd = Memfd::Hole(HoleReads::Zero);
        page.fc_marked = true;
        page.armed = false;
        page.unplugged = true;
    }

    /// virtio-mem plugs page `p` back: accessible again, still a zero hole.
    pub fn plug(&mut self, p: usize) {
        self.pages[p].unplugged = false;
    }

    /// `snapshot_layout`: the dirty state, residency, classification and reset, with the backend
    /// applying the result to its file. `race` injects guest writes at the two points where a
    /// running guest can interleave.
    pub fn snapshot(&mut self, race: Race) -> Layout {
        self.snapshot_ordered(race, RESIDENCY_AFTER_DIRTY)
    }

    fn read_dirty(&mut self) -> [bool; PAGES] {
        // KVM's log (which resets it) ORed with Firecracker's bitmap; unplugged pages are dirty.
        let mut dirty = [false; PAGES];
        for (page, dirty) in self.pages.iter_mut().zip(&mut dirty) {
            *dirty = page.marked() || page.unplugged;
            page.kvm_marked = false;
        }
        dirty
    }

    fn read_resident(&self) -> [bool; PAGES] {
        // `mincore` is not run on unplugged slots: they are reported not resident.
        std::array::from_fn(|p| !self.pages[p].unplugged && self.pages[p].resident())
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
        zero: std::array::from_fn(|p| layout.page_is_zero(p as u64 * 4096)),
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

    /// Any state satisfying `I ∧ L`, with any (gap-free) queue of pending layouts.
    fn any_state() -> State {
        let s = State {
            pages: std::array::from_fn(|_| any_page()),
            pending: std::array::from_fn(|_| any_layout()),
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
        match kani::any::<u8>() % 10 {
            0 => s.guest_write(p, kani::any()),
            1 => s.device_mark_ahead(p),
            2 => s.device_write_armed(p, kani::any()),
            3 => s.device_write_then_mark(p, kani::any()),
            4 => s.discard(p),
            5 => s.discard_ineffective(p),
            6 => s.unplug(p),
            7 => s.plug(p),
            8 => {
                s.apply_oldest();
            }
            _ => {
                s.snapshot(any_race());
            }
        }
    }

    /// Base case: both initial states satisfy `I ∧ L`.
    #[kani::proof]
    fn initial_states_satisfy_invariant() {
        let ring: [bool; PAGES] = kani::any();
        let s = State::restored(kani::any(), ring);
        assert!(s.invariant() && s.lemma());
        let s = State::booted(ring);
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

    /// A paused snapshot is also correct after a racy `dirty-pages` pass whose layout the backend
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

    /// Without rounding discards inward, a partially covered huge page is marked but neither
    /// punched nor written: a marked base hole if the handler never populated it.
    #[kani::proof]
    #[kani::unwind(5)]
    #[kani::should_panic]
    fn without_effective_discard_check_the_invariant_breaks() {
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
                5 => s.discard_ineffective(p),
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
            let mut s = if round % 2 == 0 {
                State::restored(std::array::from_fn(|_| rng.byte()), ring)
            } else {
                State::booted(ring)
            };
            assert!(s.invariant() && s.lemma());
            for _ in 0..(rng.next() % 12) {
                rng.step(&mut s);
                assert!(s.invariant(), "{s:?}");
                assert!(s.lemma(), "{s:?}");
            }
            s.snapshot(Race::default());
            s.apply_all();
            assert!(s.file_matches_guest(), "{s:?}");
        }
    }

    /// The two bugs the model encodes, reproduced on the scenarios that found them.
    #[test]
    fn test_known_bugs_are_caught_by_the_lemma() {
        // A restored virtqueue page: marked at activation, not written before the snapshot.
        let mut s = State::restored([7; PAGES], [false; PAGES]);
        s.pages[0].fc_marked = true; // mark without the fault-in
        assert!(!s.lemma());
        // What the layout would have done: the page is a dirty hole, so "zero".
        let mut buggy = s;
        buggy.pages[0].file = 0;
        assert_ne!(buggy.pages[0].file, buggy.pages[0].guest());
        // With the fault-in, the mark is safe.
        let mut s = State::restored([7; PAGES], [false; PAGES]);
        s.device_mark_ahead(0);
        assert!(s.lemma());
        s.snapshot(Race::default());
        s.apply_all();
        assert!(s.file_matches_guest());

        // A partial hugetlbfs discard on a never-populated restored page.
        let mut s = State::restored([9; PAGES], [false; PAGES]);
        s.pages[1].fc_marked = true; // marked though nothing was punched
        assert!(!s.lemma());
        let mut s = State::restored([9; PAGES], [false; PAGES]);
        s.discard_ineffective(1);
        assert!(s.lemma());
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
