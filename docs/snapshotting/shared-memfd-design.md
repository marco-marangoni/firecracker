# Shared memfd: design

Status: implemented (developer preview) as the `SharedMemfd` memory backend. The
user-facing documentation is `shared-memfd.md`; this document records the design
rationale. It predates the name: where it says "memory backend" as a mode, read
`SharedMemfd`.

## Goal

Customers running a UFFD page-fault handler need the guest memory shared with
that handler, and they need to be able to produce full and differential memory
snapshots, byte-for-byte identical to Firecracker's, without Firecracker writing
guest memory to disk. The same mechanism must work on first boot, when there is
no page-fault handler, and must be usable as the source side of a live migration
(dirty pages while running, a consistent final pass while paused).

Rather than introducing a second external actor next to the UFFD handler, this
design generalises the UFFD handler into a *memory backend*: one peer, one
socket, one handshake. A UFFD handler is a memory backend that also receives a
uffd. On first boot the same program is started and receives only the memfd.

Firecracker's part is deliberately small: allocate guest memory as a single
memfd, hand it over, and tell the orchestrator over the HTTP API which pages of
it constitute a snapshot. Copying bytes is the peer's job.

## Current behaviour this design relies on

- Guest memory is allocated by `memory::anonymous` (MAP_PRIVATE) unless a
  vhost-user block device is configured, in which case `memory::memfd_backed`
  creates a single sealed memfd (`F_SEAL_SHRINK|GROW|SEAL`) and maps every
  region MAP_SHARED at `offset = running sum of the preceding region sizes`
  (`src/vmm/src/vstate/memory.rs`; `src/vmm/src/resources.rs`,
  `VmResources::allocate_memory_regions`).
- The snapshot memory file has exactly that layout: `GuestMemoryMmap::dump` and
  `dump_dirty` walk regions in collection order, write plugged slots and `seek`
  over unplugged (virtio-mem) slots; the file size is the sum of all region
  sizes (`KvmVm::snapshot_memory_to_file`, `src/vmm/src/vstate/vm.rs`). With a
  single memfd, the memfd content *is* the full snapshot, for a booted microVM.
  (After a UFFD restore it is not: pages the handler has not populated are holes
  in the memfd while the guest-visible content is still the snapshot file's; see
  §9.)
- The virtio-mem hotplug region is currently allocated separately
  (`VmResources::allocate_memory_region`, from `build_microvm_for_boot`), which
  in the memfd case yields a second memfd. Only its guest address needs the KVM
  VM to exist; its size is known up front.
- `create_snapshot` (`src/vmm/src/persist.rs`) runs, in order: `save_state`
  (every device's `prepare_save`; virtio-block drains in-flight io_uring
  requests and `fsync`s, virtio-net returns parsed-but-unfilled RX buffers to
  the queue so they are parsed and marked again later), vmstate to file, then
  `snapshot_memory_to_file`. For `Diff` that is `KvmVm::get_dirty_bitmap`
  (`KVM_GET_DIRTY_LOG`, which clears KVM's log; `mincore` when
  `track_dirty_pages` is off) followed by `dump_dirty`, which writes every page
  dirty in either the KVM bitmap or Firecracker's own `AtomicBitmap` (device
  writes) at host-page (4 KiB) granularity and resets Firecracker's bitmap; on
  failure the KVM bitmap is folded back (`store_dirty_bitmap`) so no dirty
  information is lost. For `Full` it is `dump` followed by a reset of both
  bitmaps. Last, `mark_virtio_queue_memory_dirty` marks every activated device's
  queue pages dirty so that they are part of the *next* diff, because queue
  accesses at runtime do not go through the bitmap.
- **`Paused` freezes the whole VMM, not just the vCPUs.** `Vmm::pause_vm` only
  parks the vCPU threads, but in the `firecracker` binary
  `ApiServerAdapter::handle_request`
  (`src/firecracker/src/api_server_adapter.rs`) leaves the `EventManager` loop
  after a successful `Pause` and blocks on the API channel until `Resume`. No
  device fd, virtqueue eventfd, tap, rate limiter or timer is polled; the only
  code that runs on the VMM thread is API request handling. Consequently, once
  `snapshot/create` has returned, nothing in Firecracker reads or writes guest
  memory or virtqueues for the rest of the pause, and vmstate cannot diverge
  from memory. The remaining writers are the kernel completing io_uring requests
  issued by virtio-block before the pause (drained by `prepare_save` inside
  `snapshot/create`) and external vhost-user backends (an untracked,
  pre-existing gap). See `docs/snapshotting/snapshot-support.md`, "What `Paused`
  means".
- The UFFD protocol (`src/vmm/src/persist.rs`, `guest_memory_from_uffd`,
  `send_uffd_handshake`): Firecracker `connect()`s to a UDS, sends a JSON
  `Vec<GuestRegionUffdMapping>` plus the uffd via `SCM_RIGHTS`, then `forget`s
  the stream. The connection stays open for the VM's life (the handler uses it
  to notice Firecracker exiting) but Firecracker never reads it.
- `PUT /snapshot/create` requires `Paused` and today always answers 204. Other
  endpoints already return `200` with a JSON body through `VmmData` variants
  (`GET /vm/config`, `GET /balloon/statistics`).

What is missing for a memory backend: a single memfd covering all regions, a way
to hand it over (at boot as well as at restore), and an API that reports which
pages of the memfd a snapshot consists of.

## Design

### 1. Memory allocation: one memfd for everything

When a memory backend is configured, guest memory is memfd-backed and
MAP_SHARED, and all regions, including the hotplug region, live in one memfd
whose layout equals the snapshot file layout.

- `VmResources` owns the memfd (`Option<Arc<File>>` plus a next-offset cursor,
  or a small allocator object returned by `allocate_guest_memory` and reused by
  `allocate_memory_region`). The memfd is sized
  `DRAM + memory_hotplug.total_size` up front; DRAM regions are mapped first,
  the hotplug region is mapped at the next offset once its address is known.
- `memory::create` gains a base offset so a region can be mapped from the shared
  memfd at a non-zero offset.
- Invariant, asserted in unit tests: for every region, its memfd offset equals
  the offset `dump` writes it at. Regions are sorted by guest address and the
  hotplug region sits past the 64-bit MMIO hole, so it is always last.
- On restore all regions are known from `GuestMemoryState`, so
  `memfd_backed(mem_state.regions(), ..)` already yields one memfd.
- `GuestRegionMmapExt::discard_range` (balloon, virtio-mem unplug) uses
  `madvise(MADV_REMOVE)` on shared file mappings, which punches a hole into the
  memfd and frees the pages; `MADV_DONTNEED` only drops the PTEs of a shared
  mapping and releases nothing (the vhost-user memfd path inherited that).
  `MADV_REMOVE` rather than a direct `fallocate` so that a registered uffd still
  receives the `remove` event, which is how a backend learns that a range it has
  not populated now reads as zero instead of the snapshot file's content. Every
  discarded range is also marked dirty in Firecracker's bitmap, so that the next
  diff records the zeroed pages (this benefits Firecracker-written diffs too).
- Shared mappings have costlier page faults than anonymous memory, and THP on
  shmem depends on the host's `shmem_enabled`. This is the price of sharing, as
  for vhost-user today.

Without a memory backend, memory allocation is exactly as today. vhost-user
backends keep receiving the memfd through the vhost-user protocol; if both are
configured they share the same memfd.

### 2. One handshake: the UFFD handshake

There is no new protocol. The message Firecracker sends to a memory backend is
the existing UFFD handshake, byte for byte: a JSON array of
`GuestRegionUffdMapping`, one entry per region, sent with `SCM_RIGHTS` fds in a
single `sendmsg` (`send_uffd_handshake`, `src/vmm/src/persist.rs`).

```json
[
  {
    "base_host_virt_addr": 140000000000000,
    "size": 1073741824,
    "offset": 0,
    "page_size": 4096,
    "page_size_kib": 4096
  }
]
```

What differs is only which fds accompany it:

| Situation                             | fds             |
| :------------------------------------ | :-------------- |
| restore, `backend_type: Uffd` (today) | `[uffd]`        |
| restore, `backend_type: SharedMemfd`  | `[uffd, memfd]` |
| boot, `machine-config.mem_backend`    | `[memfd]`       |

- The message already carries everything a peer needs. `offset` is the region's
  offset in the memfd, which by §1 is also its offset in a snapshot file;
  `page_size` is the backing page size (4 KiB or the hugetlbfs size) as today.
  The plug state of virtio-mem slots is only relevant when copying and is
  returned with every dirty set (§7, §8); the total size is the sum of region
  sizes; how Firecracker tracks dirty pages (`KVM_GET_DIRTY_LOG` or `mincore`)
  is invisible to a peer that receives a page bitmap.
