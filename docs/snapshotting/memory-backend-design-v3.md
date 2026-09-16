# Memory backend: sharing guest memory, dirty ranges over the API

Status: design. Not yet implemented.

## Goal

Customers running a UFFD page-fault handler need the guest memory shared with
that handler, and they need to be able to produce full and differential memory
snapshots, byte-for-byte identical to Firecracker's, without Firecracker writing
guest memory to disk. The same mechanism must work on first boot, when there is
no page-fault handler, and must be usable as the source side of a live migration
(dirty ranges while running, a consistent final pass while paused).

Rather than introducing a second external actor next to the UFFD handler, this
design generalises the UFFD handler into a *memory backend*: one peer, one
socket, one handshake. A UFFD handler is a memory backend that also receives a
uffd. On first boot the same program is started and receives only the memfd.

Firecracker's part is deliberately small: allocate guest memory as a single
memfd, hand it over, and tell the orchestrator over the HTTP API which byte
ranges of it constitute a snapshot. Copying bytes is the peer's job.

## Current behaviour this design relies on

- Guest memory is allocated by `memory::anonymous` (MAP_PRIVATE) unless a
  vhost-user block device is configured, in which case `memory::memfd_backed`
  creates a single sealed memfd (`F_SEAL_SHRINK|GROW|SEAL`) and maps every
  region MAP_SHARED at `offset = running sum of the preceding region sizes`
  (`src/vmm/src/vstate/memory.rs`; `src/vmm/src/resources.rs`,
  `VmResources::allocate_memory_regions`).
- The snapshot memory file has exactly that layout: `GuestMemoryMmap::dump`
  and `dump_dirty` walk regions in collection order, write plugged slots and
  `seek` over unplugged (virtio-mem) slots; the file size is the sum of all
  region sizes (`KvmVm::snapshot_memory_to_file`, `src/vmm/src/vstate/vm.rs`).
  With a single memfd, the memfd content *is* the full snapshot.
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
  bitmaps. Last, `mark_virtio_queue_memory_dirty` marks every activated
  device's queue pages dirty so that they are part of the *next* diff, because
  queue accesses at runtime do not go through the bitmap.
- **`Paused` freezes the whole VMM, not just the vCPUs.** `Vmm::pause_vm` only
  parks the vCPU threads, but in the `firecracker` binary
  `ApiServerAdapter::handle_request`
  (`src/firecracker/src/api_server_adapter.rs`) leaves the `EventManager` loop
  after a successful `Pause` and blocks on the API channel until `Resume`. No
  device fd, virtqueue eventfd, tap, rate limiter or timer is polled; the only
  code that runs on the VMM thread is API request handling. Consequently, once
  `snapshot/create` has returned, nothing in Firecracker reads or writes guest
  memory or virtqueues for the rest of the pause, and vmstate cannot diverge
  from memory. The remaining writers are the kernel completing io_uring
  requests issued by virtio-block before the pause (drained by `prepare_save`
  inside `snapshot/create`) and external vhost-user backends (an untracked,
  pre-existing gap). See `docs/snapshotting/snapshot-support.md`, "What
  `Paused` means".
- The UFFD protocol (`src/vmm/src/persist.rs`, `guest_memory_from_uffd`,
  `send_uffd_handshake`): Firecracker `connect()`s to a UDS, sends a JSON
  `Vec<GuestRegionUffdMapping>` plus the uffd via `SCM_RIGHTS`, then `forget`s
  the stream. The connection stays open for the VM's life (the handler uses it
  to notice Firecracker exiting) but Firecracker never reads it.
- `PUT /snapshot/create` requires `Paused` and today always answers 204. Other
  endpoints already return `200` with a JSON body through `VmmData` variants
  (`GET /vm/config`, `GET /balloon/statistics`).

What is missing for a memory backend: a single memfd covering all regions, a
way to hand it over (at boot as well as at restore), and an API that reports
which ranges of the memfd a snapshot consists of.

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
- `memory::create` gains a base offset so a region can be mapped from the
  shared memfd at a non-zero offset.
- Invariant, asserted in unit tests: for every region, its memfd offset equals
  the offset `dump` writes it at. Regions are sorted by guest address and the
  hotplug region sits past the 64-bit MMIO hole, so it is always last.
