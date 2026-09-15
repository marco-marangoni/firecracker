# Guest Memory Manager: design and implementation plan

Status: draft / implementation plan. Not yet implemented.

## Goal

Allow an external, trusted "guest memory manager" process to own a shared view
of the microVM's guest memory and to produce full and differential memory
snapshots that are byte-for-byte identical to the ones Firecracker produces
today via `PUT /snapshot/create`.

The manager is an additional mechanism, orthogonal to the existing restore
backends. It never populates guest memory: population on restore is still done
by Firecracker (`File` backend) or by a UFFD page-fault handler (`Uffd`
backend). The UFFD protocol is unchanged.

## Current behaviour this design relies on

- Guest memory is allocated by `memory::anonymous` (MAP_PRIVATE) unless a
  vhost-user block device is configured, in which case `memory::memfd_backed`
  creates a single sealed memfd (`F_SEAL_SHRINK|GROW|SEAL`) and maps every
  region MAP_SHARED at `offset = running sum of the preceding region sizes`
  (`src/vmm/src/vstate/memory.rs`, `memory::create`, `memory::memfd_backed`;
  `src/vmm/src/resources.rs`, `VmResources::allocate_memory_regions`).
- The snapshot memory file has exactly the same layout: `GuestMemoryMmap::dump`
  and `dump_dirty` walk regions in collection order, write plugged slots and
  `seek` over unplugged (virtio-mem) slots. The file size is the sum of all
  region sizes (`KvmVm::snapshot_memory_to_file`, `src/vmm/src/vstate/vm.rs`).
  Therefore, with a single memfd, the memfd content *is* the full snapshot.
- The hotpluggable region is currently allocated separately
  (`VmResources::allocate_memory_region`, called from `build_microvm_for_boot`),
  which in the memfd case produces a second memfd.
- Diff snapshots: `mark_virtio_queue_memory_dirty`, then
  `KvmVm::get_dirty_bitmap` (`KVM_GET_DIRTY_LOG`, which clears KVM's log;
  `mincore` fallback when `track_dirty_pages` is off), then `dump_dirty` writes
  every page that is dirty in either the KVM bitmap or Firecracker's own
  `AtomicBitmap` (device writes), at host page (4 KiB) granularity. On success
  Firecracker's bitmap is reset; on failure the KVM bitmap is folded back into
  it (`store_dirty_bitmap`) so no dirty information is lost.
- `Vmm::pause_vm` only pauses vCPUs. Device emulation on the VMM thread keeps
  running while the VM is `Paused`. Firecracker's snapshots are consistent only
  because vmstate save and memory dump happen synchronously in one VMM-thread
  call with no event-loop iteration in between.
- UFFD restore (`persist.rs`, `guest_memory_from_uffd`, `send_uffd_handshake`):
  Firecracker `connect()`s to a UDS, sends a JSON `Vec<GuestRegionUffdMapping>`
  plus the uffd via `SCM_RIGHTS`, then forgets the socket.

## Design

### 1. Memory allocation: one memfd for everything

When a memory manager is configured (or a vhost-user device is present), guest
memory is memfd-backed and MAP_SHARED. All regions, including the virtio-mem
hotplug region, live in one memfd whose layout equals the snapshot file layout.

Changes:

- `VmResources` owns the memfd (`Option<Arc<File>>` plus a "next offset") or,
  equivalently, a small `GuestMemoryAllocator` created by
  `allocate_guest_memory` and reused by `allocate_memory_region`. The memfd is
  sized as `DRAM + memory_hotplug.total_size` up front. DRAM regions are mapped
  first, the hotplug region (allocated later in `build_microvm_for_boot` once
  its guest address is known) is mapped at the next offset.
- `memory::create` gains a starting offset (or takes
  `(GuestAddress, size, file_offset)` triples) so the hotplug region can be
  mapped from the shared memfd at a non-zero base offset.
- Invariant, asserted in unit tests: for every region, `file_offset(region)` in
  the memfd equals the offset `dump` writes it at (regions are sorted by guest
  address; the hotplug region sits past the 64-bit MMIO hole so it is always
  last).
- On restore, all regions are known from `GuestMemoryState`, so
  `memfd_backed(mem_state.regions(), ..)` already yields one memfd.