- The uffd, when present, is always first, so a handler written for today's
  protocol that reads a single fd keeps working even if it is handed the longer
  list. The memfd is always last. A peer knows which list to expect from how it
  was started; if it wants to check a single fd, `fstat` tells them apart: a
  memfd is a regular file, a uffd an anonymous inode without a file type.
  (`readlink /proc/self/fd/N` would work too, but handlers usually run in a
  chroot without `/proc`.)
- No version field. The handshake has been extended before (`page_size` was
  added, `page_size_kib` deprecated) by adding fields, and handlers deserialise
  with serde, which ignores unknown fields. Future extensions follow the same
  rule: add fields, append fds. Anything that cannot be done that way is a new
  `backend_type`, not a new version.
- A peer that received a uffd serves faults; a peer that received a memfd keeps
  it. Firecracker does not create a uffd at boot: memory starts zeroed and a
  uffd would only turn every first touch into a round trip.
- After the handshake Firecracker `forget`s the stream, exactly as today. It
  does not read from it and does not monitor it: nothing Firecracker does later
  depends on the backend being alive. The backend can still use the open
  connection to detect Firecracker exiting and obtain Firecracker's PID via
  `SO_PEERCRED`, as today.
- Trust and lifecycle: the backend is trusted (same as a UFFD handler today);
  connection failure fails boot/restore.

Consequences for the rest of the design:

- Implementation: `send_uffd_handshake` takes a slice of fds instead of one.
  That is the whole protocol change.
- Existing handlers become memory backends by reading one more fd. The example
  handlers in `src/firecracker/examples/uffd/` are extended, not duplicated
  (§9).
- `backend_type: SharedMemfd` does not select a protocol; it means "the `Uffd`
  backend, and also hand over the memfd" (§6 for the name).
- The choice is **not** persisted in the snapshot (`VmInfo` is untouched). A VM
  booted with a memory backend can be snapshotted and restored with `Uffd` or
  `File`, and vice versa. Consequences of the choice are confined to the running
  instance: memory backing, the fd list, and what `snapshot/create` does with
  memory.

### 3. Restore population

Populating from a snapshot file while sharing memory is not offered: a memory
backend at restore always receives a uffd and populates memory itself. A
MAP_PRIVATE mapping of the snapshot file cannot be handed out as an fd, and
shmem cannot lazily reflect another file, so this would need a copy or an
in-process fault handler. To be investigated if a need appears; it would not
change the handshake.

### 4. Consistency model: why there is no synchronisation

The snapshot is consistent if the memory bytes the peer copies correspond to the
vmstate Firecracker saved. Today that holds because vmstate save and memory dump
are one synchronous call. With a memory backend it holds because of the pause
invariant: `snapshot/create` requires `Paused`, and from the moment it returns
until `PATCH /vm Resumed` no code path in Firecracker writes guest memory or
virtqueues. The bytes in the memfd at the pages `snapshot/create` returned are
the bytes `dump`/`dump_dirty` would have written at that moment, and they stay
that way for the rest of the pause. The peer can therefore copy at its own pace,
as long as it finishes before the resume. There is no window to hold open, no
acknowledgement to wait for and no blocked VMM thread.

This is a statement about *writes to guest memory after `snapshot/create`
returns*, nothing broader. Firecracker is not idle while paused:
`snapshot/create` itself modifies guest memory (draining block I/O completes
reads into guest buffers) and reads and resets the dirty bitmaps to produce the
bitmap; `PUT /snapshot/dirty-pages` reads and resets the bitmaps again;
configuration requests read and modify device state. None of them modify guest
memory once `snapshot/create` has returned, which is all the argument needs.

The writers, and why each is quiescent after `snapshot/create` returns:

- vCPU threads: parked in their paused loop since `PATCH /vm Paused`; KVM does
  not run the guest, so the KVM dirty log is stable.
- Device emulation: lives on the VMM thread, which is not running the event loop
  while paused. Devices cannot parse descriptors, write frames or update used
  rings.
- Asynchronous block I/O: io_uring requests issued before the pause are
  completed by the kernel regardless of the event loop.
  `VirtioBlock:: prepare_save`, run by `save_state` inside `snapshot/create`,
  drains them and processes the completion queue, which is where the pages they
  wrote are marked dirty (`mark_dirty_mem_and_unwrap`). After `snapshot/create`
  nothing is in flight.
- vhost-user backends: external, untracked, unchanged from today.
- The memory backend itself: trusted not to write.

Two ordering rules follow, and the doc for the feature must state them:

1. The dirty set that accompanies a snapshot is the one returned by
   `snapshot/create` (§7). It is computed after `prepare_save`, so it includes
   the pages written by drained block I/O and excludes nothing that vmstate
   already reflects.
1. The standalone dirty-pages request (§8) is for pre-copy passes. It can be
   issued in any state and never loses pages (anything it misses because a
   completion has not been processed yet is marked later and shows up in a
   subsequent set), but it is not a substitute for the set returned by
   `snapshot/create`.

If the invariant is ever weakened (a future change that services device fds
while paused), snapshot consistency with a memory backend breaks silently. The
`INVARIANT` comment in `ApiServerAdapter::handle_request` and an integration
test (a paused VM with a backend must not have its RX ring or `rx_bytes_count`
change while tap traffic is injected, §10) exist to make that visible.

### 5. Boot API

`PUT /machine-config` gains a `mem_backend` field using the existing
`MemBackendConfig` shape:

```json
{
  "mem_backend": {
    "backend_type": "SharedMemfd",
    "backend_path": "/path/to/backend.sock"
  }
}
```

Only `SharedMemfd` is valid pre-boot (`File` and `Uffd` describe how to
*populate* memory and have no meaning on a fresh boot). Presence means:
memfd-backed memory (§1) and a memory backend handshake carrying the memfd,
performed in `build_microvm_for_boot` after all regions are mapped and
registered with KVM and before vCPUs start. Machine-config is the home because
this is a memory-allocation property like `track_dirty_pages` and `huge_pages`.
It is accepted from the JSON config file as well. Not persisted in the snapshot.

### 6. Restore API

`snapshot/load.mem_backend.backend_type` gains a third value:

```json
{
  "snapshot_path": "...",
  "mem_backend": {
    "backend_type": "SharedMemfd",
    "backend_path": "/path/to/backend.sock"
  }
}
```

- `File` and `Uffd`: today's behaviour, byte for byte.
- `SharedMemfd`: memfd-backed memory; a uffd is created and registered on the
  shmem mapping (`UFFDIO_COPY`/`UFFDIO_ZEROPAGE` are supported on shmem-backed
  VMAs in MISSING mode on Linux 5.10, `UFFD_FEATURE_MISSING_SHMEM`, and on
  hugetlbfs memfds); the UFFD handshake is sent with `[uffd, memfd]`. Fault
  serving on the backend side is the same code a UFFD handler runs today.

Nothing about the previous instance's backend is read from the snapshot; the
`backend_type` alone decides.

Since the handshake is the UFFD handshake, `SharedMemfd` is `Uffd` plus one fd,
and a boolean on the existing backend would express that just as well:

```json
{
  "mem_backend": {
    "backend_type": "Uffd",
    "backend_path": "...",
    "share_memory": true
  }
}
```

Trade-off: the boolean keeps the `backend_type` enum at its two natural values
(how memory is *populated*) and makes the relationship obvious, but has no
sensible meaning for `File` (must be rejected) and no counterpart at boot, where
there is no population and `machine-config.mem_backend` would carry a
`backend_type` that means nothing. The third enum value reads naturally in both
places. It is named `SharedMemfd` after the mechanism it selects (guest memory
is a memfd that is shared with the peer), in the style of `File` and `Uffd`,
rather than after the peer's role; an earlier draft called it `MemoryBackend`,
which read awkwardly inside `mem_backend`.

### 7. `PUT /snapshot/create` with a memory backend

*(Revised three times. The response format changed from a dirty bitmap plus an
`unplugged` range list plus a `populated` bitmap to two bitmaps that classify
every page; the backend path was then restricted to `Diff` snapshots, a `Full`
being written by Firecracker itself whether or not a backend is attached; and
the two bitmaps, first chunked and trailing-trimmed, then run-length compressed,
became Roaring bitmaps. The history is in the decisions below and in §14.)*

The rule: **Firecracker writes `Full` snapshots; the backend produces `Diff`
snapshots.** A `Full` with a backend attached is exactly a `Full` without one:
`mem_file_path` required, `dump` through Firecracker's mapping, 204. Reading
through the mapping faults in every page the backend has not populated (the
backend serves it from its snapshot file, as for any other fault), so the result
is complete and depends on nothing else, at the cost of populating the whole
memfd after a lazy restore. That cost is accepted: a `Full` is the recovery path
(lost `Diff` response) and the bootstrap, not the steady state, and it is the
same cost a `Full` after a UFFD restore has today.