- On restore all regions are known from `GuestMemoryState`, so
  `memfd_backed(mem_state.regions(), ..)` already yields one memfd.
- Out of scope: `GuestRegionMmapExt::discard_range` (balloon, virtio-mem
  unplug) keeps using `madvise(MADV_DONTNEED)`, which does not release pages of
  a shared mapping. Inherited from the vhost-user memfd path; separate
  workstream. It does not affect snapshot identity: unplugged slots are skipped
  by both Firecracker and the backend based on `plugged` state, not on page
  contents.
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

| Situation                              | fds                    |
| :------------------------------------- | :--------------------- |
| restore, `backend_type: Uffd` (today)  | `[uffd]`               |
| restore, `backend_type: MemoryBackend` | `[uffd, memfd]`        |
| boot, `machine-config.mem_backend`     | `[memfd]`              |

- The message already carries everything a peer needs. `offset` is the
  region's offset in the memfd, which by §1 is also its offset in a snapshot
  file; `page_size` is the backing page size (4 KiB or the hugetlbfs size) as
  today. The plug state of virtio-mem slots is only relevant when copying and
  is returned with every dirty set (§7, §8); the total size is the sum of
  region sizes; how Firecracker tracks dirty pages (`KVM_GET_DIRTY_LOG` or
  `mincore`) is invisible to a peer that receives byte ranges.
- The uffd, when present, is always first, so a handler written for today's
  protocol that reads a single fd keeps working even if it is handed the longer
  list. The memfd is always last. A peer knows which list to expect from how it
  was started; if it wants to check, `readlink /proc/self/fd/N` distinguishes
  `anon_inode:[userfaultfd]` from `/memfd:...`.
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
- `backend_type: MemoryBackend` does not select a protocol; it means "the
  `Uffd` backend, and also hand over the memfd". Which spelling of that is
  best is an API question (§6).
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

The snapshot is consistent if the memory bytes the peer copies correspond to
the vmstate Firecracker saved. Today that holds because vmstate save and memory
dump are one synchronous call. With a memory backend it holds because of the
pause invariant: `snapshot/create` requires `Paused`, and from the moment it
returns until `PATCH /vm Resumed` no code path in Firecracker writes guest
memory or virtqueues. The bytes in the memfd at the ranges `snapshot/create`
returned are the bytes `dump`/`dump_dirty` would have written at that moment,
and they stay that way for the rest of the pause. The peer can therefore copy
at its own pace, as long as it finishes before the resume. There is no window
to hold open, no acknowledgement to wait for and no blocked VMM thread.

This is a statement about *writes to guest memory after `snapshot/create`
returns*, nothing broader. Firecracker is not idle while paused:
`snapshot/create` itself modifies guest memory (draining block I/O completes
reads into guest buffers) and reads and resets the dirty bitmaps to produce the
ranges; `PUT /snapshot/dirty-ranges` reads and resets the bitmaps again;
configuration requests read and modify device state. None of them modify guest
memory once `snapshot/create` has returned, which is all the argument needs.

The writers, and why each is quiescent after `snapshot/create` returns:

- vCPU threads: parked in their paused loop since `PATCH /vm Paused`; KVM does
  not run the guest, so the KVM dirty log is stable.
- Device emulation: lives on the VMM thread, which is not running the event
  loop while paused. Devices cannot parse descriptors, write frames or update
  used rings.
- Asynchronous block I/O: io_uring requests issued before the pause are
  completed by the kernel regardless of the event loop. `VirtioBlock::
  prepare_save`, run by `save_state` inside `snapshot/create`, drains them and
  processes the completion queue, which is where the pages they wrote are
  marked dirty (`mark_dirty_mem_and_unwrap`). After `snapshot/create` nothing
  is in flight.
- vhost-user backends: external, untracked, unchanged from today.
- The memory backend itself: trusted not to write.

Two ordering rules follow, and the doc for the feature must state them:

1. The dirty set that accompanies a snapshot is the one returned by
   `snapshot/create` (§7). It is computed after `prepare_save`, so it includes
   the pages written by drained block I/O and excludes nothing that vmstate
   already reflects.