- Out of scope: `GuestRegionMmapExt::discard_range` (balloon, virtio-mem unplug)
  keeps using `madvise(MADV_DONTNEED)`, which does not release pages of a shared
  mapping (see the existing TODO in `memory.rs`). Manager mode inherits this
  limitation from the vhost-user memfd path; fixing it is a separate workstream.
  It does not affect snapshot identity: Firecracker and the manager read the
  same memfd, and unplugged virtio-mem slots are skipped by both based on the
  `plugged` state, not on page contents.
- Performance note (already documented in `allocate_memory_regions`): shared
  mappings have more expensive page faults than anonymous memory. Manager mode
  opts into that, exactly as vhost-user does today.

vhost-user note: vhost-user block backends already receive the guest memfd over
the vhost-user protocol (`SET_MEM_TABLE`). Nothing changes for them; if both a
vhost-user device and a manager are configured they share the same memfd.

### 2. Boot-time API

New pre-boot endpoint:

```
PUT /memory-manager
{ "socket_path": "/path/to/manager.sock" }
```

- `vmm_config/memory_manager.rs`: `MemoryManagerConfig { socket_path }`.
- `VmResources::memory_manager: Option<MemoryManagerConfig>`; also settable from
  the JSON config file.
- `VmmAction::SetMemoryManager` (pre-boot only; `OperationNotSupportedPostBoot`
  afterwards), `api_server/request/memory_manager.rs`, swagger `MemoryManager`
  schema.
- Effect: forces memfd-backed memory (§1) and performs the handshake (§3) during
  `build_microvm_for_boot`, after all regions are mapped and registered with KVM
  and before vCPUs start.

### 3. Handshake

Firecracker connects to `socket_path` and sends one NDJSON message with the
memfd attached via `SCM_RIGHTS` (`ScmSocket::send_with_fd`, as UFFD does):