Request, for a `Diff`:

```json
{
  "snapshot_type": "Diff",
  "snapshot_path": "/path/vmstate"
}
```

`mem_file_path` becomes optional in `CreateSnapshotParams`. For a `Diff` with a
memory backend connected it must be absent (400 otherwise: Firecracker does not
write guest memory in this mode); in every other case it must be present, as
today. The request carries no `mem_backend` field: whether a backend is
connected is a property of the instance, fixed at boot or restore, and repeating
it here would only add a way to get a 400. There are no other parameters.

Response: `200 OK` with

```json
{
  "snapshot_type": "Diff",
  "memory": {
    "total_size": 65536,
    "page_size": 4096,
    "bitmap_encoding": "roaring",
    "memfd_authoritative_pages": "OjAAAAEAAAAAAAIAEAAAAAAAAQAMAA==",
    "zero_pages": "OjAAAAEAAAAAAAMAEAAAAAgACQAKAAsA"
  }
}
```

(A 64 KiB guest, for the sake of a readable example: each set is one Roaring
array container, 16 and 20 bytes. Pages 0, 1 and 12 are to be copied from the
memfd; pages 8 to 11, an unplugged virtio-mem slot, are to be zeroed; every
other page is unchanged.)

Every page of the memory file is in exactly one of three classes, and the two
bitmaps name two of them:

- **memfd authoritative** (`memfd_authoritative_pages`): the content is in the
  memfd; the peer copies it.
- **zero** (`zero_pages`): the page must read as zero in the file the peer
  produces. How is the peer's business (write zeros, punch, or tell its storage
  the range is zero).
- **unchanged** (neither): not touched since the dirty state was last consumed.
  Not part of the diff. Firecracker has nothing to say about it and the memfd
  may not hold it (after a restore it may still be only in the peer's snapshot
  file); the file the diff is applied to already has it.

`total_size` is the size of a full memory file, all regions including the
hotplug region whether plugged or not; the peer creates the target at that size.
Both sets are Roaring bitmaps of page indices (file offset / `page_size`, the
host page size), disjoint, in the portable serialization format, then base64;
`bitmap_encoding` names the serialization so a peer fails cleanly if it ever
changes.

The classification is `dirty ∧ resident` → authoritative, `dirty ∧ ¬resident` →
zero, `¬dirty` → unchanged, where *dirty* is the union of KVM's log and
Firecracker's `AtomicBitmap` (every page written or discarded since the last
consumption, and every page of an unplugged virtio-mem slot) and *resident* is
`mincore(2)` over Firecracker's mapping, taken *after* the dirty state is read.
Nothing is aggregated or rounded: each page is classified on its own. Unplugged
slots are dirty and never resident, so they are zero pages; Roaring stores them
as one run per 65536-page container, a few bytes whatever the size of the
hotplug region.

Procedure, on the VMM thread, inside the paused API loop:

1. require `Paused`, as today;
1. validate `mem_file_path`, before anything is written;
1. `save_state` and write vmstate to `snapshot_path`, as today (this runs
   `prepare_save`: block drain, net RX buffer reset);
1. read the dirty state: `KVM_GET_DIRTY_LOG` (or `mincore`, when tracking is
   off) ORed with Firecracker's bitmap per plugged slot;
1. `mincore` over every plugged slot;
1. classify (`SnapshotMemoryLayout::classify`: `dirty & resident`,
   `dirty - resident`, then `optimize()` to pick run containers; one function
   shared with the benchmark), reset Firecracker's dirty bitmap;
1. `mark_virtio_queue_memory_dirty`, as today, so queue pages are in the next
   diff;
1. serialise: Roaring portable format then base64 per set, and return the body.

#### Reading and resetting the dirty state without losing a mark

There is no atomic read-and-reset step, and none is needed, because the two
sources of marks are consumed in two different ways that each cannot lose a
write; this is also exactly how `dump_dirty` has always consumed them.

- *KVM's log* is read with `KVM_GET_DIRTY_LOG`, which the kernel implements as
  an atomic fetch-and-clear per slot. vCPUs run concurrently with a
  `dirty-pages` request; a page written after the fetch is in the next log.
  Nothing on Firecracker's side resets this log separately for a `Diff`.

- *Firecracker's `AtomicBitmap`* is read in step 4 and cleared in step 6, which
  is not atomic. It does not need to be, because every writer of that bitmap
  runs on the VMM thread, the thread that is executing the request: block I/O
  completions (`mark_dirty_mem_and_unwrap`), virtio-net RX buffer parsing
  (`IoVecBufferMut`), balloon and virtio-mem discards (`discard_range`), and
  `mark_virtio_queue_memory_dirty` itself. An API request is handled between two
  iterations of the event loop, so no device handler can interleave with steps
  4–6. The guest can only make KVM's log dirty, never this bitmap.

  The one writer that is not on the VMM thread is device activation: a vCPU
  thread handling the guest's `DRIVER_OK` write calls `Queue::initialize`, which
  marks (and faults in) the rings, while holding the device's mutex, and sets
  the device activated before releasing it. If that lands between steps 4 and 6
  the marks are cleared, but step 7 locks each device in turn and re-marks the
  rings of every device that is activated at that moment, so it either waits for
  the activation to finish and re-marks, or runs before it and leaves the
  activation's own marks intact. The same race exists for `dump_dirty` today and
  is closed the same way; the model in `snapshot_layout_model.rs` has it as
  "rings are re-armed after every consumption".

What *is* racy with a running guest is the guest's own writes between reading
KVM's log and reading `mincore`, and between `mincore` and the reset, which is
why the order of the two reads matters (next section) and why the set is a
superset rather than a snapshot: the Kani harnesses
`operations_preserve_invariant` and
`without_residency_after_dirty_the_invariant_breaks` cover exactly those two
interleavings.

#### What changes when device I/O leaves the VMM thread

The argument above leans on one fact about today's Firecracker: every writer of
Firecracker's dirty bitmap runs on the thread that consumes it. Moving device
emulation to I/O threads removes that fact, and the design has to stop relying
on it rather than re-derive it per device. Four changes, in order of weight:

1. **Consume Firecracker's bitmap with an atomic swap.** `AtomicBitmap` already
   has `get_and_reset()` (a per-word atomic exchange). Step 4 becomes "swap each
   region's bitmap out and build the dirty set from the swapped words", and step
   6 disappears: a mark set after the swap belongs to the next set, exactly as
   with `KVM_GET_DIRTY_LOG`. The fold-back on error (`store_dirty_bitmap`) must
   then OR back both KVM's words and the swapped words. `dump_dirty` has the
   same read-then-reset and should get the same change.
1. **Mark after writing, never only before.** With a swap, "mark at t₁, consume
   at t₂, write at t₃" loses the write for good, and with a net I/O thread an RX
   buffer can be parsed between `prepare_dirty_tracking_reset` and the swap and
   filled afterwards. The fix is for `IoVecBufferMut` to mark the written range
   after the tap read (it holds the slices; this costs nothing). That makes the
   invariant a local property of each write with no ordering argument, makes
   `prepare_dirty_tracking_reset` unnecessary, and makes `fault_in_marked_range`
   unnecessary for RX buffers (a page marked after a write is resident). The
   same change is an improvement to the single-threaded code and should not wait
   for I/O threads.
1. **Virtqueue rings.** The used ring is written through a raw pointer without a
   mark; the re-mark of every activated device's rings after each consumption
   covers it, with or without I/O threads, because the re-mark is unconditional
   and happens after the swap. The fault-in at activation stays necessary (the
   `should_panic` harness in `snapshot_layout_model.rs` shows what breaks
   without it). The cleaner end state is for `Queue::add_used` to mark after
   writing, after which rings are ordinary pages and both the re-mark and the
   activation race described above go away.
1. **Make the pause invariant explicit.** `Paused` today means "vCPUs stopped
   and the VMM thread parked in the paused API loop", which is what lets the
   backend copy after `snapshot/create` with nothing writing guest memory. With
   I/O threads, pause must quiesce them: stop queue processing, drain in-flight
   completions (an io_uring block write landing after the layout was computed is
   the failure to design against), acknowledge, and only then run
   `create_snapshot`; resume reverses it. `prepare_save` already drains block
   devices; this generalises it into a precondition of `Paused`. It is the one
   item that is real engineering rather than a local change, and it is needed by
   every snapshot path, not only the memory backend.

Discards are already in the right shape (`discard_range` punches, then marks:
after the swap the mark lands in the next set, which zeroes the page; before it,
the page is a dirty hole in this set), and the `mincore`-after-dirty rule is
unaffected. The Kani model should gain device operations at the race points
(today only guest writes race), consumption as a swap, and the two variants of
item 2; the expectation is that mark-after-write proves and mark-ahead with a
concurrent device does not, which would be the formal reason for the change.

