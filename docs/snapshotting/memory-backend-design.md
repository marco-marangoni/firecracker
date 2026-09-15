# Memory backend: sharing guest memory with the UFFD handler

Status: proof of concept implemented on this branch (Firecracker side, reference
backend in `src/firecracker/examples/memory_backend`, integration tests in
`tests/integration_tests/functional/test_mem_backend.py`). Not yet reviewed.

## Goal

Customers running a UFFD page-fault handler need the guest memory shared with
that handler, and they need to be able to produce full and differential memory
snapshots, byte-for-byte identical to Firecracker's, without Firecracker writing
guest memory to disk. The same mechanism must work on first boot, when there is
no page-fault handler, and must be usable as the source side of a live migration
(dirty ranges while running, consistent final pass while paused).

Rather than introducing a second external actor next to the UFFD handler, this
design generalises the UFFD handler into a *memory backend*: one peer, one
socket, one handshake. A UFFD handler is a memory backend that also receives a
uffd. On first boot the same program is started and receives only the memfd.

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
  single memfd, the memfd content *is* the full snapshot.
- The virtio-mem hotplug region is currently allocated separately
  (`VmResources::allocate_memory_region`, from `build_microvm_for_boot`), which
  in the memfd case yields a second memfd. Only its guest address needs the KVM
  VM to exist; its size is known up front.
- Diff snapshots: `mark_virtio_queue_memory_dirty`, then
  `KvmVm::get_dirty_bitmap` (`KVM_GET_DIRTY_LOG`, which clears KVM's log;
  `mincore` fallback when `track_dirty_pages` is off), then `dump_dirty` writes
  every page dirty in either the KVM bitmap or Firecracker's own `AtomicBitmap`
  (device writes), at host-page (4 KiB) granularity. On success Firecracker's
  bitmap is reset; on failure the KVM bitmap is folded back into it
  (`store_dirty_bitmap`) so no dirty information is lost.
- **Correction (see the third design document).** The bullet below is wrong for
  the `firecracker` binary. `Vmm::pause_vm` only pauses vCPUs, but
  `ApiServerAdapter::handle_request`
  (`src/firecracker/src/api_server_adapter.rs`) stops running the event loop
  after a `Pause` and blocks on the API channel until `Resume`, so device
  emulation is frozen for the whole pause and vmstate cannot diverge from guest
  memory after `snapshot/create`. The synchronisation window in §7 is therefore
  unnecessary, and a request channel serviced from `Vmm::process` (§4) would not
  even be read while paused. Kept as written for history.
- ~~`Vmm::pause_vm` only pauses vCPUs. Device emulation on the VMM thread keeps
  running while the VM is `Paused`. Firecracker's snapshots are consistent only
  because vmstate save and memory dump happen synchronously in one VMM-thread
  call.~~
- The UFFD protocol (`src/vmm/src/persist.rs`, `guest_memory_from_uffd`,
  `send_uffd_handshake`): Firecracker `connect()`s to a UDS, sends a JSON
  `Vec<GuestRegionUffdMapping>` plus the uffd via `SCM_RIGHTS`, then `forget`s
  the stream. The connection therefore already stays open for the VM's life (the
  handler uses it to notice Firecracker exiting), but Firecracker never reads
  it. The example handler runtime
  (`src/firecracker/examples/uffd/uffd_utils.rs`) already polls both the uffd
  and the UDS.

What is missing for a memory backend: the memfd, the `file_offset`/`plugged`
columns in the mapping table, Firecracker reading requests on the socket, and a
way to run the handshake at boot.

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
- Out of scope: `GuestRegionMmapExt::discard_range` (balloon, virtio-mem unplug)
  keeps using `madvise(MADV_DONTNEED)`, which does not release pages of a shared
  mapping. This limitation is inherited from the vhost-user memfd path and is a
  separate workstream. It does not affect snapshot identity: Firecracker and the
  backend read the same memfd, and unplugged slots are skipped by both based on
  `plugged` state, not on page contents.
- Shared mappings have costlier page faults than anonymous memory, and THP on
  shmem depends on the host's `shmem_enabled`. This is the price of sharing, as
  for vhost-user today.

Without a memory backend, memory allocation is exactly as today. vhost-user
backends keep receiving the memfd through the vhost-user protocol; if both are
configured they share the same memfd.