```json
{
  "protocol_version": 1,
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

This is `GuestMemoryState` plus fd offsets and host addresses, a superset of
`GuestRegionUffdMapping`. Unlike UFFD, the socket stays open and becomes the
request channel. `page_size` is the host page size used for dirty tracking (4
KiB even with hugetlbfs backing, matching `dump_dirty`).

The manager is trusted (same trust level as a UFFD handler). It is expected to
be started before Firecracker connects; connection failure fails boot/restore.
If the socket later disconnects, Firecracker logs an error, increments a metric
and continues: its own mapping stays valid, but since there is no reconnect in
v1, `PUT /snapshot/create` fails for the rest of the VM's life.

### 4. Request/response protocol

Newline-delimited JSON over the handshake socket, serviced on the VMM thread:
the socket fd is added to `Vmm`'s own `EventOps` in `Vmm::init` and handled in
`Vmm::process`, giving the handler `&mut Vmm` (needed for the device manager and
the KVM VM). Requests are processed one at a time; the manager must not
pipeline.

Requests:

- `{"DescribeMemory": {}}` → the `regions` array from §3 with the current
  `plugged` bitvecs (virtio-mem plug state changes over time). Ranges covering
  all plugged slots, i.e. what a full snapshot contains, are derived from it.

- `{"GetDirtyRanges": {}}` → callable in any VM state. Firecracker:

  1. calls `mark_virtio_queue_memory_dirty` (as `snapshot_memory_to_file` does,
     so queue pages are always included);
  1. takes the KVM dirty log (or `mincore` when tracking is disabled);
  1. computes the union with its own bitmap, restricted to plugged slots;
  1. resets both bitmaps (consuming semantics, identical to `dump_dirty`);
  1. replies with sorted, merged, page-aligned ranges in memfd/file offset
     space: `{"DirtyRanges": {"ranges": [{"offset": 0, "len": 8192}, ...]}}`.

  If serialising or sending the reply fails, the bitmap is folded back into
  Firecracker's `AtomicBitmap` (`store_dirty_bitmap`), mirroring the existing
  failure path, so the next call returns a superset.

- Errors:
  `{"Error": {"kind": "UnknownRequest" | "Busy" | "Internal", "message": "..."}}`.

Range encoding was chosen over raw bitmaps because it maps directly onto
`copy_file_range`/`pwrite` in the manager and is trivially language-neutral. A
`format` field can be added later if a bitmap encoding becomes necessary for
pathologically fragmented dirty sets.

Consistency note for callers: when the VM is running (or paused but outside a
snapshot window, see §5), `GetDirtyRanges` gives no atomicity guarantee: a page
can be modified after the ranges were computed and before the manager copies it.
The result is still safe (that page is dirty again and will be in the next
diff). This mode is intended for iterative pre-copy schemes.

Dirty-bitmap ownership: once a manager is configured, the dirty bitmaps (KVM log
and Firecracker's `AtomicBitmap`) belong to the manager. Firecracker only reads
and resets them when handing ranges to the manager (`GetDirtyRanges` and the
`SnapshotRequest` of §5). In manager mode `PUT /snapshot/create` never writes
guest memory itself, so `mem_file_path` is rejected there for both `Full` and
`Diff`. `mark_virtio_queue_memory_dirty` only *adds* dirty pages and stays as
is.

### 5. `PUT /snapshot/create` in manager mode

```
PUT /snapshot/create
{
  "snapshot_type": "Full" | "Diff",
  "snapshot_path": "/path/vmstate",
  "mem_backend": { "backend_type": "MemoryManager" }
}
```

`mem_backend` is mutually exclusive with `mem_file_path`. When a manager is
configured, `mem_file_path` is rejected (Firecracker no longer writes guest
memory itself) and `mem_backend.backend_type` must be `MemoryManager`; without a
configured manager, `MemoryManager` is rejected.

Why Firecracker must wait for the manager: `Paused` only stops vCPUs. Device
emulation on the VMM thread keeps running, so between the vmstate save and the
manager's copy a tap RX frame or an in-flight block completion can modify guest
memory and the used rings. Today's snapshots cannot exhibit this because vmstate
save and memory dump are one synchronous VMM-thread call; the manager path
recreates that property by keeping the VMM thread inside the `snapshot/create`
handler until the manager is done.

Firecracker, on the VMM thread:

1. requires `Paused`, as today;
1. saves vmstate to `snapshot_path` (`save_state` runs every device's
   `prepare_save`; for virtio-block this drains in-flight io_uring requests, so
   no kernel-side DMA into guest memory is pending afterwards);
1. computes the ranges the manager has to copy: for `Diff`, the `GetDirtyRanges`
   procedure of §4 (queue pages marked, KVM log taken, union with Firecracker's
   bitmap, bitmaps reset); for `Full`, all plugged slots;
1. sends
   `{"SnapshotRequest": {"snapshot_type": "Diff", "regions": [...], "ranges": [...]}}`
   on the manager socket (regions carry the current `plugged` state);
1. blocks reading exactly one reply,
   `{"SnapshotDone": {"success": true|false, "message": "..."}}`. The manager
   needs nothing else from Firecracker during the window because the ranges were
   pushed; any other message is a protocol error and fails the snapshot;
1. returns 204, or an error carrying the manager's message, to the API client.
   On `success: false`, on a protocol error or on disconnect, the ranges of step
   3 are folded back into Firecracker's bitmap (`store_dirty_bitmap`) so the
   next diff is a superset.

Writers to guest memory during step 5, and why each is quiescent: vCPU threads
are parked in their paused loop, so KVM does not run the guest; device emulation
lives on the VMM thread, which is blocked in this handler; async block I/O was
drained in step 2; the manager is trusted not to write. Pages that devices
dirtied between `PATCH /vm Paused` and `snapshot/create` are covered: they are
in the bitmaps and vmstate was captured after them, exactly as today. The order
`save_state` → mark queue pages → collect bitmaps is the same as in today's
`create_snapshot`. vhost-user backends are an external writer whose pages were
never tracked; that pre-existing gap is unchanged.

There is no timeout: the manager is trusted, and a hung manager blocks the
snapshot the same way a hung filesystem write would today. A timeout can be
added later without a protocol change.

Pushing the ranges in `SnapshotRequest` saves a round trip and makes the
consumed dirty set explicit; `GetDirtyRanges` outside the window remains
available for pre-copy schemes.

### 6. `PUT /snapshot/load`

New optional field, orthogonal to `mem_backend`:

```
PUT /snapshot/load
{
  "snapshot_path": "...",
  "mem_backend": { "backend_type": "Uffd", "backend_path": "..." },
  "memory_manager": { "socket_path": "/path/to/manager.sock" }
}
```

- `Uffd` + manager: guest memory is created with `memfd_backed` (one memfd,
  MAP_SHARED) instead of `anonymous`, the uffd is registered on that mapping,
  and the UFFD handshake is sent unchanged. The handler keeps serving faults
  with `UFFDIO_COPY`/`UFFDIO_ZEROPAGE`; both are supported on shmem-backed VMAs
  in MISSING mode on Linux 5.10 (`UFFD_FEATURE_MISSING_SHMEM`, since 4.11/4.13),
  and on hugetlbfs memfds. After the UFFD handshake, Firecracker performs the
  manager handshake (§3). The manager receives the memfd only, it does not take
  part in population.
- `File` + manager is rejected with a dedicated error for now. This is a scoping
  choice, not a technical limit: a memfd can be created, the open question is
  only how to fill it, since shmem cannot lazily reflect another file. Options,
  in order of preference if the need arises: (a) eager copy of the snapshot file
  into the memfd (`copy_file_range` file→shmem works cross-filesystem on 5.10,
  with a read/write fallback) at the cost of reading and allocating all guest
  memory at restore instead of demand-loading from the page cache; (b) an
  in-Firecracker UFFD fault-handler thread that copies pages on demand,
  effectively an internal `on_demand_handler`; (c) `MAP_SHARED` of a reflinked
  copy of the snapshot file handed to the manager, lazy but every guest write
  becomes dirty page cache subject to writeback. None of these changes the
  handshake or the protocol, so the decision can be deferred.
- Ordering inside `restore_from_snapshot`: build regions, register with KVM,
  UFFD handshake, manager handshake, then `build_microvm_from_snapshot`.

### 7. Byte-for-byte identity: how it is demonstrated

In manager mode Firecracker never writes a memory file, so a Firecracker-made
reference cannot be produced from the same VM in an integration test. Identity
is therefore established in two layers:

1. At the Rust level, both implementations are driven from identical inputs (the
   same memory, KVM bitmap, Firecracker bitmap and `plugged` state) and their
   outputs are compared byte for byte.
1. At the integration level, manager-made snapshots are shown to be
   self-consistent through rebase and to restore correctly in Firecracker, which
   only accepts the exact layout `dump` produces.

Definitions: `M_full(t)` is the manager's copy of all plugged ranges of the
memfd; `M_diff(t0,t1)` is the manager's sparse file of the ranges received in a
`Diff` `SnapshotRequest` at `t1`; `rebase(base, diff)` is the existing
`rebase-snap` tool / test-framework `Snapshot.rebase_snapshot`.

Rust unit tests (`src/vmm/src/vstate/memory.rs`, `persist.rs`):

- Given the same `(kvm_bitmap, firecracker_bitmap, plugged)` input, the range
  list produced for `GetDirtyRanges` covers exactly the pages `dump_dirty`
  writes (property-style test over random bitmaps, including a trailing slot
  whose page count is not a multiple of 64 and unplugged slots).
- Applying those ranges from a memfd into a file yields a file identical to
  `dump_dirty`'s output; applying the plugged-slot ranges of a `Full`
  `SnapshotRequest` yields `dump`'s output.
- The memfd offset of every region equals the offset `dump` writes it at, with
  and without a hotplug region.

Reference manager (`src/firecracker/examples/memory_manager/`, Rust, built like
the `examples/uffd/*` binaries): performs the handshake, answers
`SnapshotRequest` by `copy_file_range`-ing the received ranges from the memfd
into a target file (creating it at `total_size` for `Full`, merging into an
existing base for `Diff`), and sends `SnapshotDone`. It is the manager used by
the integration tests and documentation.

Python framework (`tests/framework/utils_memory_manager.py`,
`Microvm.memory_manager`, `SnapshotType.FULL_MANAGER/DIFF_MANAGER`).

Integration tests (`tests/integration_tests/functional/test_memory_manager.py`):

1. Full snapshot layout: boot with manager, run a workload, pause,
   `snapshot/create Full` → `M_full(t0)`. Assert its size is `total_size`, that
   unplugged virtio-mem slots are all zero, and that Firecracker restores from
   it (`File` backend, no manager) with a healthy guest.
1. Diff self-consistency through rebase (the manager-side analogue of
   `test_snapshot_basic.py::test_cmp_full_and_first_diff_mem`): resume, run a
   workload, pause, `snapshot/create Diff` → `M_diff(t0,t1)`; resume the *same*
   VM, pause again without running anything, `snapshot/create Full` →
   `M_full(t1)`. Assert `rebase(M_full(t0), M_diff) == M_full(t1)`. Any dirty
   page missed by `GetDirtyRanges` shows up as a mismatch.
1. Chains: several manager diffs rebased onto the base equal a final manager
   full copy, and the rebased result restores with a healthy guest.
1. Variants of 1–3: virtio-mem with unplugged slots, balloon inflated, hugetlbfs
   2M, `track_dirty_pages=false` (`mincore` path), x86 with >4 GiB (two DRAM
   regions), aarch64.
1. Restore round trip: a manager-produced snapshot restores with the `File` and
   `Uffd` backends and the guest is healthy; the existing UFFD example handlers
   are reused unchanged.
1. Restore with `Uffd` + `memory_manager`, then create another manager diff and
   repeat test 2; the UFFD handler must work unchanged on the shmem-backed
   mapping.
1. Cross-check against a Firecracker-made snapshot: boot a VM *without* manager
   from the same base snapshot, pause immediately, take a classic `Full`
   snapshot and compare with a manager `Full` of a VM with manager restored from
   the same base and paused immediately. This is only meaningful because nothing
   ran in either VM; it checks layout, hole handling and the `total_size`
   convention end to end rather than dirty tracking.
1. Negative: `MemoryManager` backend without a configured manager → 400;
   `mem_file_path` in manager mode → 400; `File` + `memory_manager` on load →
   400; manager disconnect → Firecracker keeps running and `snapshot/create`
   reports an error instead of hanging; `SnapshotDone{success: false}` → error
   surfaced to the API client and the next diff includes the failed ranges.

### 8. Security and operational notes

- Threat model: the manager is as trusted as a UFFD handler and receives a
  read/write fd to all guest memory. It should run in the same jail/user as the
  Firecracker instance; the socket path is interpreted inside the jailer chroot.
- Seccomp: audit the VMM-thread filter for `connect`, `sendmsg`, `recvmsg`;
  `copy_file_range` is only used by the manager.
- The memfd remains sealed against resize. Firecracker keeps its own fd, so
  manager death never invalidates guest memory.
- Metrics:
  `memory_manager.{handshake_fails, requests, request_fails, disconnects, snapshot_request_duration_us}`.

### 9. Work breakdown (PR sequence)

1. Single-memfd allocation for DRAM + hotplug; `memory::create` base offset;
   unit tests for the offset invariant. No API change, mergeable alone.
1. `MemoryManagerConfig`, `PUT /memory-manager`, handshake on boot, socket
   registered in `Vmm::init`; `DescribeMemory`; seccomp; metrics.
1. `GetDirtyRanges` with range computation factored out of
   `GuestMemorySlot::dump_dirty` so both paths share one iterator over dirty
   pages; unit identity tests.
1. `PUT /snapshot/create` in manager mode: `SnapshotRequest`/`SnapshotDone`
   window on the VMM thread, `mem_file_path` rejection, failure fold-back.
1. `PUT /snapshot/load` `memory_manager` field: `Uffd`+manager (memfd + UFFD),
   `File`+manager rejected.
1. Reference manager, Python framework, integration tests (§7).
1. Docs (`docs/snapshotting/guest-memory-manager.md`, update
   `snapshot-support.md` and `handling-page-faults-on-snapshot-resume.md`),
   swagger, CHANGELOG.

### 10. Decisions taken

- The manager is trusted; connection failure at boot/restore is fatal, later
  disconnects are logged and Firecracker continues.
- Dirty information is exchanged as page-aligned `{offset, len}` ranges.
- `PUT /snapshot/create` in manager mode never writes memory; it pushes the
  ranges to the manager and blocks the VMM thread until `SnapshotDone`. No
  timeout for now.
- `GetDirtyRanges` is callable in any VM state and always resets. A
  non-resetting variant can be added later if needed.
- Restore: `Uffd` + manager supported, UFFD protocol unchanged; `File` + manager
  rejected for now.
- Balloon/virtio-mem discard behaviour on shared mappings is out of scope.