#### Invariant: dirty ⇒ memfd authoritative or zero

The two-bitmap format only works if a dirty page's content is never somewhere
else than the memfd. Everything that marks a page dirty *after* writing it
satisfies that trivially: KVM's log, block completions
(`mark_dirty_mem_and_unwrap`), `discard_range`. Two paths mark *before* writing,
because the write happens through a raw pointer that has lost its `vm-memory`
context: `GuestMemorySlice::new` (virtqueue rings, at activation and at every
`mark_virtio_queue_memory_dirty`) and `IoVecBufferMut::load_descriptor_chain`
(virtio-net RX buffers, marked when parsed, filled when a frame arrives). After
a UFFD restore such a page can be marked while still a hole in the memfd, its
content only in the backend's snapshot file; it would be reported
`dirty ∧ ¬resident`, "zero", and the backend would zero a page the guest sees as
content. This was the case the first version of the design worked around by
making the backend track what it had populated (§14).

Both paths now fault the range in right after marking it
(`fault_in_marked_range`: one volatile read per page). Through a UFFD mapping
that brings the snapshot content into the memfd; on a booted microVM it maps a
zero page for never-touched memory and is a plain load otherwise; on hugetlbfs
it installs Firecracker's huge PTE, which is what `mincore` reports there. The
cost is bounded by the ring sizes plus the RX buffers in flight, a few dozen
pages per device. The RX case is not a race: the tap write happens strictly
after the parse, so at the time of the read the page is either already the
guest's content or a hole that must become content before it is written. A unit
test punches a range, marks it through `GuestMemorySlice::new`, and checks
`mincore` reports it resident (`test_fault_in_marked_range`).

Two more properties the comment on the tracking task lists, and where they hold:

- *A write is eventually marked dirty, or its page is permanently dirty.* KVM
  marks on the fault after each reset; block marks on completion; the RX buffer
  is marked at parse time and re-marked after a reset by
  `prepare_dirty_tracking_reset` (§8); the rings are re-marked after every
  consumption by `mark_virtio_queue_memory_dirty`. Nothing writes guest memory
  outside those paths except vhost-user backends (the pre-existing gap).
- *`mincore` is taken after the dirty bitmaps.* A page written between the two
  reads is dirty in the next set and resident now: reported authoritative
  (correct, the memfd holds it) rather than zero. The other order could report a
  page as zero that was written just after `mincore` and before the log was
  read.

#### Decision: `Full` snapshots are Firecracker's, not the backend's