### 2. Two protocols, selected per boot/restore

- UFFD protocol (existing, unchanged, kept indefinitely): a bare JSON array of
  `GuestRegionUffdMapping` plus one uffd, anonymous guest memory, no further
  traffic on the socket. Selected with `backend_type: Uffd`. No existing handler
  has to change.
- Memory backend protocol (new): a JSON object plus one or more fds, on a
  connection that stays open and carries requests (§4). Selected with
  `backend_type: MemoryBackend`, at boot or at restore.

These are different protocols, not versions of one another, and a handler
implements one of them. The orchestrator knows which handler it started and
picks the matching `backend_type`; there is no in-band version field or format
sniffing. Future additions to the memory backend protocol are additive JSON
fields and additional `fds` entries.

The choice is **not** persisted in the snapshot (`VmInfo` is untouched). A VM
booted with a memory backend can be snapshotted and restored with the UFFD
protocol or from a file; a VM restored with the UFFD protocol can be snapshotted
and restored with a memory backend. Consequences of the choice are confined to
the running instance: memory backing, handshake format, and how
`snapshot/create` writes memory.

### 3. Handshake (memory backend protocol)

Firecracker connects to the backend's UDS and sends one message and the fds in a
single `sendmsg`:

```json
{
  "fds": ["memfd", "uffd"],
  "page_size": 4096,
  "track_dirty_pages": true,
  "total_size": 1073741824,
  "regions": [
    {
      "guest_addr": 0,
      "size": 1073741824,
      "file_offset": 0,
      "host_virt_addr": 140000000000000,
      "region_type": "Dram",
      "slot_size": 1073741824,
      "plugged": [true]
    }
  ]
}
```

- `fds` names the `SCM_RIGHTS` fds in order. Boot sends `["memfd"]`; restore
  sends `["memfd", "uffd"]`. New fds can be appended later without breaking
  parsers.
- `regions` is `GuestMemoryState` plus `file_offset` and `host_virt_addr`, a
  superset of `GuestRegionUffdMapping`. `page_size` is the host page size used
  for dirty tracking (4 KiB even with hugetlbfs, matching `dump_dirty`).
- The backend's role is determined by `fds`: if `uffd` is present it registers a
  fault-serving path; it always keeps the memfd and always serves requests.
  Firecracker does not create a uffd at boot: memory starts zeroed and a uffd
  would only turn every first touch into a round trip.
- Trust and lifecycle: the backend is trusted (same as a UFFD handler today);
  connection failure fails boot/restore. If the connection later drops,
  Firecracker logs, counts a metric and keeps running; `snapshot/create` then
  fails until the VM is restarted (no reconnect for now). The backend obtains
  Firecracker's PID via `SO_PEERCRED`, as today.

### 4. Requests over the connection

Newline-delimited JSON, serviced on the VMM thread: the socket fd is added to
`Vmm`'s `EventOps` in `Vmm::init` and handled in `Vmm::process`
(`src/vmm/src/lib.rs`), which gives the handler `&mut Vmm`. Requests are handled
synchronously, one at a time. There is a single request:

- `{"GetDirtyRanges": {}}` → callable in any VM state, consuming. Firecracker
  runs `mark_virtio_queue_memory_dirty`, takes the KVM dirty log (or `mincore`
  when tracking is off), unions it with its own `AtomicBitmap`, restricts to
  plugged slots, resets both bitmaps and replies
  `{"DirtyRanges": {"ranges": [{"offset": 0, "len": 8192}, ...]}}` with sorted,
  merged, page-aligned ranges in memfd/file offset space. If the reply cannot be
  sent, the bitmap is folded back with `store_dirty_bitmap` so the next call
  returns a superset. Outside a snapshot window this gives no atomicity with
  respect to device writes; it is meant for pre-copy passes.

Replies are either the response above or an error:
`{"Error": {"kind": "UnknownRequest" | "Internal", "message": "..."}}`.
`UnknownRequest` covers malformed JSON and unrecognised request names;
`Internal` covers failures while serving a valid request (for example
`KVM_GET_DIRTY_LOG` failing), in which case no bitmap was reset.