1. The standalone dirty-ranges request (§8) is for pre-copy passes. It can be
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
    "backend_type": "MemoryBackend",
    "backend_path": "/path/to/backend.sock"
  }
}
```

Only `MemoryBackend` is valid pre-boot (`File` and `Uffd` describe how to
*populate* memory and have no meaning on a fresh boot). Presence means:
memfd-backed memory (§1) and a memory backend handshake carrying the memfd,
performed in `build_microvm_for_boot` after all regions are mapped and
registered with KVM and before vCPUs start. Machine-config is the home because
this is a memory-allocation property like `track_dirty_pages` and `huge_pages`.
It is accepted from the JSON config file as well. Not persisted in the
snapshot.

### 6. Restore API

`snapshot/load.mem_backend.backend_type` gains a third value:

```json
{
  "snapshot_path": "...",
  "mem_backend": {
    "backend_type": "MemoryBackend",
    "backend_path": "/path/to/backend.sock"
  }
}
```

- `File` and `Uffd`: today's behaviour, byte for byte.
- `MemoryBackend`: memfd-backed memory; a uffd is created and registered on
  the shmem mapping (`UFFDIO_COPY`/`UFFDIO_ZEROPAGE` are supported on
  shmem-backed VMAs in MISSING mode on Linux 5.10,
  `UFFD_FEATURE_MISSING_SHMEM`, and on hugetlbfs memfds); the UFFD handshake
  is sent with `[uffd, memfd]`. Fault serving on the backend side is the same
  code a UFFD handler runs today.

Nothing about the previous instance's backend is read from the snapshot; the
`backend_type` alone decides.

Since the handshake is the UFFD handshake, `MemoryBackend` is `Uffd` plus one
fd, and a boolean on the existing backend would express that just as well:

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
sensible meaning for `File` (must be rejected) and no counterpart at boot,
where there is no population and `machine-config.mem_backend` would carry a
`backend_type` that means nothing. The third enum value reads naturally in both
places. This document uses `MemoryBackend`; the spelling is an API-review
question and changes nothing else.

### 7. `PUT /snapshot/create` with a memory backend

Request:

```json
{
  "snapshot_type": "Full" | "Diff",
  "snapshot_path": "/path/vmstate"
}
```

`mem_file_path` becomes optional in `CreateSnapshotParams`. With a memory
backend connected it must be absent (400 otherwise: Firecracker does not write
guest memory in this mode); without one it must be present, as today. The
request carries no `mem_backend` field: whether a backend is connected is a
property of the instance, fixed at boot or restore, and repeating it here would
only add a way to get a 400.

Response, with a backend: `200 OK` with

```json
{
  "snapshot_type": "Diff",
  "memory": {
    "total_size": 1073741824,
    "ranges": [
      { "offset": 0, "len": 8192 },
      { "offset": 1048576, "len": 4096 }
    ],
    "unplugged": [
      { "offset": 805306368, "len": 268435456 }
    ]
  }
}
```

- `total_size` is the size of a full memory file (sum of all region sizes,
  including the hotplug region); the peer creates the target at that size.
- `ranges` are the bytes to copy from the memfd into the target at the same
  offset.
- `unplugged` are the bytes the peer must zero in the target: the current
  unplugged virtio-mem slots. A fresh `dump`/`dump_dirty` output has zeros
  there; a slot unplugged since the last diff would otherwise keep stale bytes
  in a merged file. (Firecracker's own in-place merge into an existing memory
  file does leave stale bytes there; restore never maps them, so both are
  valid, and zeroing is what makes the peer's file byte-identical to a fresh
  one.)

All offsets are memfd offsets, which by §1 are file offsets. No per-region
information is needed by the peer, so none is returned.

Without a backend the response stays `204 No Content`. A new
`VmmData::SnapshotMemoryLayout` variant carries the body through the existing
API plumbing.

Procedure, on the VMM thread, inside the paused API loop:

1. require `Paused`, as today;
1. `save_state` and write vmstate to `snapshot_path`, as today (this runs
   `prepare_save`: block drain, net RX buffer reset);
1. compute `ranges`: `Diff` → take the KVM dirty log (or `mincore`), union with
   Firecracker's `AtomicBitmap`, restrict to plugged slots, reset both bitmaps;
   `Full` → all plugged slots, and reset both bitmaps, mirroring what
   `snapshot_memory_to_file` does for `Full` today;
1. `mark_virtio_queue_memory_dirty`, as today, so queue pages are in the next
   diff;
1. return the body.

`ranges` and `unplugged` are sorted, merged, page-aligned `{offset, len}`
pairs, disjoint from each other. `ranges` covers exactly the pages
`dump_dirty` (or `dump`) would write.

Failure handling:

- If step 2 fails, nothing has been consumed and the error is returned as
  today.
- If step 3 fails while taking the KVM log, the bitmaps are folded back with
  `store_dirty_bitmap`, as `dump_dirty` does today, and an error is returned.
- Once the body is handed to the API thread, the dirty set is consumed. If the
  HTTP client loses the response (disconnect mid-write), Firecracker cannot
  know, and the ranges are gone; the orchestrator must treat a lost `Diff`
  response as "take a `Full` next". This is the same contract as losing a diff
  file today.
- The backend is never consulted, so a dead backend does not fail the request;
  the orchestrator finds out when it tries to use the memfd.

Range encoding was chosen over raw bitmaps because it maps directly onto
`copy_file_range`/`pwrite` and is language-neutral. Worst case (every other
page dirty) a range list is roughly 3 MiB of JSON per GiB of guest memory; a
`"format": "bitmap"` request field returning base64 bitmaps per region (32 KiB
per GiB) can be added later without breaking clients.

### 8. `PUT /snapshot/dirty-ranges`: pre-copy passes

```json
PUT /snapshot/dirty-ranges
{}
```

Response: `200 OK` with the same `memory` object as §7, or an error.

Semantics:

- Callable in `Running` or `Paused`. Consuming: the same computation as step 3
  of §7 for `Diff`, followed by `mark_virtio_queue_memory_dirty`.
- Before resetting, Firecracker calls a new
  `VirtioDevice::prepare_dirty_tracking_reset` hook on every activated device.
  virtio-net marks RX buffers dirty when it *parses* them
  (`IoVecBufferMut::load_descriptor_chain`), not when a frame is later written
  into them; a buffer parsed before a reset and filled after it would otherwise
  never show up in a later set. The hook does for virtio-net what
  `prepare_save` does (return parsed, unfilled descriptors to the queue)
  without the snapshot-only side effects of `prepare_save` on other devices
  (vsock connection reset, block drain and `fsync`). Any future device that
  marks memory dirty ahead of writing must implement it.
- Gives no atomicity with respect to concurrent guest or device writes: a page
  can be modified after the set was computed and before the peer copies it. It
  is then dirty again and will be in the next set. This is the intended use:
  iterative pre-copy, with `PATCH /vm Paused` + `snapshot/create` as the final
  pass.
- **Rejected with 400 when no memory backend is connected.** See the decision
  below.

`PUT` rather than `GET` because the request has a side effect (it consumes the
dirty set); the empty body keeps room for an additive `format` field.

#### Decision: dirty ranges only with a memory backend

The endpoint consumes the dirty bitmaps. Without a memory backend, Firecracker
is the only party that can turn those bitmaps into bytes (nobody else can read
anonymous MAP_PRIVATE guest memory), so a consuming call would silently punch
holes in the next Firecracker-written `Diff` snapshot with no way for
Firecracker to compensate. There is also no consumer for the result: the ranges
describe offsets into a memfd that does not exist in that configuration.

Options considered:

- Allow it anyway and document the hazard: rejected, it is a foot-gun with no
  use case.
- Offer a non-consuming "peek" variant (take the KVM log, union with
  Firecracker's bitmap, fold the union back into Firecracker's bitmap so
  nothing is lost, return the ranges): technically simple and safe, but still
  has no consumer without a backend. It can be added later as
  `{"consume": false}` if a monitoring use case appears (for example,
  estimating the size of the next diff before pausing).
- Tie the endpoint to the backend: chosen. With a backend connected Firecracker
  never writes guest memory itself, so the bitmaps have exactly one consumer
  and ownership is unambiguous.

### 9. Backend implementation (reference)

There is no separate reference backend. The existing example handlers in
`src/firecracker/examples/uffd/` already parse the handshake and already run a
poll loop over the uffd and the UDS (`uffd_utils.rs`); they are extended to be
memory backends:

- `uffd_utils.rs` receives all fds from the handshake instead of one. The uffd
  path is built if a uffd was received; the memfd is kept if one was received.
  A handler started for a boot receives only the memfd and runs the poll loop
  without a fault source.
- A control socket of the handler's own, unrelated to Firecracker, on which the
  orchestrator (the test framework) sends
  `{"Copy": {"mem_path": "...", "memory": <the object returned by
  Firecracker>}}`. The handler copies each range from the memfd into `mem_path`
  with `copy_file_range` (creating the file at `total_size`, merging into an
  existing file for diffs, zeroing `unplugged`) and replies
  `{"Done": {"success": true|false, "message": "..."}}`. This is one module
  shared by all example handlers.

The split reflects the real deployment: the orchestrator talks HTTP to
Firecracker and whatever it likes to its backend; Firecracker talks to the
backend once, at the handshake. The existing handlers keep working unchanged
when started as plain UFFD handlers.

### 10. Byte-for-byte identity: how it is demonstrated

With a memory backend connected Firecracker never writes a memory file, so a
Firecracker-made reference cannot be produced from the same VM in an
integration test. Identity is therefore established in two layers.

Rust unit tests (`src/vmm/src/vstate/memory.rs`, `persist.rs`), where both
implementations are driven from identical inputs:

- Given the same `(kvm_bitmap, firecracker_bitmap, plugged)` input, the range
  list produced for a `Diff` covers exactly the pages `dump_dirty` writes
  (property-style test over random bitmaps, including a trailing slot whose
  page count is not a multiple of 64 and unplugged slots); the range list for
  a `Full` covers exactly the bytes `dump` writes.
- Applying those ranges from a memfd into a file yields a file identical to
  `dump_dirty`'s / `dump`'s output.
- The memfd offset of every region equals the offset `dump` writes it at, with
  and without a hotplug region.

Integration tests (`tests/integration_tests/functional/test_memory_backend.py`,
framework support in `tests/framework/utils_memory_backend.py`,
`Microvm.mem_backend`, `SnapshotType.FULL_BACKEND/DIFF_BACKEND`, where the
framework plays the orchestrator: calls `snapshot/create`, forwards the body to
the backend's control socket). Definitions: `M_full(t)` is the backend's copy
of all plugged ranges; `M_diff(t0,t1)` is the backend's sparse file of the
ranges returned by a `Diff` `snapshot/create` at `t1`; `rebase` is the existing
`rebase-snap` tool / `Snapshot.rebase_snapshot`.

1. Full snapshot layout: boot with a backend, run a workload, pause,
   `snapshot/create Full` → `M_full(t0)`. Assert size `total_size`, unplugged
   slots all zero, and that Firecracker restores from it (`File` backend) with
   a healthy guest.
1. Diff self-consistency (the backend-side analogue of
   `test_snapshot_basic.py::test_cmp_full_and_first_diff_mem`): resume, run a
   workload, pause, `snapshot/create Diff` → `M_diff(t0,t1)`; resume the same
   VM, pause again without running anything, `snapshot/create Full` →
   `M_full(t1)`. Assert `rebase(M_full(t0), M_diff) == M_full(t1)`. Any dirty
   page missed by the range computation shows up as a mismatch.
1. Chains: several diffs rebased onto the base equal a final full copy, and the
   rebased result restores with a healthy guest.
1. Pre-copy: while running a workload, issue several `dirty-ranges` requests
   and copy each set; pause; `snapshot/create Diff`; copy. The result must
   equal a `Full` taken right after, as in test 2. Exercises the
   `prepare_dirty_tracking_reset` hook under RX traffic.
1. Pause invariant: pause a VM with a backend, inject tap traffic for a few
   seconds, assert the guest-visible RX used index (read from the memfd) and
   the device's `rx_bytes_count` metric do not change until resume.
1. Cross-check against a Firecracker-made snapshot: restore one VM with a
   backend and one without from the same base snapshot, pause both
   immediately, take a backend `Full` and a classic `Full`, compare. This is
   meaningful only because nothing ran in either VM; it checks layout, hole
   handling and `total_size` end to end rather than dirty tracking.
1. Variants of 1–4: virtio-mem with unplugged slots, balloon inflated,
   hugetlbfs 2M, `track_dirty_pages=false` (`mincore` path), x86 with >4 GiB
   (two DRAM regions), aarch64.

Compatibility tests:

- The UFFD protocol is unaffected: existing `test_uffd.py` runs unchanged with
  the existing handlers.
- Memory backend at boot (no uffd) and at restore (uffd) with the same backend
  binary; a backend-produced snapshot restores with `Uffd`, `File` and
  `MemoryBackend`.
- Model change across restore: boot with backend → snapshot → restore with
  `Uffd` and with `File`; restore with `Uffd` → snapshot → restore with
  `MemoryBackend`; in each case the resulting VM snapshots correctly in its
  new mode.
- Negative: `snapshot/create` with `mem_file_path` and a backend → 400;
  without `mem_file_path` and no backend → 400; `dirty-ranges` without a
  backend → 400; `machine-config.mem_backend` with `File`/`Uffd` → 400;
  backend process killed → VM keeps running, `snapshot/create` still returns
  ranges, the orchestrator's copy fails.

### 11. Security and operational notes

- The backend receives a read/write fd to all guest memory; it must run in the
  same jail/user as a UFFD handler does today. Socket paths are relative to the
  jailer chroot.
- Seccomp: the VMM-thread filter already allows `connect` and `sendmsg` for
  the UFFD handshake; nothing new is needed since Firecracker never reads the
  backend socket. `copy_file_range` is only used by the backend.
- The memfd is sealed against resize; Firecracker keeps its own fd, so backend
  death never invalidates guest memory.
- API response size: `ranges` can be large for fragmented dirty sets (§7). The
  API server's payload limit applies to requests only; responses are streamed
  as today for `GET /vm/config`.
- Metrics: `mem_backend.{handshake_fails, dirty_ranges_requests,
  dirty_ranges_fails}` and the existing snapshot latency metrics.

### 12. Work breakdown (PR sequence)

1. Single-memfd allocation for DRAM + hotplug; `memory::create` base offset;
   offset-invariant unit tests. No API change, mergeable alone.
1. Dirty-range computation factored out of `GuestMemorySlot::dump_dirty` and
   `dump` so file writing and range production share one iterator; Rust
   identity tests. No API change.
1. `send_uffd_handshake` with a list of fds, `MachineConfig.mem_backend`,
   boot-time handshake, metrics.
1. `PUT /snapshot/create` with a backend: optional `mem_file_path`,
   rejections, `VmmData::SnapshotMemoryLayout`, 200 body, swagger.
1. `PUT /snapshot/dirty-ranges`, `prepare_dirty_tracking_reset` hook for
   virtio-net, swagger.
1. `snapshot/load` with `backend_type: MemoryBackend`: memfd + uffd on shmem,
   handshake with two fds.
1. Example handlers extended to receive the memfd and copy ranges, Python
   framework, integration and compatibility tests, including the pause
   invariant test.
1. Docs (`docs/snapshotting/memory-backend.md`, updates to
   `handling-page-faults-on-snapshot-resume.md` and `snapshot-support.md`),
   CHANGELOG.

### 13. Decisions taken

- The backend is trusted; connection failure at boot/restore is fatal.
  Firecracker never reads the backend socket after the handshake and does not
  monitor it.
- One handshake, the existing UFFD one, unchanged; a memory backend differs
  from a UFFD handler only by the fds it receives (uffd first, memfd last). No
  version field; extensions are additive fields and appended fds. Nothing
  about the choice is persisted in the snapshot.
- No synchronisation between Firecracker and the backend at snapshot time. The
  pause invariant of the `firecracker` binary is what makes this correct, and
  it is documented and tested as such.
- Dirty information is exchanged over the HTTP API as page-aligned
  `{offset, len}` ranges, in the `snapshot/create` response (final, consistent
  set) and from `PUT /snapshot/dirty-ranges` (pre-copy, consuming, any state).
- `PUT /snapshot/create` with a backend never writes memory and takes no
  `mem_file_path`; the response code changes from 204 to 200 in that mode
  only.
- `PUT /snapshot/dirty-ranges` is rejected without a memory backend. A
  non-consuming variant is possible later if a use case appears.
- A lost `Diff` response is the orchestrator's problem; it falls back to
  `Full`.
- Restore with a backend always uses a uffd for population; file population
  with sharing is deferred.
- Balloon/virtio-mem discard behaviour on shared mappings is out of scope.