An earlier revision had the backend produce `Full` snapshots too, from a layout
computed as `resident → authoritative`, `discarded ∧ ¬resident → zero`, else
unchanged, where *discarded* was a second, never-consumed `AtomicBitmap` set by
`discard_range`. It was needed because a `Full` cannot use the tracked dirty
state (a previous diff consumed it), and `mincore` alone cannot tell a page the
balloon released since the last diff (a hole that must read zero) from a page
never touched since restore (a hole that must read the base's content);
`test_balloon_inflate_after_restore` caught a `Full` bringing released pages
back from the base before the bitmap existed.

That `Full` was not what its name promised: its "unchanged" pages meant "the
base's content", so the orchestrator needed the base to assemble a memory file
from it. It was a `Diff` over the whole history since restore, reconstructible
after the diffs had been consumed, and that was its only genuine use: recovering
from a lost `Diff` response. On a booted microVM it added nothing over a `Diff`.
Routing `Full` through Firecracker's own `dump` instead gives a file that
depends on neither the base nor any consumed state (the fault handler supplies
the base content, the kernel supplies zeros for discarded ranges, the memfd
supplies the rest), keeps the recovery contract as it is today, and removes: the
`discarded` bitmap and its write on every discard; the `Full` arm of the
classification; the handler's "copy unchanged gaps from the base" mode; and the
hugetlbfs question of what a partial discard means for a `Full`. What it costs
is the population of the memfd that a `Full` after a lazy restore causes, and a
`Full` blocking if the handler is dead. Both are the behaviour of a `Full` after
a UFFD restore today.

Two alternatives were considered for the lost-response case and set aside: an
`ack_previous` flag letting the next request fold the previous set back in
(small, but a new stateful protocol element for a rare event), and telling
orchestrators a lost response ends the snapshot lineage (simplest, but weaker
than what `File`/`Uffd` users have). The `Full` path needs neither.

Failure handling:

- If the vmstate write fails, nothing has been consumed and the error is
  returned as today.
- If reading the KVM log or `mincore` fails, the bitmaps are folded back with
  `store_dirty_bitmap`, as `dump_dirty` does today, and an error is returned.
- Once the body is handed to the API thread, the dirty set is consumed. If the
  HTTP client loses the response (disconnect mid-write), Firecracker cannot
  know, and the bitmap is gone; the orchestrator must treat a lost `Diff`
  response as "take a `Full` next". This is the same contract as losing a diff
  file today.
- The backend is never consulted, so a dead backend does not fail the request;
  the orchestrator finds out when it tries to use the memfd.

#### Decision: two class bitmaps, not dirty + populated + unplugged

The previous format shipped the dirty bitmap up to the last plugged slot, an
`unplugged` range list, and a `populated` bitmap at backing-page granularity,
and left the peer to combine them: dirty ∧ populated → copy, dirty ∧ ¬populated
→ zero, unplugged → zero, plus the peer's own record of what it had populated
through UFFD to route never-populated pages to the base. The two-bitmap format
was chosen over it, and over the alternative of shipping `dirty` and `mincore`
raw and letting the peer AND them, because the peer's rule becomes trivial and
needs no state (each bit says what to do; in particular the peer no longer needs
`PopulatedPages` for the copy, §9, which was the most subtle part of the
reference handler and the source of two bugs found by the tests), and because
only Firecracker holds both inputs at the same instant, so only it can classify
exactly.

#### Decision: Roaring bitmaps, no chunking

The first two-bitmap version reported `zero_pages` per caller-chosen chunk
(`zero_chunk_size`, default 512 KiB; a chunk was zero only if all its pages
were, and the zero pages of a mixed chunk were moved to the authoritative set,
which is safe since a hole reads as zero) and trimmed trailing zero bytes from
both bitmaps, so that an unplugged hotplug region at the end of the file cost
nothing. That bought a 128× smaller zero bitmap and a free unplugged tail, at
the price of a request parameter with validation and two error variants, the
"pages past the end are clear" rule, the mixed-chunk move, a `Diff` whose zero
set was approximate at the chunk boundary, and a special case (unplugged slots
in the middle of the file, or a region not ending at a chunk boundary) in every
explanation.

A compressed representation of the full page sets does better on every axis: no
granularity, no trim rule, no rounding, unplugged slots anywhere in the file for
free. Two were implemented and measured. The second version was PackBits (TIFF's
byte-oriented RLE) over full-length bitmaps: dependency-free, a dozen lines to
decode, bounded at +1/128 on dense input. The shipped version is
[Roaring](https://roaringbitmap.org), chosen over it because:

- it is a standard with a published portable format and implementations in every
  mainstream language, so a backend deserializes with a library call instead of
  carrying a codec of its own;
- the deserialized form is a set with constant-time `contains` and ordered
  iteration, so a backend copies straight from it (the reference handler's
  `runs()` iterates the two sets and never materialises a bitmap) and a fault
  handler could consult it directly;
- run containers make unplugged and released memory cost a few bytes per 256 MiB
  rather than two bytes per 4 MiB, and sparse sets cost 2 bytes per page rather
  than a byte per 8 pages plus RLE records;
- the dense worst case is the same 8 KiB per 65536 pages as a plain bitmap, by
  construction, where PackBits could reach +1/128.

The cost is a dependency in Firecracker (`roaring`, with `bytemuck` and
`byteorder`; 0.11 is needed for run containers) and in the example handler, and
an encode that is slower than a byte pass on dense input (2.8 ms against 1.9 ms
for the 25% churn case below), which is in the noise next to
`KVM_GET_DIRTY_LOG`.

Benchmark (a criterion harness written for this decision and not kept in the
tree, 1 GiB guest, synthetic dirty/resident inputs; "encode" is Firecracker
turning the two per-page inputs into the JSON body, "decode" is a peer parsing
it and counting pages per class; `current` is the dirty + populated + unplugged
format, `raw` ships `dirty` and `mincore` trimmed at 4 KiB, `packbits` the
previous version, `new` the shipped Roaring format):

| scenario               | format   | body (bytes) | FC encode | peer decode |
| :--------------------- | :------- | -----------: | --------: | ----------: |
| idle-diff              | current  |       87,494 |    862 µs |      206 µs |
| idle-diff              | raw      |       87,446 |    789 µs |      194 µs |
| idle-diff              | packbits |        3,560 |    367 µs |      106 µs |
| idle-diff              | new      |        1,549 |    625 µs |        3 µs |
| churn25-diff           | current  |       87,494 |  1,567 µs |      243 µs |
| churn25-diff           | raw      |       87,450 |  1,402 µs |      179 µs |
| churn25-diff           | packbits |       44,744 |  1,903 µs |      172 µs |
| churn25-diff           | new      |       43,873 |  2,822 µs |       86 µs |
| balloon50-diff         | current  |       87,494 |    893 µs |      206 µs |
| balloon50-diff         | raw      |       87,366 |    767 µs |      179 µs |
| balloon50-diff         | packbits |        2,660 |    703 µs |      103 µs |
| balloon50-diff         | new      |        1,549 |    895 µs |        4 µs |
| idle-diff+8x-unplugged | current  |       87,532 |    859 µs |      206 µs |
| idle-diff+8x-unplugged | raw      |      436,974 |  5,908 µs |    1,068 µs |
| idle-diff+8x-unplugged | packbits |       14,524 |  7,389 µs |      864 µs |
| idle-diff+8x-unplugged | new      |        2,145 |  7,402 µs |        9 µs |

Reading it: the body is the smallest of every format in every scenario (idle and
balloon: 1.5 KiB, 2 bytes per dirty page; 8 GiB unplugged: 2 KiB; dense churn:
the plain-bitmap size); peer decode is near zero because deserializing is a
`memcpy` per container and counting is a cardinality lookup, whereas every other
format walks bytes. The encode column includes building the Roaring sets from
the per-page inputs (the harness inserts runs the way `snapshot_layout` does)
and is dominated by that on the unplugged row, where both the harness and the
packbits variant touch 9 GiB-worth of input; the shipped code inserts an
unplugged slot as one range. The encode column excludes `KVM_GET_DIRTY_LOG` and
`mincore` (identical for all formats and larger than any of these numbers: 0.1
ms per GiB on hugetlbfs, 1.8 ms per GiB on tmpfs for `mincore` alone). The
chunked variant that preceded both measured 43.8 KiB in every diff scenario.

#### Decision: a bitmap, not a range list

An earlier version of this design returned the dirty set as a sorted list of
page-aligned `{offset, len}` ranges. Measurements on 1 GiB and 4 GiB guests
replaced it with the bitmap:

- Fresh allocations, page cache and even random first touches come out as a few
  hundred to a thousand ranges regardless of how much is dirty, because the
  guest allocator hands out physically contiguous pages. Random rewrites of
  memory a process already owns (the steady state of a database or a language
  runtime heap, and therefore the realistic pre-copy case) do not: 25% churn
  over a 512 MiB buffer produced 25 000 ranges and 0.9 MiB of JSON, 50% churn 34
  000 ranges and 1.2 MiB. The analytical worst case (every other page) is 131
  072 ranges and about 4.5 MiB of JSON per GiB of guest memory, built as a
  single `String` on the VMM thread.
- A bitmap is 32 KiB per GiB (43 KiB as base64), whatever the guest does. In the
  idle case that is larger than the range list (a few KiB), but it is noise next
  to the `KVM_GET_DIRTY_LOG` ioctl and a single `copy_file_range`, and a
  protocol whose cost depends on the guest's access pattern is the thing that
  cannot be fixed later.
- Encoding to base64 costs about 1 ms per GiB on the VMM thread (about half of
  that again for `serde_json` to scan the string), decoding is one library call
  in every language; an array of integers is larger, slower to parse, and `u64`
  does not fit a JSON number in JavaScript.
- Coarser tracking (reporting 512 KiB chunks, say) was considered and rejected
  as an API knob: KVM reports 4 KiB pages, so it saves nothing on Firecracker's
  side, and rounding out a sparse set multiplied the bytes to copy by 7–17× in
  the late pre-copy passes that decide downtime. With a bitmap the peer makes
  that trade itself, per pass, by merging runs across small gaps to save copy
  calls, and Firecracker has no opinion about it.
- A peer that keeps per-page state of its own (what it populated through UFFD,
  for serving faults) and a hugetlbfs peer that rounds decisions to its backing
  page do bitwise operations on same-shaped arrays. Against a range list they
  are a splitting loop.
- Everything that may change how the bitmap is produced
  (`KVM_CAP_DIRTY_LOG_RING`, `KVM_CAP_MANUAL_DIRTY_LOG_PROTECT2`,
  over-approximation) produces a bitmap naturally and needs no API change.

The bitmap covers the file up to the end of the last plugged slot, and no
further, and unplugged slots never have bits set. The hotplug region is always
the last region in the file (§1) and can be gigabytes of unplugged memory; a
bitmap over the whole file would carry 32 KiB of zeros per GiB of it in every
response, and reporting the unplug marks would add a burst of set bits after
every unplug. Ending at the last plugged slot rather than at the last set bit
was chosen so that the length is a function of the plug state alone: a peer can
size its buffers once, and a `Diff` with nothing dirty still looks like every
other `Diff`. The peer needs no region knowledge either way: whatever lies past
the end is clear. Leading zeros are *not* omitted (there is no `pages_offset`
field): the virtqueue pages live in low memory and are re-marked dirty after
every reset, so the first bytes are never zero in practice, and such a field
could not be added later without breaking clients that predate it, so the
decision is made now, against. `pages` is omitted altogether for a `Full`: it
would be all ones outside `unplugged`, which the peer can derive from
`total_size` and `unplugged` alone.

`unplugged` stays a range list: it is a "zero this" instruction rather than
dirty information, and it is a handful of huge extents. A binary response
(`Accept: application/octet-stream`, which `micro_http` already parses into a
`MediaType`) can be added later for peers that measure the base64 cost; it does
not change the JSON contract.

### 8. `PUT /snapshot/dirty-pages`: pre-copy passes

```json
PUT /snapshot/dirty-pages
{}
```

Response: `200 OK` with the same `memory` object as §7, or an error. There are
no parameters; an empty body and `{}` are both accepted, and unknown fields are
rejected, so that parameters can be added later.

Semantics:

- Callable in `Running` or `Paused`. Consuming: the same computation as steps
  4–6 of §7 for `Diff`, followed by `mark_virtio_queue_memory_dirty`.
- Before resetting, Firecracker calls a new
  `VirtioDevice::prepare_dirty_tracking_reset` hook on every activated device.
  virtio-net marks RX buffers dirty when it *parses* them
  (`IoVecBufferMut::load_descriptor_chain`), not when a frame is later written
  into them; a buffer parsed before a reset and filled after it would otherwise
  never show up in a later set. The hook does for virtio-net what `prepare_save`
  does (return parsed, unfilled descriptors to the queue) without the
  snapshot-only side effects of `prepare_save` on other devices (vsock
  connection reset, block drain and `fsync`). Any future device that marks
  memory dirty ahead of writing must implement it.
- Gives no atomicity with respect to concurrent guest or device writes: a page
  can be modified after the set was computed and before the peer copies it. It
  is then dirty again and will be in the next set. This is the intended use:
  iterative pre-copy, with `PATCH /vm Paused` + `snapshot/create` as the final
  pass.
- **Rejected with 400 when no memory backend is connected.** See the decision
  below.

`PUT` rather than `GET` because the request has a side effect (it consumes the
dirty set); the body is a JSON object so that fields can be added. The path
names what is returned (pages) rather than how it is encoded.

#### Decision: dirty pages only with a memory backend

The endpoint consumes the dirty bitmaps. Without a memory backend, Firecracker
is the only party that can turn those bitmaps into bytes (nobody else can read
anonymous MAP_PRIVATE guest memory), so a consuming call would silently punch
holes in the next Firecracker-written `Diff` snapshot with no way for
Firecracker to compensate. There is also no consumer for the result: the bitmap
describes offsets into a memfd that does not exist in that configuration.

Options considered:

- Allow it anyway and document the hazard: rejected, it is a foot-gun with no
  use case.
- Offer a non-consuming "peek" variant (take the KVM log, union with
  Firecracker's bitmap, fold the union back into Firecracker's bitmap so nothing
  is lost, return the bitmap): technically simple and safe, but still has no
  consumer without a backend. It can be added later as `{"consume": false}` if a
  monitoring use case appears (for example, estimating the size of the next diff
  before pausing).
- Tie the endpoint to the backend: chosen. With a backend connected Firecracker
  never writes guest memory itself, so the bitmaps have exactly one consumer and
  ownership is unambiguous.

### 9. Backend implementation (reference)

There is no separate reference backend. The existing example handlers in
`src/firecracker/examples/uffd/` already parse the handshake and already run a
poll loop over the uffd and the UDS (`uffd_utils.rs`); they are extended to be
memory backends:

- `uffd_utils.rs` receives all fds from the handshake instead of one
  (`Handshake`). The uffd path is built if a uffd was received; the memfd is
  kept if one was received. A handler started for a boot receives only the memfd
  and runs the poll loop without a fault source. Command line:
  `handler <uffd_socket> [<mem_file>] [--control-sock <path>]`; the memory file
  is optional because a boot has nothing to populate from.
- A control socket of the handler's own, unrelated to Firecracker, on which the
  orchestrator (the test framework) sends
  `{"Copy": {"mem_path": "...", "memory": <the object returned by Firecracker>}}`
  (one request per connection). The handler applies the layout to `mem_path`
  (creating the file at `total_size`, merging into an existing file otherwise):
  authoritative runs are copied from the memfd (`copy_file_range` with a
  read/write fallback), zero runs are written as zeros, the rest is left alone.
  It replies `{"Done": {"success", "message", "copied_bytes", "zeroed_bytes"}}`.
  This is one module shared by all example handlers.

The split reflects the real deployment: the orchestrator talks HTTP to
Firecracker and whatever it likes to its backend; Firecracker talks to the
backend once, at the handshake. The existing handlers keep working unchanged
when started as plain UFFD handlers.

#### Where a page's bytes come from

The original version of this document assumed the memfd is always the source.
That holds for a booted microVM, but not after a restore: the memfd starts
empty, and a page only holds content once the handler has served a fault for it.
For every other page a read through Firecracker's mapping would fault and the
handler would serve the snapshot file, which is therefore the guest-visible
content and what Firecracker's own `dump` writes (and, with a backend attached,
still does write for a `Full`: the fault handler is how it gets those bytes).

With the two-bitmap format and the "dirty ⇒ authoritative or zero" invariant
(§7) the handler's rule for a diff is the bitmaps, nothing else:

| Page                           | Read from            |
| :----------------------------- | :------------------- |
| in `memfd_authoritative_pages` | memfd                |
| in `zero_pages`                | zero                 |
| neither                        | not part of the diff |

`copy_pages` decodes both bitmaps (`DecodedLayout`), walks the file in page
order producing runs of the same class (`PageClass::{Authoritative, Zero}`;
unchanged pages are the gaps), and copies or zeroes each run. Zeros are written,
never punched: to `rebase-snap` a hole in a diff file means "not in the diff",
whereas a released page *is* in the diff and must overwrite the base's old
bytes, which is also what Firecracker's own `dump_dirty` does. Since the memfd
and the snapshot file share one layout, all of this is a matter of offsets. The
handler's `PopulatedPages` is gone: it existed only for the copy decision (fault
serving never consulted it; `UFFDIO_COPY` reports `EEXIST` for a page already
populated, and an unregistered range faults no more). The `remove` handler now
just unregisters.