Range encoding was chosen over raw bitmaps because it maps directly onto
`copy_file_range`/`pwrite` in the backend and is language-neutral. A bitmap
encoding can be added later as an additive field if fragmented dirty sets make
it necessary.

No layout query is needed: the regions are delivered in the handshake and again,
with current `plugged` state, in every `SnapshotRequest` (§7). A slot unplugged
between pre-copy passes is therefore zeroed by the backend at the final pass,
matching what `dump` writes for unplugged slots.

Resetting dirty tracking while devices keep running has one subtlety that
`GetDirtyRanges` has to handle and that snapshots never had to: virtio-net marks
the guest's RX buffers dirty when it *parses* them, not when a frame is later
written into them (`IoVecBufferMut::load_descriptor_chain`). A buffer parsed
before a reset and filled after it would never show up in a later dirty set.
Snapshots are safe because `Net::prepare_save` drops the parsed, unfilled
buffers so they are parsed (and marked) again afterwards. `GetDirtyRanges`
therefore calls a new `VirtioDevice::prepare_dirty_tracking_reset` hook before
resetting, which for virtio-net does the same, without the snapshot-only side
effects of `prepare_save` (vsock connection reset, block drain). Any future
device that marks memory dirty ahead of writing must implement that hook.

Dirty-bitmap ownership: with a memory backend connected, Firecracker only reads
or resets the bitmaps when handing ranges to the backend.

### 5. Boot API

`PUT /machine-config` gains a `mem_backend` field using the existing
`MemBackendConfig` shape:

```json
{ "mem_backend": { "backend_type": "MemoryBackend", "backend_path": "/path/to/backend.sock" } }
```