#### Discards: balloon and virtio-mem

`GuestRegionMmapExt::discard_range` used `madvise(MADV_DONTNEED)` for shared
file mappings, which only drops Firecracker's page table entries and releases
nothing (the vhost-user memfd path inherited that, and the balloon never
reclaimed host memory there). It now uses `madvise(MADV_REMOVE)` for shared file
mappings, which punches a hole into the memfd: the pages are freed and read as
zero through every mapping, exactly what `MADV_DONTNEED` achieves for anonymous
memory. `MADV_REMOVE` rather than `fallocate(PUNCH_HOLE)` on the fd, because it
goes through `userfaultfd_remove()` and the registered handler keeps receiving
the `remove` event (Firecracker's `madvise` blocks until the handler has read
it). That event is what lets the handler stop serving the range from its
snapshot file; the recommended handling is to unregister the range, after which
the kernel serves zero pages for it without the handler. A handler that ignores
`remove` events, or keeps a punched range registered while its bitmap still says
the pages are populated, produces copies that differ from the guest's view.

`discard_range` also marks every discarded range dirty in Firecracker's bitmap,
so that the next `Diff` (Firecracker-written or backend-copied) records the
zeroed pages instead of leaving the pre-release bytes in a merged file. This
needs `track_dirty_pages`: a `mincore`-based diff only sees resident pages and
cannot express "this page became zero".

### 10. Byte-for-byte identity: how it is demonstrated

With a memory backend connected Firecracker never writes a memory file, so a
Firecracker-made reference cannot be produced from the same VM in an integration
test. Identity is therefore established in two layers.

Rust unit tests (`src/vmm/src/vstate/memory.rs`, `persist.rs`), where both
implementations are driven from identical inputs:

- Given the same `(kvm_bitmap, firecracker_bitmap, plugged, residency)` input,
  applying the `Diff` layout (copy authoritative, zero the zero pages, leave the
  rest) onto a base file yields `dump_dirty`'s output over the same base
  (property-style test over random bitmaps, including a trailing slot whose page
  count is not a multiple of 64, unplugged slots and punched plugged pages,
  `test_dirty_layout_matches_dump_dirty`).
- The classification itself: written pages resident, punched pages not,
  re-touched pages resident again, each page on its own; unplugged slots are
  zero pages and a 1 GiB unplugged region is under 32 bytes on the wire
  (`test_layout_classifies_resident_and_holes`,
  `test_layout_unplugged_slots_are_zero`); `classify` against a hand-built
  layout, the wire format of the documentation example byte for byte, and
  malformed input (`vmm_config::snapshot::tests`).
- `discard_range` on a shared hugetlbfs mapping rounds inward to the huge page,
  freeing and marking exactly what `MADV_REMOVE` frees
  (`test_discard_range_on_hugetlbfs_memfd_rounds_inward`).
- Request validation: `mem_file_path` is required or forbidden per snapshot type
  and backend presence, checked before anything is written
  (`persist::tests::test_create_snapshot_params_validation`).
- The invariant: marking a range through `GuestMemorySlice::new` faults it in
  (`test_fault_in_marked_range`).
- The memfd offset of every region equals the offset `dump` writes it at, with
  and without a hotplug region.

Integration tests (`tests/integration_tests/functional/test_memory_backend.py`;
framework support in `tests/framework/utils_uffd.py` (`UffdHandler` with a
control socket and `copy()`), `Microvm.mem_backend`,
`Microvm.spawn_mem_backend`, `Microvm.dirty_pages`, `make_snapshot` branching on
an attached backend, `restore_from_snapshot(mem_backend=True)`,
`basic_config(mem_backend=<handler>)`, and `http_api.request` accepting `200`
for `PUT`). The framework plays the orchestrator: it calls `snapshot/create`,
forwards the body to the backend's control socket. Definitions: `M_full(t)` is
the backend's copy of all plugged pages; `M_diff(t0,t1)` is the backend's sparse
file of the pages returned by a `Diff` `snapshot/create` at `t1`; `rebase` is
the existing `rebase-snap` tool / `Snapshot.rebase_snapshot`.

1. First diff of a booted microVM: boot with a backend, run a workload, pause,
   `snapshot/create Diff` into a fresh file → `M(t0)`, a complete memory file
   since unchanged pages are zero after a boot. Assert size `total_size`, that
   it equals Firecracker's own `Full` of the same state, and that Firecracker
   restores from it (`File` backend) with a healthy guest.
   (`test_boot_diff_snapshot_restores`)
1. Diff self-consistency (the backend-side analogue of
   `test_snapshot_basic.py::test_cmp_full_and_first_diff_mem`): resume, run a
   workload, pause, `snapshot/create Diff` → `M_diff(t0,t1)`; still paused,
   `snapshot/create Full` → `M_full(t1)`. Assert
   `rebase(M_full(t0), M_diff) == M_full(t1)`. Any dirty page missed by the
   bitmap computation shows up as a mismatch. (`test_diff_self_consistency`)
1. Chains: several diffs rebased onto the base equal a final full copy, and the
   rebased result restores with a healthy guest. (`test_diff_chain_restores`)
1. Pre-copy: while running a workload, issue several `dirty-pages` requests and
   copy each set; pause; `snapshot/create Diff`; copy. The result must equal a
   `Full` taken right after. Exercises the `prepare_dirty_tracking_reset` hook
   under ssh traffic. (`test_precopy_dirty_pages`,
   `test_dirty_pages_are_consumed`)
1. Pause invariant: pause a VM with a backend, take `M_full`, inject tap traffic
   (ssh connection attempts) for a few seconds, take another `M_full` while
   still paused. Assert the two copies are identical and the net device's RX
   metrics stayed at zero. (`test_pause_invariant`; reading the RX used index
   from the memfd, as first planned, is not needed: comparing whole copies is
   stronger.)
1. Cross-check against a Firecracker-made snapshot: restore one VM with a
   backend and one without from the same base snapshot, keep both paused, take a
   backend `Diff` rebased onto the base, a `Full` written by Firecracker through
   the backend (every never-populated page faulted in), and a classic `Full`,
   compare. Correction: the two are *not* byte-identical, because KVM writes the
   kvmclock pages (wall clock, per-vCPU time info) with the current time when
   their MSRs are set at restore. The test computes the pages at which the
   classic copy differs from the base (a handful) and requires every difference
   of the backend copy, to the base and to the classic copy, to be within that
   set. (`test_cross_check_with_firecracker_snapshot`)
1. Balloon: (a) boot with a backend, `M_full(t0)`, inflate while running,
   `Diff`, `Full`, `rebase == Full`, which needs the discarded ranges both
   punched (zeros in `Full`) and marked dirty (present in `Diff`); (b) same
   after a restore with a backend, where the handler receives `remove` events
   for a mix of populated and never-populated pages, then deflate, reuse the
   memory and restore from the result (with 2M pages, only whole huge pages the
   guest released in one range are freed and marked, so the test only checks
   identity there); (c) Firecracker's RSS drops on inflate with memfd-backed
   memory. (`test_balloon_inflate_at_boot`,
   `test_balloon_inflate_after_restore`, `test_balloon_inflate_reclaims_memory`;
   the plain-memory counterpart of (a) is
   `test_balloon.py::test_balloon_inflate_marks_pages_dirty`.)
1. virtio-mem: `total_size` includes the hotplug region; unplugged slots are
   zero pages and zero in the file, the zero bitmap of a 768 MiB file with 512
   MiB unplugged is 192 bytes on the wire, a plugged slot is authoritative where
   touched; the result restores with the plugged size intact.
   (`test_virtio_mem_unplugged_slots`, `test_virtio_mem_unplug_after_use`, which
   also checks a replugged slot is fully classified as authoritative or zero)
1. Variants: hugetlbfs 2M for 1–4, the restore-with-backend model change and the
   balloon-after-restore test (`PAGE_CONFIGS`; restores of 2M snapshots go
   through the UFFD handler since `File` rejects them);
   `track_dirty_pages=false` (`mincore` diff of a booted VM rebases onto the
   base correctly; it cannot record discarded pages, §9); x86 with >4 GiB, whose
   two DRAM regions are contiguous in the bitmap (`test_two_dram_regions`). Not
   yet run: aarch64.

Compatibility tests (all in the same file unless noted):

- The UFFD protocol is unaffected: existing `test_uffd.py` runs unchanged with
  the existing handlers.
- Memory backend at boot (no uffd) and at restore (uffd) with the same backend
  binary, for both `on_demand` and `fault_all`; a backend-produced snapshot
  restores with `Uffd`, `File` and `SharedMemfd`.
- Model change across restore: boot with backend → snapshot → restore with
  `Uffd` and with `File`; restore with `Uffd`/`SharedMemfd` → snapshot → restore
  with `SharedMemfd`; in each case the resulting VM snapshots correctly in its
  new mode.
- Negative: `snapshot/create Diff` with `mem_file_path` and a backend → 400;
  `Full` without `mem_file_path`, backend or not → 400; `dirty-pages` without a
  backend → 400; unknown fields on either endpoint → 400 without consuming or
  writing anything; `machine-config.mem_backend` with `File`/`Uffd` → 400;
  unreachable backend fails `InstanceStart`; backend process killed → VM keeps
  running, `snapshot/create Diff` still returns the layout, the orchestrator's
  copy fails.

### 11. Security and operational notes

- The backend receives a read/write fd to all guest memory; it must run in the
  same jail/user as a UFFD handler does today. Socket paths are relative to the
  jailer chroot.
- Seccomp: the VMM-thread filter already allows `connect` and `sendmsg` for the
  UFFD handshake; nothing new is needed since Firecracker never reads the
  backend socket. `copy_file_range` is only used by the backend.
- The memfd is sealed against resize; Firecracker keeps its own fd, so backend
  death never invalidates guest memory.
- API response size: at most 44 KiB of base64 per GiB of guest memory per set (a
  dense random dirty set), a few KiB for an idle guest, under 100 bytes per GiB
  of unplugged or released memory (§7). The API server's payload limit applies
  to requests only; responses are streamed as today for `GET /vm/config`.
- Seccomp for discards: the VMM thread's `madvise` rule is unrestricted, so
  `MADV_REMOVE` needs no filter change.
- Metrics:
  `mem_backend.{handshake_fails, dirty_pages_requests, dirty_pages_fails}` and
  the existing snapshot latency metrics. Not implemented yet (§12).

### 12. Work breakdown

The branch `snapshot-improvements` holds everything below as one uncommitted
change. The split is the order in which it should land, each item mergeable on
its own and reviewable in isolation; items marked *(no API)* change nothing a
user can see.

**Landed on the branch**

1. *(no API)* Single memfd for DRAM and hotplug: `MemfdBacking`,
   `memory::create` with a base offset, `VmResources::allocate_guest_memory`
   returning the backing for `allocate_memory_region` to continue from;
   offset-invariant unit test.
1. *(no API)* Discards on shared memory: `discard_range` uses `MADV_REMOVE` for
   shared file mappings, rounds a hugetlbfs range inward to the backing page
   before punching and marks exactly what it punched; unit tests for the hole,
   the marks and the rounding. Also fixes balloon reclaim for vhost-user.
1. *(no API)* Fault-in on mark-ahead: `fault_in_marked_range` after
   `GuestMemorySlice::new` and in `IoVecBufferMut::append_descriptor_chain`;
   unit test. (Superseded for RX buffers by the first item of the next list.)
1. *(no API)* Layout computation: `GuestMemorySlot::for_each_dirty_batch` as the
   single definition of "which pages a diff contains", shared with `dump_dirty`;
   `mincore_resident`; `SnapshotMemoryLayout::classify` as the single definition
   of the three classes; the Roaring serde adapter; property-style identity
   tests against `dump_dirty` over random bitmaps and residency, including
   fold-back on error; the `roaring` dependency.
1. *(no API)* The Kani model `vstate/snapshot_layout_model.rs`: nine harnesses
   (base case, inductive step, paused and racy-then-paused correctness, three
   rule checks, two backend-misuse checks), run by `test_kani.py`.
1. Handshake and boot: `send_uffd_handshake(&[RawFd])`, `uffd_mappings`,
   `MachineConfig.mem_backend` (only `SharedMemfd`), handshake in
   `build_microvm_for_boot` after all regions are registered with KVM,
   `Vmm.mem_backend_attached`; swagger.
1. `snapshot/load` with `backend_type: SharedMemfd`: memfd-backed memory, uffd
   registered on the shmem mapping, handshake with `[uffd, memfd]`; swagger.
1. `PUT /snapshot/create Diff` with a backend: `mem_file_path: Option`, the two
   rejections, `KvmVm::snapshot_memory_layout`, `VmmData::SnapshotMemory`, 200
   body; `Full` unchanged; swagger.
1. `PUT /snapshot/dirty-pages`: `VmmAction::GetDirtyPages`, `Vmm::dirty_pages`,
   `VirtioDevice::prepare_dirty_tracking_reset` with the virtio-net
   implementation; swagger. (The hook goes away with mark-after-write, next
   list.)
1. Example handlers: `Handshake` with all fds, `fstat`-based fd classification,
   optional memory file, `copy_pages` from the two Roaring sets, control socket,
   unit tests.
1. Python framework and integration tests (§10); docs: `shared-memfd.md`, this
   document, `snapshot-support.md`,
   `handling-page-faults-on-snapshot-resume.md`, `ballooning.md`, CHANGELOG.

**Next, before leaving developer preview**