Only `MemoryBackend` is valid pre-boot (`File` and `Uffd` describe how to
*populate* memory and have no meaning on a fresh boot). Presence means:
memfd-backed memory (§1) and a memory backend handshake with `["memfd"]`
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
    "backend_type": "MemoryBackend",
    "backend_path": "/path/to/backend.sock"
  }
}
```

- `File` and `Uffd`: today's behaviour, byte for byte.
- `MemoryBackend`: memfd-backed memory; a uffd is created and registered on the
  shmem mapping (`UFFDIO_COPY`/`UFFDIO_ZEROPAGE` are supported on shmem-backed
  VMAs in MISSING mode on Linux 5.10, `UFFD_FEATURE_MISSING_SHMEM`, and on
  hugetlbfs memfds); handshake with `["memfd", "uffd"]`; the connection is kept
  for requests. Fault serving on the backend side is the same code a UFFD
  handler runs today, only the handshake parser differs.
- Populating from a file while sharing memory is not offered for now, so a
  memory backend at restore always receives a uffd. A MAP_PRIVATE mapping of the
  snapshot file cannot be handed out as an fd, and shmem cannot lazily reflect
  another file. If the need arises the options are, in order of preference: an
  eager copy of the snapshot file into the memfd (`copy_file_range` file→shmem,
  cross-filesystem on 5.10, with a read/write fallback) at the cost of reading
  and allocating all guest memory at restore; an in-Firecracker UFFD
  fault-handler thread copying pages on demand; or a MAP_SHARED mapping of a
  reflinked copy of the snapshot file, which is lazy but makes every guest write
  dirty page cache subject to writeback. None of them changes the handshake, so
  the decision can be deferred.

Nothing about the previous instance's backend is read from the snapshot; the
`backend_type` alone decides.

### 7. `PUT /snapshot/create` with a memory backend

```json
{
  "snapshot_type": "Full" | "Diff",
  "snapshot_path": "/path/vmstate",
  "mem_backend": { "backend_type": "MemoryBackend" }
}
```

With a memory backend connected, `mem_file_path` is rejected and
`mem_backend.backend_type` must be `MemoryBackend`; without one, `MemoryBackend`
is rejected and behaviour is unchanged.

Synchronisation is required because `Paused` only stops vCPUs (`Vmm::pause_vm`);
device emulation on the VMM thread keeps running and can write guest memory and
used rings (a tap RX frame, an in-flight block completion) after the vmstate
save. Firecracker's own snapshots avoid this only because save and dump are one
synchronous call. The backend path recreates that property. On the VMM thread:

1. require `Paused`, as today;
1. save vmstate (`save_state` runs every device's `prepare_save`; virtio-block
   drains in-flight io_uring requests, so no kernel-side DMA into guest memory
   is pending afterwards);
1. compute the ranges: `Diff` → the `GetDirtyRanges` procedure (consuming);
   `Full` → all plugged slots;
1. send
   `{"SnapshotRequest": {"snapshot_type": "Diff", "regions": [...], "ranges": [...]}}`
   (regions carry the current `plugged` state);
1. block reading exactly one
   `{"SnapshotDone": {"success": true|false, "message": "..."}}`; any other
   message is a protocol error;
1. return 204, or an error carrying the backend's message. On failure, protocol
   error or disconnect, fold the ranges of step 3 back into Firecracker's bitmap
   so the next diff is a superset.

During step 5 nothing can write guest memory: vCPUs are parked in their paused
loop, device emulation lives on the blocked VMM thread, async block I/O was
drained in step 2, and the backend is trusted. Pages that devices dirtied
between `PATCH /vm Paused` and `snapshot/create` are covered: they are in the
bitmaps and vmstate was captured after them, exactly as today. The order
`save_state` → mark queue pages → collect bitmaps is the same as in today's
`create_snapshot`. vhost-user backends are an external writer whose pages were
never tracked; that pre-existing gap is unchanged.

There is no timeout: the backend is trusted, and a hung backend blocks the
snapshot the same way a hung filesystem write would today. A timeout can be
added later without a protocol change.

### 8. Backend implementation (reference)

A handler implements one protocol, so the reference memory backend is a separate
example (`src/firecracker/examples/memory_backend/`) rather than a mode of the
UFFD handlers. It shares the fault-serving and runtime code with
`examples/uffd/uffd_utils.rs` (factor the `UffdHandler` page-serving logic and
the `Runtime` poll loop into a common module) and differs in:

- the handshake parser: JSON object with `fds`, keeps the memfd, builds the uffd
  path only if a uffd was received;
- message handling on the UDS in the same poll loop: issuing `GetDirtyRanges`
  for pre-copy passes and answering `SnapshotRequest` by copying ranges from the
  memfd into the target file with `copy_file_range` (creating at `total_size`
  for `Full`, merging for `Diff`, zeroing unplugged slots) and replying
  `SnapshotDone`;
- running without a uffd at boot.

The existing UFFD example handlers are untouched.

### 9. Byte-for-byte identity: how it is demonstrated

With a memory backend connected Firecracker never writes a memory file, so a
Firecracker-made reference cannot be produced from the same VM in an integration
test. Identity is therefore established in two layers.

Rust unit tests (`src/vmm/src/vstate/memory.rs`, `persist.rs`), where both
implementations are driven from identical inputs:

- Given the same `(kvm_bitmap, firecracker_bitmap, plugged)` input, the range
  list produced for `GetDirtyRanges` covers exactly the pages `dump_dirty`
  writes (property-style test over random bitmaps, including a trailing slot
  whose page count is not a multiple of 64 and unplugged slots).
- Applying those ranges from a memfd into a file yields a file identical to
  `dump_dirty`'s output; applying the plugged-slot ranges of a `Full`
  `SnapshotRequest` yields `dump`'s output.
- The memfd offset of every region equals the offset `dump` writes it at, with
  and without a hotplug region.

Integration tests (`tests/integration_tests/functional/test_memory_backend.py`,
framework support in `tests/framework/utils_memory_backend.py`,
`Microvm.mem_backend`, `SnapshotType.FULL_BACKEND/DIFF_BACKEND`). Definitions:
`M_full(t)` is the backend's copy of all plugged ranges; `M_diff(t0,t1)` is the
backend's sparse file of the ranges received in a `Diff` `SnapshotRequest` at
`t1`; `rebase` is the existing `rebase-snap` tool / `Snapshot.rebase_snapshot`.

1. Full snapshot layout: boot with a backend, run a workload, pause,
   `snapshot/create Full` → `M_full(t0)`. Assert size `total_size`, unplugged
   slots all zero, and that Firecracker restores from it (`File` backend) with a
   healthy guest.
1. Diff self-consistency (the backend-side analogue of
   `test_snapshot_basic.py::test_cmp_full_and_first_diff_mem`): resume, run a
   workload, pause, `snapshot/create Diff` → `M_diff(t0,t1)`; resume the same
   VM, pause again without running anything, `snapshot/create Full` →
   `M_full(t1)`. Assert `rebase(M_full(t0), M_diff) == M_full(t1)`. Any dirty
   page missed by the range computation shows up as a mismatch.
1. Chains: several diffs rebased onto the base equal a final full copy, and the
   rebased result restores with a healthy guest.
1. Cross-check against a Firecracker-made snapshot: restore one VM with a
   backend and one without from the same base snapshot, pause both immediately,
   take a backend `Full` and a classic `Full`, compare. This is meaningful only
   because nothing ran in either VM; it checks layout, hole handling and
   `total_size` end to end rather than dirty tracking.
1. Variants of 1–3: virtio-mem with unplugged slots, balloon inflated, hugetlbfs
   2M, `track_dirty_pages=false` (`mincore` path), x86 with >4 GiB (two DRAM
   regions), aarch64.

Compatibility tests:

- The UFFD protocol is unaffected: existing `test_uffd.py` runs unchanged with
  the existing handlers.
- Memory backend at boot (no uffd) and at restore (uffd) with the same backend
  binary; a backend-produced snapshot restores with `Uffd`, `File` and
  `MemoryBackend`.
- Model change across restore: boot with backend → snapshot → restore with
  `Uffd` and with `File`; restore with `Uffd` → snapshot → restore with
  `MemoryBackend`; in each case the resulting VM snapshots correctly in its new
  mode.
- Negative: `snapshot/create` with `MemoryBackend` but no backend → 400;
  `mem_file_path` with a backend → 400; `machine-config.mem_backend` with
  `File`/`Uffd` → 400; backend disconnect → VM keeps running, `snapshot/create`
  errors; `SnapshotDone{success:false}` → error surfaced and the next diff
  includes the failed ranges.

### 10. Security and operational notes

- The backend receives a read/write fd to all guest memory; it must run in the
  same jail/user as a UFFD handler does today. Socket paths are relative to the
  jailer chroot.
- Seccomp: audit the VMM-thread filter for `connect`, `sendmsg`, `recvmsg`;
  `copy_file_range` is only used by the backend.
- The memfd is sealed against resize; Firecracker keeps its own fd, so backend
  death never invalidates guest memory.
- Metrics:
  `mem_backend.{handshake_fails, requests, request_fails, disconnects, snapshot_request_duration_us}`.

### 11. Work breakdown (PR sequence)

1. Single-memfd allocation for DRAM + hotplug; `memory::create` base offset;
   offset-invariant unit tests. No API change, mergeable alone.
1. Memory backend handshake serialisation, `MachineConfig.mem_backend`,
   boot-time handshake, socket registered in `Vmm::init`, seccomp, metrics.
1. `GetDirtyRanges`, with the dirty-page iteration factored out of
   `GuestMemorySlot::dump_dirty` so both paths share one iterator; Rust identity
   tests.
1. `PUT /snapshot/create` with `MemoryBackend`: window, rejections, fold-back.
1. `snapshot/load` with `backend_type: MemoryBackend`: memfd + uffd on shmem,
   handshake with two fds.
1. Reference memory backend example (shared code with `examples/uffd`), Python
   framework, integration and compatibility tests.
1. Docs (`docs/snapshotting/memory-backend.md`, updates to
   `handling-page-faults-on-snapshot-resume.md` and `snapshot-support.md`),
   swagger, CHANGELOG.

### 12. Decisions taken

- The backend is trusted; connection failure at boot/restore is fatal, later
  disconnects are logged and Firecracker continues.
- Two protocols, both kept indefinitely, selected by `backend_type`; nothing
  about the choice is persisted in the snapshot.
- Dirty information is exchanged as page-aligned `{offset, len}` ranges.
- `PUT /snapshot/create` with a backend never writes memory; it pushes the
  ranges and blocks the VMM thread until `SnapshotDone`. No timeout for now.
- `GetDirtyRanges` is callable in any VM state and always resets; a
  non-resetting variant can be added later if needed.
- Restore with a backend always uses a uffd for population; file population with
  sharing is deferred.
- Balloon/virtio-mem discard behaviour on shared mappings is out of scope.