1. *(no API)* Mark after writing in `IoVecBufferMut` (§7, "What changes when
   device I/O leaves the VMM thread", item 2); then remove
   `prepare_dirty_tracking_reset` and the RX-buffer call to
   `fault_in_marked_range`. Extend the Kani model with device operations at the
   race points and a `should_panic` harness for mark-ahead under concurrency.
1. *(no API)* Consume Firecracker's bitmap with `get_and_reset()` in both
   `snapshot_layout` and `dump_dirty`; fold back the swapped words on error.
   Model consumption as a swap in Kani.
1. Metrics: `mem_backend.*` (§11).
1. aarch64 run of the integration tests.
1. Decide whether Firecracker's own `Diff` keeps writing zero pages for
   discarded ranges on anonymous memory (introduced by the discard change;
   correct, larger diff files after an inflate).
1. Free page reporting with a backend on hugetlbfs: untested; it is the way to
   reclaim there, since plain inflation frees only huge pages a single release
   covers.

**Later, with the I/O-thread work**

1. Quiesce I/O threads as part of `Paused` (§7, item 4): stop queue processing,
   drain in-flight completions, acknowledge before `create_snapshot`. A
   precondition for every snapshot path.
1. *(no API)* `Queue::add_used` marks after writing; remove the per-consumption
   ring re-mark and the activation special case.

### 13. Decisions taken

- The backend is trusted; connection failure at boot/restore is fatal.
  Firecracker never reads the backend socket after the handshake and does not
  monitor it.
- One handshake, the existing UFFD one, unchanged; a memory backend differs from
  a UFFD handler only by the fds it receives (uffd first, memfd last). No
  version field; extensions are additive fields and appended fds. Nothing about
  the choice is persisted in the snapshot.
- No synchronisation between Firecracker and the backend at snapshot time. The
  pause invariant of the `firecracker` binary is what makes this correct, and it
  is documented and tested as such.
- Dirty information is exchanged over the HTTP API as two bitmaps that classify
  every page of the memory file: `memfd_authoritative_pages` (dirty ∧ resident)
  and `zero_pages` (dirty ∧ ¬resident), as Roaring bitmaps of host-page indices
  in the portable format, base64-encoded, in the `snapshot/create` response
  (final, consistent set) and from `PUT /snapshot/dirty-pages` (pre-copy,
  consuming, any state). A range list was the first choice and was replaced
  after measuring it (§7); a dirty bitmap plus `unplugged` list plus `populated`
  bitmap was the second and was replaced by the two class bitmaps after
  measuring those; chunked, trailing-trimmed class bitmaps were the third,
  PackBits-compressed full-length ones the fourth, and Roaring replaced both
  after measuring (§7). The authoritative set is a superset of the modified
  resident pages, not promised to be exact.
- Firecracker upholds "dirty ⇒ memfd authoritative or zero" by faulting in every
  range it marks dirty ahead of writing it (`fault_in_marked_range`), and takes
  `mincore` after reading the dirty state. On hugetlbfs, `discard_range` frees
  and marks whole huge pages only, as `MADV_REMOVE` does.
- Firecracker writes `Full` snapshots, backend or not; only a `Diff` with a
  backend goes to the backend: `PUT /snapshot/create Diff` then never writes
  memory, takes no `mem_file_path`, and answers 200 instead of 204.
- `PUT /snapshot/dirty-pages` is rejected without a memory backend. A
  non-consuming variant is possible later if a use case appears.
- A lost `Diff` response is the orchestrator's problem; it falls back to `Full`.
- Restore with a backend always uses a uffd for population; file population with
  sharing is deferred.
- Firecracker tells the backend where the bytes of every *changed* page come
  from (memfd or zero); for unchanged pages the backend knows (zero after a
  boot, the base after a restore). Earlier versions of this design had the
  backend decide from its own record of what it had populated through UFFD,
  because a dirty page could still be unpopulated; the invariant above removed
  that case. Populating every dirty page eagerly (`MADV_POPULATE_READ` over the
  whole set) was rejected as too expensive for a lazy restore; faulting in only
  the few pages marked ahead of a write is the cheap version of the same idea.
- Balloon/virtio-mem discards punch holes into the memfd (`MADV_REMOVE`, so the
  uffd `remove` event is preserved) and are marked dirty.
- The peer cannot rely on finding those holes in the memfd itself. On tmpfs
  `lseek(SEEK_DATA/SEEK_HOLE)` reports them; on hugetlbfs nothing does: `lseek`
  is `default_llseek` and reports the whole file as data, `mincore` on the
  *peer's* mapping reports the peer's page tables (so an untouched page is
  "absent" whether it is a hole or not), `cachestat` returns `EOPNOTSUPP`,
  `FIEMAP` is not implemented. (An earlier version of this document claimed
  `mincore` worked for the peer on both; it does not on hugetlbfs.) Firecracker
  therefore folds residency in *its* mapping into the classification (§7). On
  hugetlbfs `mincore` is per huge page: a dirty 4 KiB page in a resident huge
  page is authoritative, one in a non-resident huge page is zero; both are
  right.

### 14. Corrections made in retrospect

What the implementation taught us relative to the first version of this
document:

- The memfd is not always the source of a page's bytes. After a UFFD restore,
  pages the handler has not populated are holes in the memfd while the guest
  sees the snapshot file's content. The handler has to track what it populated
  and read the rest from its snapshot file (§9). This was the only gap that
  affected correctness.
- Balloon and virtio-mem discards on shared memory were declared out of scope
  and are now in: `MADV_DONTNEED` released nothing on a memfd, and a backend
  needs the `remove` event to know that a punched, never-populated page reads
  zero rather than snapshot bytes. Discarded ranges are also marked dirty, which
  the original design missed and which also affects Firecracker-written diffs
  (§9).
- Two paused restores of the same snapshot are not byte-identical: KVM writes
  the kvmclock pages at restore. The cross-check test tolerates exactly those
  pages (§10).
- `/proc` is not available to a jailed handler; fd classification uses `fstat`
  (§2).
- The `backend_type` value is `SharedMemfd`, named after the mechanism like
  `File` and `Uffd`; `MemoryBackend` read badly inside `mem_backend` (§6).
- `PUT` requests can answer `200` with a body; the test framework's HTTP client
  had to learn that.
- The pause-invariant test compares two full copies taken around injected
  traffic rather than reading virtqueue state out of the memfd, which is both
  simpler and stronger (§10).
- `pages` and uffd `remove` events are 4 KiB granular regardless of the backing
  page size, so a handler on hugetlbfs must make its source decision per backing
  page and round `remove` ranges inward. The 2M balloon test caught the example
  handler stepping `page_size` from an unaligned range start and missing a
  source change at the next huge-page boundary.
- The first version of this document exchanged dirty information as a list of
  page-aligned ranges. Measuring real dirty sets showed the list to be tiny when
  the guest allocates fresh memory and to explode when a process rewrites memory
  it already owns, which is the pre-copy steady state; it was replaced by a
  fixed-size bitmap (§7).
- The second version exchanged that bitmap up to the last plugged slot, an
  `unplugged` range list, and (later) a `populated` bitmap from `mincore`, and
  had the backend combine them with its own record of what it had populated
  through UFFD. Three things replaced it with the two class bitmaps of §7: the
  backend's rule needed per-page state and had two bugs found only by tests (a
  wrong source at a huge-page boundary; punching holes into a diff, which
  `rebase-snap` reads as "absent"); a backend `Full` after a consumed diff on a
  restored microVM copied balloon-released pages back from the base, because
  nothing remembered the discard once the dirty mark was gone; and the measured
  formats (§7) showed the class bitmaps to be half the size and 2–4× cheaper on
  both sides.
- The third version chunked `zero_pages` at a caller-chosen granularity and
  trimmed trailing zero bytes from both bitmaps, to keep the zero bitmap small
  and an unplugged tail free. A compressed representation of the full page sets
  achieves both without a parameter, a trim rule or any rounding, and handles
  unplugged slots anywhere in the file; `zero_chunk_size`, its validation and
  `DirtyPagesParams` are gone, `dirty-pages` takes no body again, and
  `bitmap_encoding` names the serialization. PackBits over full-length bitmaps
  was implemented first (dependency-free, a dozen lines to decode); Roaring
  replaced it for being a standard with libraries everywhere, for constant-time
  membership on the decoded form, and for being smaller still in every scenario
  measured (§7), at the cost of one dependency.
- "Dirty ⇒ memfd authoritative" did not hold for pages marked ahead of a write
  (virtqueue rings, RX buffers) after a UFFD restore. Faulting those ranges in
  when they are marked (`fault_in_marked_range`) made it hold, and made the
  backend's `PopulatedPages` unnecessary: that record was "memfd is
  authoritative here", fed by the handler's own populates and by `remove`
  events; residency now covers the former and the dirty mark `discard_range`
  leaves covers the latter, from the other end of the same events.
- The third version had the backend produce `Full` snapshots from a layout that
  needed a persistent `discarded` bitmap in Firecracker (a `Full` cannot use the
  consumed dirty state, and residency cannot tell a released hole from a
  never-touched one). Such a `Full` still depended on the base for its unchanged
  pages, so it was a whole-history `Diff` under another name, useful only to
  recover from a lost `Diff` response. `Full` now goes through Firecracker's own
  `dump` with a backend attached as without; the bitmap, the `Full`
  classification and the handler's base-copy mode are gone (§7).
- On hugetlbfs, `discard_range` marked every 4 KiB page of the requested range
  dirty while `MADV_REMOVE` freed only the huge pages the range covered
  entirely: the rest was reported dirty but was neither in the memfd nor zero.
  The 2M balloon-after-restore test caught it once the backend stopped keeping
  its own record; `discard_range` now rounds inward to the backing page for
  shared file mappings, as the kernel does.
- Firecracker's `snapshot_layout` reports the layout in a single pass over two
  byte-per-page maps (`SnapshotMemoryLayout::classify`), which turned out to be
  the fastest of the formats measured, not the slowest as one might expect from
  "Firecracker does more work": one bitmap to base64 instead of two dominates.
