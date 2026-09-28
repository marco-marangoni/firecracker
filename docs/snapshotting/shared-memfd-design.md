# Shared memfd: design

Status: implemented (developer preview) as the `SharedMemfd` memory backend. The
user-facing documentation is `shared-memfd.md`; this document records the design
rationale. It predates the name: where it says "memory backend" as a mode, read
`SharedMemfd`.

**Revision 2 (implemented).** Revision 1, the first implementation on this
branch, exchanged dirty information over the HTTP API: `PUT /snapshot/create`
answered `200` with a base64 bitmap and `PUT /snapshot/dirty-pages` returned the
same for pre-copy passes, and the orchestrator relayed the bitmap to the
backend. This revision moves the exchange onto the socket the handshake already
uses:

- the backend asks Firecracker for the dirty set on the handshake connection,
  and Firecracker answers there, with the bitmap as raw bytes (§8);
- `PUT /snapshot/create` writes vmstate only and keeps answering `204` (§7);
- `PUT /snapshot/dirty-pages` is gone; the HTTP API surface of the feature
  shrinks to `machine-config.mem_backend`, `backend_type: SharedMemfd` on
  `snapshot/load`, and `mem_file_path` becoming optional on `snapshot/create`;
- the connection is serviced by the API thread, not the VMM thread (§8).

Sections whose text changed for this revision say so; §15 lists everything the
change touched in code, tests and documentation, what was settled during the
implementation, and the alternatives that were weighed.

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
memfd, hand it over, and tell the backend, when it asks, which pages of it
constitute a snapshot. Copying bytes is the peer's job. The orchestrator drives
the sequence over the HTTP API but never sees the dirty information.

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
- `PUT /snapshot/create` requires `Paused` and always answers 204.
- The `firecracker` binary runs the HTTP API on its own thread (`fc_api`,
  `ApiServer::run`, `src/firecracker/src/api_server/mod.rs`): a `micro_http`
  server with an epoll loop that turns each request into a `VmmAction`, sends it
  to the VMM thread over an `mpsc` channel plus an eventfd kick, blocks for the
  `VmmData`/error answer and writes the HTTP response. The VMM thread reads that
  channel from its `EventManager` loop while running and from a blocking `recv`
  loop while paused (see above). With `--no-api` there is no such thread.

What is missing for a memory backend: a single memfd covering all regions, a way
to hand it over (at boot as well as at restore), and a way for the backend to
learn which pages of the memfd a snapshot consists of.

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
  returned with every dirty set (§8); the total size is the sum of region sizes;
  how Firecracker tracks dirty pages (`KVM_GET_DIRTY_LOG` or `mincore`) is
  invisible to a peer that receives a page bitmap.
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
- *(Revised.)* What happens to the connection after the handshake depends on the
  backend type. For `Uffd` Firecracker `forget`s the stream, exactly as today:
  it never reads it, never writes to it again, and a handler written for today's
  protocol sees no difference. For `SharedMemfd` the connection stays open as
  the control channel on which the backend requests dirty information (§8);
  Firecracker reads requests from it and writes replies, and nothing else.
  Firecracker never sends anything unsolicited, so a backend that never asks
  never has to read either. In both cases the backend can use the connection to
  detect Firecracker exiting and obtain Firecracker's PID via `SO_PEERCRED`, as
  today, and nothing Firecracker does depends on the backend being alive.
- Trust and lifecycle: the backend is trusted (same as a UFFD handler today);
  connection failure fails boot/restore.

Consequences for the rest of the design:

- Implementation: `send_uffd_handshake` takes a slice of fds instead of one and,
  for `SharedMemfd`, returns the stream instead of forgetting it. The handshake
  message itself is unchanged. The request/reply exchange that follows on a
  `SharedMemfd` connection is new (§8) and is the only other protocol
  Firecracker speaks to a backend.
- Existing handlers become memory backends by reading one more fd and, if they
  want to produce snapshots, by sending one kind of request on the connection
  they already hold. The example handlers in `src/firecracker/examples/uffd/`
  are extended, not duplicated (§9).
- `backend_type: SharedMemfd` does not select a different handshake; it means
  "the `Uffd` backend, and also hand over the memfd and keep the line open" (§6
  for the name).
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
virtqueues. The bytes in the memfd are, for the rest of the pause, the bytes
`dump`/`dump_dirty` would have written at that moment, and the dirty set the
backend obtains at any point in that window (§8) is the set `dump_dirty` would
have written. The peer can therefore ask and copy at its own pace, as long as it
finishes before the resume. There is no window to hold open, no acknowledgement
to wait for and no blocked VMM thread.

This is a statement about *writes to guest memory after `snapshot/create`
returns*, nothing broader. Firecracker is not idle while paused:
`snapshot/create` itself modifies guest memory (draining block I/O completes
reads into guest buffers); a `DirtyPages` request from the backend reads and
resets the dirty bitmaps; configuration requests read and modify device state.
None of them modify guest memory once `snapshot/create` has returned, which is
all the argument needs.

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

*(Revised.)* With a backend, `snapshot/create` does not touch the dirty tracking
state at all: it does not read the KVM log, reset a bitmap or mark the virtqueue
pages. Dirty information accumulates until the backend asks for it. Three
ordering rules follow, and the doc for the feature must state them:

1. The dirty set that accompanies a snapshot is the one returned to a
   `DirtyPages` request issued **after `snapshot/create` has returned and before
   `Resume`**. It is a superset of everything vmstate reflects: the pages the
   block drain wrote inside `snapshot/create` were marked dirty as they were
   written, and nothing has modified guest memory since. This is the only
   request whose answer is a consistent snapshot; how the backend learns that
   `snapshot/create` has returned is the orchestrator's business (it made the
   call).
1. A request issued while `Running` is a pre-copy pass. It never loses pages
   (anything it misses because a completion has not been processed yet is marked
   later and shows up in a subsequent set), but a page it reports may be written
   again before the backend copies it.
1. A request issued between `Pause` and `snapshot/create` is legal and behaves
   like a pre-copy pass with a quiet guest, but its answer is *not* the final
   set: `snapshot/create` may still write guest memory when it drains block I/O.
   A backend that copies from it and then does not ask again after
   `snapshot/create` produces a snapshot whose memory predates its vmstate.

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

*(Revised: in revision 1 this request answered `200` with the dirty bitmap; it
now answers `204` and leaves the dirty state alone.)*

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

With a backend the request does exactly one thing: `save_state` (which runs
every device's `prepare_save`: block drain and `fsync`, net RX buffer reset) and
vmstate to `snapshot_path`. It does not read the KVM dirty log, does not reset
either bitmap and does not mark the virtqueue pages; the memory part of the
snapshot is between the backend and Firecracker's socket (§8). The response is
`204 No Content`, as without a backend. `snapshot_type` has no effect with a
backend: it describes the memory file, and the backend decides what it writes.
It is accepted rather than rejected because `#[serde(default)]` makes an absent
field indistinguishable from `Full`, and rejecting `Diff` would force an
orchestrator producing a diff to say otherwise.

Procedure, on the VMM thread, inside the paused API loop:

1. require `Paused`, as today;
1. validate `mem_file_path` against the presence of a backend, before anything
   is written;
1. `save_state` and write vmstate to `snapshot_path`, as today.

Failure handling is that of today's request: nothing has been consumed, the
error is returned. The backend is never consulted, so a dead backend does not
fail the request; the orchestrator finds out when it tells the backend to copy.

Why the dirty set does not come back in this response any more:

- The consumer of the dirty set is the backend, not the orchestrator. In
  revision 1 the orchestrator received 32 KiB per GiB of base64 it had no use
  for except to hand it on, and every orchestrator had to grow a relay and a
  second protocol to its backend for it.
- Splitting the operation in two (vmstate here, memory on the backend
  connection) costs nothing in consistency: the pause invariant (§4) guarantees
  that whatever the backend asks for after this request has returned is the set
  `dump_dirty` would have written at this point.
- A response body on `PUT /snapshot/create` was the one place where the HTTP API
  changed shape depending on how the instance was started (`204` or `200`). It
  no longer does. `mem_file_path: Option` is the only remaining difference.

### 8. The dirty-pages exchange on the backend connection

*(Revised: replaces `PUT /snapshot/dirty-pages`, which is removed.)*

After the handshake, a `SharedMemfd` connection is a request/reply channel with
one request in it. The backend asks for the pages dirtied since the dirty state
was last consumed; Firecracker answers with a bitmap and the current list of
unplugged slots, and resets the tracking. This is the only message Firecracker
ever reads from a backend, and the reply is the only thing it ever writes after
the handshake. It is never sent unsolicited.

#### Framing

The handshake message stays as it is: an unframed JSON array read with a single
`recvmsg` together with the fds (§2). Every message *after* it, in either
direction, is a frame:

| offset         | size       | field                                     |
| :------------- | :--------- | :---------------------------------------- |
| 0              | 4          | `json_len`, `u32`, little-endian          |
| 4              | 4          | `blob_len`, `u32`, little-endian          |
| 8              | `json_len` | a JSON object, UTF-8                      |
| 8 + `json_len` | `blob_len` | binary payload, meaning given by the JSON |

The JSON carries everything small and structured, the blob carries the one bulk
item, the bitmap, as raw bytes. There is no base64 anywhere. A frame is written
with a single `writev` where possible; a reader accumulates until it has 8
bytes, then until it has the rest. Messages on the connection are strictly
alternating (one outstanding request at a time, from the backend); Firecracker
does not need to and does not queue.

Limits enforced by Firecracker on frames it reads: `json_len` at most 64 KiB,
`blob_len` must be 0 (no request carries a blob today). A frame that violates
either cannot be resynchronised and closes the connection (§8, errors).

#### Request

```json
{ "request": "DirtyPages" }
```

as a frame: `18 00 00 00 00 00 00 00` followed by the 24 bytes of the JSON
without whitespace (byte counts here and below are for the compact form;
whitespace is legal and simply counted). Unknown fields are rejected
(`deny_unknown_fields`): a future optional field such as `"consume": false` sent
to a Firecracker that does not know it must fail loudly rather than be silently
ignored, because ignoring it would do the opposite of what was asked. Extensions
to the request are therefore new fields with the understanding that a backend
sends them only when it knows the Firecracker version it talks to, or falls back
on an error reply.

#### Reply

```json
{
  "total_size": 65536,
  "page_size": 4096,
  "unplugged": [{ "offset": 32768, "len": 16384 }]
}
```

followed by the bitmap as the blob. For this 64 KiB example the frame is
`50 00 00 00 02 00 00 00`, the 80 bytes of the JSON above (compact), then the
two bytes `03 10`: pages 0, 1 and 12 are to be copied, pages 8 to 11 are an
unplugged virtio-mem slot, to be zeroed. A 1 GiB guest has a 32 KiB blob.

- `total_size` is the size of a full memory file (sum of all region sizes,
  including the hotplug region); the peer creates the target at that size.
- `page_size` is the granularity of the bitmap, in bytes. It is the host page
  size (4096 today), not the backing page size: on hugetlbfs the bits are still
  per 4 KiB page.
- The blob is a bitmap with one bit per `page_size` bytes of the memory file.
  Byte `i`, bit `b` (least significant bit first) stands for the page at offset
  `(8 * i + b) * page_size`. A set bit means: this page of the target must be
  brought up to date with guest memory. For a booted microVM the bytes are read
  from the memfd; after a restore the backend reads pages it has not populated
  from its own snapshot file instead (§9). The bitmap covers the file from
  offset 0 to the end of the last plugged slot,
  `ceil(plugged_end / page_size / 8)` bytes, whatever is dirty; pages past its
  end are clear, and bits of unplugged slots are never set. `blob_len` is that
  length; it is a function of the plug state alone, so a peer can size its
  buffers once.
- `unplugged` are the currently unplugged virtio-mem slots, as sorted,
  page-aligned, non-overlapping `{offset, len}` pairs. The peer must zero them
  in the target: a fresh `dump`/`dump_dirty` output has zeros there, and a slot
  unplugged since the last diff would otherwise keep stale bytes in a merged
  file. (Firecracker's own in-place merge into an existing memory file does
  leave stale bytes there; restore never maps them, so both are valid, and
  zeroing is what makes the peer's file byte-identical to a fresh one.) The
  bitmap and `unplugged` are disjoint: the bitmap says what to copy, the list
  what to zero.
- The backend must ignore fields it does not know in the reply. Extensions to
  the reply are additive fields, as for the handshake.

All offsets are memfd offsets, which by §1 are file offsets. No per-region
information is needed by the peer, so none is returned. The bitmap is one bitmap
over the whole file, not one per region, for the same reason.

There is no "full" variant of the reply. Revision 1 omitted the bitmap for a
`Full` `snapshot/create`; now Firecracker does not know or care whether the
backend is about to write a full file or a diff. A backend taking a full copy
copies every page outside `unplugged` and ignores the bitmap, but it still
issues the request, because the request is also the reset: the next diff must be
relative to this copy. (Not resetting would only make the next set a superset,
which is correct, but wasteful.)

An error reply is a frame whose JSON is

```json
{ "error": "<message>" }
```

with `blob_len` 0.

#### Semantics

- Callable while `Running` or `Paused`, and, in practice, from the moment the
  microVM is built (the connection is serviced from the end of `InstanceStart`
  or `snapshot/load` on; a request sent earlier waits in the socket buffer).
- Consuming: the same computation revision 1 did for a `Diff`: for every plugged
  slot, take the KVM dirty log (or `mincore`) and OR it with Firecracker's
  `AtomicBitmap`; skip unplugged slots (they go into `unplugged`); reset both
  bitmaps; then `mark_virtio_queue_memory_dirty`, so that the queue pages, whose
  runtime accesses are not tracked, are in the next set. The bit layout is
  KVM's: `KVM_GET_DIRTY_LOG` fills an array of `u64` in which bit `j` of word
  `i` is page `64 * i + j` of the slot, which on a little-endian host is byte
  for byte the layout above. Building the reply is an OR of the two per-slot
  bitmaps into the right position of one `Vec<u8>`, with no bit reshuffling, and
  that `Vec<u8>` is the blob, with no encoding step at all.
- Before resetting, Firecracker calls
  `VirtioDevice::prepare_dirty_tracking_reset` on every activated device.
  virtio-net marks RX buffers dirty when it *parses* them
  (`IoVecBufferMut::load_descriptor_chain`), not when a frame is later written
  into them; a buffer parsed before a reset and filled after it would otherwise
  never show up in a later set. The hook does for virtio-net what `prepare_save`
  does (return parsed, unfilled descriptors to the queue) without the
  snapshot-only side effects of `prepare_save` on other devices (vsock
  connection reset, block drain and `fsync`). Any future device that marks
  memory dirty ahead of writing must implement it.
- Gives no atomicity with respect to concurrent guest or device writes while
  `Running`: a page can be modified after the set was computed and before the
  peer copies it. It is then dirty again and will be in the next set. This is
  the intended use: iterative pre-copy, with `PATCH /vm Paused` +
  `snapshot/create` + one last request as the final pass (§4, ordering rules).

What the bitmap promises: every plugged page whose guest-visible content changed
since the dirty state was last consumed has its bit set, including pages that
became zero because they were released by the balloon, or because their slot was
unplugged and plugged again in between. It may also have bits set for pages that
did not change (today: the virtqueue pages re-marked after every reset); copying
an extra page is always correct. Today the set bits are exactly the pages
`dump_dirty` would write, and unit tests assert that (§10), but that is a
property of this implementation, not a promise to the peer: a future dirty
tracking mechanism (`KVM_CAP_DIRTY_LOG_RING`, manual clearing, coarser tracking)
may over-approximate without changing the protocol.

Unplugged slots, in one place. Dirty *tracking* and *reporting* are independent:
`discard_range` marks every discarded range dirty in Firecracker's bitmap,
whether the balloon or a virtio-mem unplug caused it, and that mark survives
until the next consumption. Reporting then depends on the slot's state at the
time of the request. A slot that is unplugged at that moment is described by
`unplugged` alone: its bits are never set, whatever the bitmap says, and the
peer's obligation is that every byte of every `unplugged` range is zero in the
file it produces. A slot that was unplugged and plugged again before the request
is plugged now, so the marks the unplug left are reported like any other dirty
page: its content became zero and the peer must copy it (zeros, or whatever the
guest wrote since). Keeping unplugged slots out of the bitmap, and ending the
bitmap with the last plugged slot, is what gives the size guarantee above: an
operator can configure a hotplug region of any size and leave it unplugged
without the replies growing by a byte, and an unplug event does not produce a
burst of set bits in the next reply either. (Firecracker's own `dump_dirty`
seeks over unplugged slots as well; the stale bytes it leaves in an in-place
merge are never mapped by restore.)

#### Errors and peer death

- If taking the KVM log fails, the bitmaps are folded back with
  `store_dirty_bitmap`, as `dump_dirty` does today, nothing is consumed, and an
  error reply is sent.
- A request that is valid JSON but not a known request, or has unknown fields,
  gets an error reply; the connection stays usable.
- A frame that breaks the limits, or bytes that are not valid JSON, cannot be
  answered meaningfully (the stream position is lost) and close the connection.
  The microVM is unaffected; `snapshot/create` keeps working (vmstate only);
  nobody can produce memory snapshots for this instance any more. There is no
  reconnection: a backend that loses the connection has lost its only channel,
  as a UFFD handler that closes its uffd has today.
- Firecracker seeing EOF or `HUP` on the connection stops servicing it and logs
  it. Same consequences as above. This is as far as "monitoring the backend"
  goes; nothing depends on it.
- Once a reply has been handed to the socket the dirty set is consumed. If the
  connection dies mid-reply the set is gone and, if the backend survives, its
  next copy must be a full one. Over a local stream socket to a trusted peer
  this needs the peer to fail while reading; it is the same contract as losing a
  `Diff` file today, and a much narrower window than losing an HTTP response
  through the orchestrator.
- Writing the reply blocks until it is written (a `write` loop on the
  non-blocking socket with `poll(POLLOUT)` in between; 32 KiB per GiB of guest
  memory, more than a default socket buffer beyond 4 GiB). The peer asked and is
  reading; a peer that stops reading stalls the API thread until it reads or
  dies (`EPIPE`, connection closed). This is a trusted-peer hazard of the same
  order as a UFFD handler that stops serving faults, and is documented rather
  than engineered around; buffered writes interleaved with HTTP service, as
  `micro_http` does for HTTP connections, are a contained follow-up if it ever
  matters.

Metrics:
`mem_backend.{dirty_pages_requests, dirty_pages_fails, protocol_errors}`, plus
`handshake_fails` (§11).

#### Decision: the connection is serviced by the API thread

Firecracker has to read this socket somewhere. The candidates:

- **The VMM thread, through the `EventManager`.** Rejected. The final request of
  every snapshot arrives while `Paused`, and the paused loop in
  `ApiServerAdapter::handle_request` does not run the event manager: it blocks
  on `from_api.recv()` and handles API requests only, which is the invariant §4
  is built on. Serving the socket there would mean a second wait mechanism in
  the paused loop (an epoll over the API eventfd and the socket) that duplicates
  what the API thread already does, plus a blocking read on the thread that runs
  device emulation.
- **A dedicated thread.** Rejected: a thread, a seccomp filter and a channel of
  its own, to do what the API thread does already.
- **The API thread.** Chosen. The API thread already is "external requests in,
  `VmmAction` to the VMM thread over the channel, response out", with an epoll
  loop and a seccomp filter that allows `read`, `write`, `recvfrom`, `sendto`
  and `epoll_ctl`. The backend connection becomes a second front end to the same
  machinery: a `DirtyPages` frame is turned into `VmmAction::GetDirtyPages`,
  sent through the same channel with the same eventfd kick, and the
  `VmmData::SnapshotMemory` answer is framed instead of being turned into an
  HTTP body. The VMM-thread side (`Vmm::dirty_pages`) is unchanged from revision
  1, requests from the backend are serialised with HTTP requests for free (the
  API thread handles one request at a time), and the paused loop receives them
  through the channel it already reads, so nothing about the pause invariant
  moves.

Where the fd travels: the handshake is performed on the VMM thread, inside
`build_microvm_for_boot` / `restore_from_snapshot`, because it needs the region
addresses and the fds that only exist there. `send_uffd_handshake` returns the
`UnixStream` for `SharedMemfd`, `Vmm` holds it
(`Vmm::mem_backend: Option<UnixStream>`) until the microVM is built, and
`run_with_api` moves it to the API thread through a channel plus an eventfd
registered in the HTTP server's epoll. From then on the API thread owns it.
`micro_http`'s `HttpServer` today panics on an event from an fd it does not
know, so either it grows a small "external fd" registration (`requests()`
reporting `ServerError::External(fd)` next to `ShutdownEvent`), or Firecracker
nests the server's `epoll()` fd in an outer epoll of its own together with the
two extra fds. The nesting needs no `micro_http` change; the extension is
cleaner. To be settled in implementation.

`--no-api`: there is no API thread, and nothing would service the connection. A
backend in that mode could only ever do pre-copy passes towards a snapshot that
cannot be taken (no API to pause or to write vmstate), so
`machine-config.mem_backend` with `--no-api` is rejected at build time with a
clear error rather than accepted with a request nobody answers.

Pre-boot, the stream does not exist (the handshake is part of `InstanceStart` /
`snapshot/load`), so `PrebootApiController` never sees `GetDirtyPages`.

A hazard for backend authors, new in this revision: the backend now waits on
Firecracker. If it does so synchronously, it is not serving page faults while it
waits, so every vCPU that faults meanwhile stalls for the duration of the
request; and if Firecracker's handling of the request ever touched a page the
backend has not populated, both would wait for each other. Firecracker's
dirty-pages path does not read or write guest memory today (it reads the KVM log
and Firecracker's own bitmaps, and rewinds a queue index in device state), and
the design intends to keep it that way, but a backend should not build on it:
the reference handler reads the reply from inside its poll loop and keeps
serving faults until the frame is complete (§9). Revision 1 did not have this
hazard because the orchestrator made the HTTP call while the handler kept
running.

#### Decision: pull, not push

Firecracker could instead compute the dirty set inside `snapshot/create` and
push it to the backend on the connection as an unsolicited message, keeping
`snapshot/create` as the atomic "vmstate plus final set" it was in revision 1
and saving the orchestrator the step of telling the backend to go. Not chosen,
for now:

- Two message kinds in two directions instead of one request and one reply, and
  a backend that must be prepared to receive at any time, including while a
  pre-copy request of its own is in flight (the stream keeps the order, but the
  backend has to demultiplex).
- `snapshot/create` would have to wait for the write to the backend to complete
  (or fail on a dead backend), reintroducing a dependency on the backend into a
  request that has none today.
- The orchestrator has to tell the backend where to write and when to start
  anyway; "the vmstate is written, take the final set now" is one more field in
  a message it already sends. The extra hop is a local round trip on the
  downtime path, microseconds against a copy that is at least milliseconds.

The framing leaves room for it: a frame from Firecracker whose JSON has a
`"notification"` key rather than a reply shape can be added later without
breaking a backend that only ever sees replies to its own requests, as long as
backends are told from the start to read the JSON before assuming it is a reply.

#### Decision: a bitmap, not a range list

*(Unchanged in substance; the base64 argument no longer applies because the
bitmap is now raw bytes.)*

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
- A bitmap is 32 KiB per GiB, whatever the guest does. In the idle case that is
  larger than the range list (a few KiB), but it is noise next to the
  `KVM_GET_DIRTY_LOG` ioctl and a single `copy_file_range`, and a protocol whose
  cost depends on the guest's access pattern is the thing that cannot be fixed
  later.
- As raw bytes on the socket the bitmap costs Firecracker nothing beyond the OR
  of the per-slot KVM bitmaps it has to do anyway; in revision 1 base64 encoding
  and `serde_json` scanning the string added about 1.5 ms per GiB on the VMM
  thread and a third to the size. An array of integers in JSON would be larger,
  slower to parse, and `u64` does not fit a JSON number in JavaScript.
- Coarser tracking (reporting 512 KiB chunks, say) was considered and rejected
  as a knob: KVM reports 4 KiB pages, so it saves nothing on Firecracker's side,
  and rounding out a sparse set multiplied the bytes to copy by 7–17× in the
  late pre-copy passes that decide downtime. With a bitmap the peer makes that
  trade itself, per pass, by merging runs across small gaps to save copy calls,
  and Firecracker has no opinion about it.
- The peer already keeps a per-page bitmap of what it populated (§9) and a
  hugetlbfs peer rounds decisions to its backing page; both are bitwise
  operations on same-shaped arrays. Against a range list they are a splitting
  loop.
- Everything that may change how the bitmap is produced
  (`KVM_CAP_DIRTY_LOG_RING`, `KVM_CAP_MANUAL_DIRTY_LOG_PROTECT2`,
  over-approximation) produces a bitmap naturally and needs no protocol change.

The bitmap covers the file up to the end of the last plugged slot, and no
further, and unplugged slots never have bits set. The hotplug region is always
the last region in the file (§1) and can be gigabytes of unplugged memory; a
bitmap over the whole file would carry 32 KiB of zeros per GiB of it in every
reply, and reporting the unplug marks would add a burst of set bits after every
unplug. Ending at the last plugged slot rather than at the last set bit was
chosen so that the length is a function of the plug state alone: a peer can size
its buffers once, and a reply with nothing dirty still looks like every other
reply. The peer needs no region knowledge either way: whatever lies past the end
is clear. Leading zeros are *not* omitted (there is no `pages_offset` field):
the virtqueue pages live in low memory and are re-marked dirty after every
reset, so the first bytes are never zero in practice, and such a field could not
be added later without breaking clients that predate it, so the decision is made
now, against.

`unplugged` stays a range list in the JSON: it is a "zero this" instruction
rather than dirty information, and it is a handful of huge extents.

#### Decision: dirty pages only with a memory backend

In revision 1 this was an explicit `400` on `PUT /snapshot/dirty-pages`. It is
now structural: the request can only arrive on a `SharedMemfd` connection, and
Firecracker never reads a `Uffd` connection. The reasoning stands and is why the
HTTP endpoint was not kept as a second front end: without a memory backend,
Firecracker is the only party that can turn the bitmaps into bytes, so a
consuming call would silently punch holes in the next Firecracker-written `Diff`
with no way to compensate, and the bitmap would describe offsets into a memfd
that does not exist. A non-consuming "peek" (take the KVM log, union with
Firecracker's bitmap, fold the union back so nothing is lost, return the bitmap)
remains possible later, on the socket as `"consume": false` or over HTTP for a
monitoring use case (estimating the next diff before pausing), if one appears.

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
- *(Revised.)* The handshake stream stays in the poll set, as today, and gains a
  second role: `Runtime::request_dirty_pages()` writes a `DirtyPages` frame on
  it and arms a frame reader; the poll loop keeps serving faults and feeds bytes
  from the stream into the reader until the reply frame is complete, then hands
  the `(header, bitmap)` pair to whoever asked. Reading the reply synchronously
  would stop fault serving for the duration of the request and is exactly the
  pattern §8 warns against, so the reference handler does not do it. A frame
  codec (`Frame::{encode, Decoder}`) is the whole protocol implementation on the
  backend side; the base64 decoder revision 1 needed is gone.
- A control socket of the handler's own, unrelated to Firecracker, on which the
  orchestrator (the test framework) sends
  `{"Copy": {"mem_path": "...", "full": true|false}}` (one request per
  connection). The handler issues a `DirtyPages` request to Firecracker, then
  writes the pages into `mem_path` (all plugged pages for `full`, the set pages
  otherwise; creating the file at `total_size`, merging into an existing file
  for diffs, zeroing `unplugged`, `copy_file_range` with a read/write fallback)
  and replies
  `{"Done": {"success": true|false, "message": "...", "set_pages": N}}`.
  `set_pages` is the number of bits set in the bitmap it received, which is what
  lets the tests observe consumption without the framework ever seeing a bitmap.
  This is one module shared by all example handlers.

The split reflects the real deployment: the orchestrator talks HTTP to
Firecracker and whatever it likes to its backend; the backend talks to
Firecracker on the one connection it was handed, and only to ask for the dirty
set. The existing handlers keep working unchanged when started as plain UFFD
handlers: they never send a request, and Firecracker never reads their
connection.

#### Where a page's bytes come from

The original version of this document assumed the memfd is always the source.
That holds for a booted microVM, but not after a restore: the memfd starts
empty, and a page only holds content once the handler has served a fault for it.
For every other page a read through Firecracker's mapping would fault and the
handler would serve the snapshot file, which is therefore the guest-visible
content and what Firecracker's own `dump` would have written. The copy must
follow the same rule:

| Page                                                  | Read from                  |
| :---------------------------------------------------- | :------------------------- |
| uffd-registered, never populated by the handler       | snapshot file, same offset |
| populated (`UFFDIO_COPY`/`UFFDIO_ZEROPAGE`, `EEXIST`) | memfd                      |
| in a range received as uffd `remove` event            | memfd (a hole, reads zero) |
| unplugged virtio-mem slot                             | zero                       |

Only the handler can tell these apart, and it can do so exactly and for free: it
is the only party that populates pages, and it is told about every discard.
`uffd_utils.rs` keeps one bit per page (`PopulatedPages`), set on every
successful populate and for every range it unregisters on `remove`, and its copy
routine ANDs the received bitmap with it to split the pages into runs from the
memfd and runs from the snapshot file. Since the memfd and the snapshot file
share one layout, this is a matter of offsets. Alternatives in which Firecracker
determines the source were considered and rejected (§13).

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
it). That event is what lets the handler treat the range as memfd-authoritative
(third row above); the recommended handling is to unregister the range, after
which the kernel serves zero pages for it without the handler. A handler that
ignores `remove` events, or keeps a punched range registered while its bitmap
still says the pages are populated, produces copies that differ from the guest's
view.

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

- Given the same `(kvm_bitmap, firecracker_bitmap, plugged)` input, the bitmap
  produced for a `DirtyPages` reply has exactly the pages `dump_dirty` writes
  set (property-style test over random bitmaps, including a trailing slot whose
  page count is not a multiple of 64 and unplugged slots); every plugged page,
  i.e. everything outside `unplugged`, is exactly the set `dump` writes.
- Copying the set pages from a memfd into a file yields a file identical to
  `dump_dirty`'s / `dump`'s output.
- The memfd offset of every region equals the offset `dump` writes it at, with
  and without a hotplug region.
- *(Revised.)* The frame codec round-trips (header, JSON, blob) for empty,
  one-byte and 32 KiB blobs; oversize `json_len` and non-zero `blob_len` on a
  request are rejected without reading the payload. The API thread's handling is
  tested over a `socketpair`: a `DirtyPages` frame produces
  `VmmAction::GetDirtyPages` on the channel and a `VmmData::SnapshotMemory`
  answer comes back as a well-formed reply frame; an error variant comes back as
  an error frame; garbage closes the socket and the HTTP server keeps serving.

Integration tests (`tests/integration_tests/functional/test_memory_backend.py`;
framework support in `tests/framework/utils_uffd.py` (`UffdHandler` with a
control socket and `copy(mem_path, full)`), `Microvm.mem_backend`,
`Microvm.spawn_mem_backend`, `make_snapshot` branching on an attached backend
(vmstate over HTTP, then `copy()` on the handler), `Microvm.precopy_pass()` as a
thin wrapper over `copy(full=False)`, `restore_from_snapshot(mem_backend=True)`
and `basic_config(mem_backend=<handler>)`). *(Revised: `Microvm.dirty_pages`,
the base64 decoding and the `200`-for-`PUT` allowance in `http_api.request` go
away; the framework never sees a bitmap, it sees the handler's `set_pages`
count.)* The framework plays the orchestrator: it calls `snapshot/create`, then
tells the backend to copy. Definitions: `M_full(t)` is the backend's copy of all
plugged pages; `M_diff(t0,t1)` is the backend's sparse file of the pages
returned to the request it issues after a `snapshot/create` at `t1`; `rebase` is
the existing `rebase-snap` tool / `Snapshot.rebase_snapshot`.

1. Full snapshot layout: boot with a backend, run a workload, pause,
   `snapshot/create`, `copy(full)` → `M_full(t0)`. Assert size `total_size` and
   that Firecracker restores from it (`File` backend) with a healthy guest.
   (`test_boot_full_snapshot_restores`)
1. Diff self-consistency (the backend-side analogue of
   `test_snapshot_basic.py::test_cmp_full_and_first_diff_mem`): resume, run a
   workload, pause, `snapshot/create`, `copy(diff)` → `M_diff(t0,t1)`; still
   paused, `copy(full)` → `M_full(t1)`. Assert
   `rebase(M_full(t0), M_diff) == M_full(t1)`. Any dirty page missed by the
   bitmap computation shows up as a mismatch. (`test_diff_self_consistency`)
1. Chains: several diffs rebased onto the base equal a final full copy, and the
   rebased result restores with a healthy guest. (`test_diff_chain_restores`)
1. Pre-copy: while running a workload, have the handler issue several
   `DirtyPages` requests and copy each set; pause; `snapshot/create`;
   `copy(diff)`. The result must equal a `copy(full)` taken right after.
   Exercises the `prepare_dirty_tracking_reset` hook under ssh traffic, and the
   handler serving faults while a request is outstanding.
   (`test_precopy_dirty_pages`, `test_dirty_pages_are_consumed`: two
   back-to-back requests on a paused guest, the second reports `set_pages` equal
   to the number of virtqueue pages only.)
1. Ordering rule 3 (§4), new: pause, `copy(diff)` *before* `snapshot/create`
   while a block read is in flight, then `snapshot/create`, then `copy(full)`;
   the rebase of the early diff onto the base must differ from the full at the
   block buffer pages, and a second `copy(diff)` after `snapshot/create` must
   close the gap. (`test_request_before_create_is_not_final`)
1. Pause invariant: pause a VM with a backend, take `M_full`, inject tap traffic
   (ssh connection attempts) for a few seconds, take another `M_full` while
   still paused. Assert the two copies are identical and the net device's RX
   metrics stayed at zero. (`test_pause_invariant`; reading the RX used index
   from the memfd, as first planned, is not needed: comparing whole copies is
   stronger.)
1. Cross-check against a Firecracker-made snapshot: restore one VM with a
   backend and one without from the same base snapshot, keep both paused, take a
   backend `Full` and a classic `Full`, compare. This exercises the
   snapshot-file source for never-populated pages end to end. Correction: the
   two are *not* byte-identical, because KVM writes the kvmclock pages (wall
   clock, per-vCPU time info) with the current time when their MSRs are set at
   restore. The test computes the pages at which the classic copy differs from
   the base (a handful) and requires every difference of the backend copy, to
   the base and to the classic copy, to be within that set.
   (`test_cross_check_with_firecracker_snapshot`)
1. Balloon: (a) boot with a backend, `M_full(t0)`, inflate while running,
   `Diff`, `Full`, `rebase == Full`, which needs the discarded ranges both
   punched (zeros in `Full`) and marked dirty (present in `Diff`); (b) same
   after a restore with a backend, where the handler receives `remove` events
   for a mix of populated and never-populated pages, then deflate, reuse the
   memory and restore from the result; (c) Firecracker's RSS drops on inflate
   with memfd-backed memory. (`test_balloon_inflate_at_boot`,
   `test_balloon_inflate_after_restore`, `test_balloon_inflate_reclaims_memory`;
   the plain-memory counterpart of (a) is
   `test_balloon.py::test_balloon_inflate_marks_pages_dirty`.)
1. virtio-mem: `total_size` includes the hotplug region, `unplugged` covers the
   unplugged slots and they are zero in the file, before and after plugging; the
   result restores with the plugged size intact.
   (`test_virtio_mem_unplugged_slots`)
1. Variants: hugetlbfs 2M for 1–4, the restore-with-backend model change and the
   balloon-after-restore test (`PAGE_CONFIGS`; restores of 2M snapshots go
   through the UFFD handler since `File` rejects them);
   `track_dirty_pages=false` (`mincore` diff of a booted VM rebases onto the
   base correctly; it cannot record discarded pages, §9); x86 with >4 GiB, whose
   two DRAM regions are contiguous in the bitmap (`test_two_dram_regions`); an 8
   GiB guest whose 256 KiB reply does not fit the default 208 KiB socket buffer,
   so that the blocking write path is exercised under seccomp
   (`test_backend_protocol_errors[8192]`). Not yet run: aarch64.

Compatibility tests (all in the same file unless noted):

- The UFFD protocol is unaffected: existing `test_uffd.py` runs unchanged with
  the existing handlers. New: a `Uffd` handler that writes a `DirtyPages` frame
  on its connection gets no answer and the microVM is unaffected (Firecracker
  does not read that connection).
- Memory backend at boot (no uffd) and at restore (uffd) with the same backend
  binary, for both `on_demand` and `fault_all`; a backend-produced snapshot
  restores with `Uffd`, `File` and `SharedMemfd`.
- Model change across restore: boot with backend → snapshot → restore with
  `Uffd` and with `File`; restore with `Uffd`/`SharedMemfd` → snapshot → restore
  with `SharedMemfd`; in each case the resulting VM snapshots correctly in its
  new mode.
- Negative: `snapshot/create` with `mem_file_path` and a backend → 400; without
  `mem_file_path` and no backend → 400; `PUT /snapshot/dirty-pages` → 404 (the
  endpoint is gone); `machine-config.mem_backend` with `File`/`Uffd` → 400;
  `machine-config.mem_backend` with `--no-api` fails the boot; unreachable
  backend fails `InstanceStart`; backend process killed → VM keeps running,
  `snapshot/create` still answers 204, HTTP API still responsive; a malformed
  frame from the backend (oversize `json_len`, non-zero `blob_len`, non-JSON) →
  connection closed by Firecracker, VM and HTTP API unaffected,
  `mem_backend.protocol_errors` incremented; an unknown request → error reply,
  connection still answers a subsequent `DirtyPages`.

### 11. Security and operational notes

- The backend receives a read/write fd to all guest memory; it must run in the
  same jail/user as a UFFD handler does today. Socket paths are relative to the
  jailer chroot.
- *(Revised.)* Firecracker now parses input from the backend, on the API thread.
  The input is a fixed 8-byte header and a JSON object of at most 64 KiB
  deserialised with serde into a closed enum (`deny_unknown_fields`); the blob
  length must be zero. Nothing is allocated before the header has been
  validated. The peer is trusted anyway (it holds guest memory), so the aim is
  robustness against a buggy backend, not defence against a hostile one; but the
  parsing surface is as small as it can be made.
- Seccomp: the VMM-thread filter already allows `connect` and `sendmsg` for the
  handshake; the API-thread filter already allows `read`, `write`, `recvfrom`,
  `sendto` and `epoll_ctl`, and gains `poll`/`ppoll` for waiting on a full
  socket buffer while writing a reply. `copy_file_range` is only used by the
  backend.
- The memfd is sealed against resize; Firecracker keeps its own fd, so backend
  death never invalidates guest memory.
- Reply size: 32 KiB per GiB of guest memory, independent of the dirty set (§8),
  as raw bytes on a local socket. The API server's payload limit is not
  involved. A backend that stops reading stalls the API thread's blocking write
  (§8, errors); the orchestrator can always kill the backend to unstall it.
- Seccomp for discards: the VMM thread's `madvise` rule is unrestricted, so
  `MADV_REMOVE` needs no filter change.
- Metrics:
  `mem_backend.{handshake_fails, dirty_pages_requests, dirty_pages_fails, protocol_errors}`
  and the existing snapshot latency metrics. Not implemented yet (§12).

### 12. Work breakdown

Implemented as revision 1 (one branch, `snapshot-improvements`; a mergeable
split is given in parentheses). Items marked *(rev. 2)* change with this
revision; §15 has the details.

1. Single memfd for DRAM + hotplug: `MemfdBacking` in `vstate/memory.rs`,
   `memory::create` with a base offset, `VmResources::allocate_guest_memory`
   returning the backing for `allocate_memory_region` to continue from;
   offset-invariant unit test. (Mergeable alone, no API change.)
1. Bitmap computation shared with file writing:
   `GuestMemorySlot::for_each_dirty_batch` is the single definition of "which
   pages a diff contains", used by both `dump_dirty` and
   `GuestMemoryExtension::dirty_layout`; `SlotFileOffsets`; property-style
   identity tests against `dump`/`dump_dirty` over random bitmaps, including
   fold-back on error; `SnapshotMemoryLayout::{set_range, page_is_set}`.
   (Mergeable alone.) *(rev. 2: `full_layout` and the base64 serde adapter go;
   `pages` becomes a plain `Vec<u8>` that is never serialised as JSON.)*
1. Handshake and boot: `send_uffd_handshake(&[RawFd])`, `uffd_mappings`,
   `MachineConfig.mem_backend` (only `SharedMemfd` accepted), handshake in
   `build_microvm_for_boot` after all regions are registered with KVM,
   `Vmm.mem_backend_attached`. *(rev. 2: `send_uffd_handshake` returns the
   stream for `SharedMemfd`; `Vmm::mem_backend: Option<UnixStream>` replaces the
   bool; `--no-api` rejects `mem_backend`.)*
1. `PUT /snapshot/create` with a backend: `mem_file_path: Option`, both
   rejections. *(rev. 2: no memory layout, no `VmmData::SnapshotMemory` on this
   path, 204; `KvmVm::snapshot_memory_layout` loses its `SnapshotType`
   parameter.)*
1. Dirty pages: `VmmAction::GetDirtyPages`, `Vmm::dirty_pages`,
   `VirtioDevice::prepare_dirty_tracking_reset` with the virtio-net
   implementation (`return_parsed_rx_buffers`, shared with `prepare_save`).
   *(rev. 2: the HTTP front end `PUT /snapshot/dirty-pages` is removed and
   replaced by the backend-connection front end in the API thread: frame codec,
   `ApiServer` owning the stream, `micro_http` external-fd support or nested
   epoll, handover channel from `run_with_api`.)*
1. `snapshot/load` with `backend_type: SharedMemfd`: memfd-backed memory, uffd
   registered on the shmem mapping, handshake with `[uffd, memfd]`.
1. Discards on shared memory: `discard_range` with `MADV_REMOVE` for shared file
   mappings, dirty marking of every discarded range, unit tests for the hole and
   the dirty bits. (Mergeable alone; also fixes balloon reclaim for vhost-user.)
1. Example handlers: `Handshake` with all fds, `fstat`-based fd classification,
   optional memory file, `PopulatedPages`, `MemorySource`/`copy_pages` walking
   the bitmap in runs with snapshot-file fallback, control socket, unit tests
   for each. *(rev. 2: frame codec and `request_dirty_pages` driven from the
   poll loop; `ControlRequest::Copy { mem_path, full }`; `Done.set_pages`;
   base64 decoder removed.)*
1. Python framework and integration tests as listed in §10. *(rev. 2: see §10
   for the framework changes and the new tests.)*
1. Docs: `shared-memfd.md` (user), this document, `snapshot-support.md`,
   `handling-page-faults-on-snapshot-resume.md`, `ballooning.md`, CHANGELOG;
   `INVARIANT` comment in `ApiServerAdapter::handle_request`. *(rev. 2: the user
   doc's API section becomes a protocol section; swagger loses
   `/snapshot/dirty-pages` and `SnapshotMemory`.)*

Remaining before this leaves developer preview:

1. Revision 2 itself (§15).
1. Metrics (`mem_backend.*`, §11).
1. Test variants not yet run: aarch64.
1. A decision on whether Firecracker's own `Diff` should keep writing zero pages
   for discarded ranges on anonymous memory (introduced by item 7; correct, but
   larger diff files after an inflate).
1. Hugetlbfs and the balloon reclaim little with plain inflation, since the
   driver releases 4 KiB pages and `MADV_REMOVE` only frees huge pages the range
   covers entirely (as before). Free page reporting, which reports whole page
   blocks, is the way to reclaim on hugetlbfs; untested with a backend.

### 13. Decisions taken

- The backend is trusted; connection failure at boot/restore is fatal.
  Firecracker does not monitor the backend: it notices the connection closing
  only because it is polling it for requests, and does nothing about it beyond
  logging.
- One handshake, the existing UFFD one, unchanged; a memory backend differs from
  a UFFD handler only by the fds it receives (uffd first, memfd last). No
  version field; extensions are additive fields and appended fds. Nothing about
  the choice is persisted in the snapshot.
- No synchronisation between Firecracker and the backend at snapshot time. The
  pause invariant of the `firecracker` binary is what makes this correct, and it
  is documented and tested as such.
- *(Revised.)* Dirty information is exchanged on the handshake connection, for
  `SharedMemfd` only, as one request (`{"request": "DirtyPages"}`) and one reply
  (a JSON header with `total_size`, `page_size`, `unplugged`, and the bitmap as
  a raw binary blob), framed as `u32 json_len, u32 blob_len, JSON, blob`. The
  handshake message itself is not framed and not changed. The request is
  consuming and legal in any state; the reply is the same whatever the backend
  intends to write, there is no "full" variant. The bitmap is one bit per
  `page_size` bytes over the file up to the last plugged slot, a superset of the
  modified pages, not promised to be exact. A range list was the first choice
  and was replaced after measuring it (§8); base64 over HTTP was the second and
  was replaced by raw bytes on the socket.
- *(Revised.)* The connection is serviced by the API thread, which turns a
  request into `VmmAction::GetDirtyPages` on the existing channel. The VMM
  thread never reads the socket. `--no-api` rejects a memory backend.
- *(Revised.)* `PUT /snapshot/create` with a backend writes vmstate only, takes
  no `mem_file_path`, ignores `snapshot_type`, touches no dirty state and
  answers `204` like every other `snapshot/create`. The final dirty set is the
  reply to a request the backend issues after `snapshot/create` has returned and
  before `Resume`.
- *(Revised.)* Pull, not push: Firecracker never writes to the connection
  unsolicited. The framing leaves room for notifications later.
- *(Revised.)* No HTTP front end for dirty pages. Without a memory backend there
  is no consumer for a consumed dirty set; a non-consuming variant is possible
  later if a use case appears, on either transport.
- A lost reply (connection died mid-write) is the backend's problem; it falls
  back to a full copy.
- Restore with a backend always uses a uffd for population; file population with
  sharing is deferred.
- Firecracker does not tell the backend where the bytes of a page come from.
  After a restore a page is either in the memfd (populated by the backend,
  possibly written since) or still only in the backend's source (registered,
  never populated), and only the backend knows which, from state it already
  owns: the pages it populated and the `remove` events it received. Two
  alternatives were rejected: populating every page of the bitmap before
  answering (`MADV_POPULATE_READ` or reading through the mapping, as `dump`
  does) makes a backend-made `Full` after a lazy restore fault in all of guest
  memory, which is the cost this design exists to avoid; and Firecracker
  classifying pages itself (memfd presence via `SEEK_HOLE`/`mincore` plus a
  bitmap of the ranges it discarded, reported as a second `source` bitmap) is
  possible additively but needs two presence mechanisms (tmpfs vs hugetlbfs) and
  is unreliable under host swap with `mincore`.
- Balloon/virtio-mem discards punch holes into the memfd (`MADV_REMOVE`, so the
  uffd `remove` event is preserved) and are marked dirty.
- The peer cannot rely on finding those holes in the memfd itself. On tmpfs
  `lseek(SEEK_DATA/SEEK_HOLE)` reports them (`shmem_file_llseek`). On hugetlbfs
  nothing does: `lseek` is `default_llseek` and reports the whole file as data,
  punched pages included; `mincore` on a mapping of the memfd reports the
  *caller's* huge PTEs (`mincore_hugetlb`), not the file's contents, so a peer
  that has not touched a page sees it as absent whether it is a hole or not;
  `cachestat` returns `EOPNOTSUPP` for hugetlbfs; `FIEMAP` is not implemented.
  (An earlier version of this document claimed `mincore` worked on both; it does
  not on hugetlbfs.) This is also why `unplugged` stays an explicit list rather
  than being folded into the bitmap and inferred from holes: a hotplug region
  can be gigabytes of never-plugged memory, and on hugetlbfs the peer would have
  to read all of it as zeros. The same applies to balloon-discarded pages, which
  are marked dirty and read back as zeros: a hugetlbfs peer cannot tell them
  from data without Firecracker's help (an additive "known zero" bitmap in the
  reply) or, after a restore, its own uffd `remove` events. Open item.

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
  had to learn that. (Moot again in revision 2.)
- The pause-invariant test compares two full copies taken around injected
  traffic rather than reading virtqueue state out of the memfd, which is both
  simpler and stronger (§10).
- The bitmap and uffd `remove` events are 4 KiB granular regardless of the
  backing page size, so a handler on hugetlbfs must make its source decision per
  backing page and round `remove` ranges inward. The 2M balloon test caught the
  example handler stepping `page_size` from an unaligned range start and missing
  a source change at the next huge-page boundary.
- The first version of this document exchanged dirty information as a list of
  page-aligned ranges. Measuring real dirty sets showed the list to be tiny when
  the guest allocates fresh memory and to explode when a process rewrites memory
  it already owns, which is the pre-copy steady state; it was replaced by a
  fixed-size bitmap (§8).
- Revision 1 exchanged that bitmap over the HTTP API, base64 in JSON, with the
  orchestrator relaying it to the backend. Writing the reference orchestrator
  (the test framework) and the handler's control protocol showed the relay to be
  pure plumbing: the orchestrator never looked at the bitmap, every integration
  had to grow a second protocol to carry it, and the one bulk item of the whole
  design was being text-encoded to travel through a JSON API to a process that
  already held a socket to Firecracker. Revision 2 moves it there (§7, §8).

### 15. Revision 2: what changed, and what was weighed

This section is the delta between revision 1 and the design as written above, as
implemented. Everything not listed is unchanged.

#### Firecracker

- `persist::send_uffd_handshake` returns `Option<UnixStream>`: `Some` for
  `SharedMemfd`, `None` (after `forget`) for `Uffd`.
  `Vmm::mem_backend_attached: bool` becomes
  `Vmm::mem_backend: Option<UnixStream>` (attached ⇔ `Some`, until the stream is
  taken by `run_with_api`; a separate bool or the `Option`'s history keeps
  `create_snapshot`'s check working after the take).
- `persist::create_snapshot`: with a backend, return after writing vmstate; drop
  the `Option<SnapshotMemoryLayout>` return and `VmmData::SnapshotMemory` on the
  `CreateSnapshot` path; `PUT /snapshot/create` is `204` again in
  `parsed_request` and swagger. `KvmVm::snapshot_memory_layout(SnapshotType)`
  becomes `dirty_memory_layout()`; `GuestMemoryExtension::full_layout` is
  removed (it was only used for `Full` replies).
- `SnapshotMemoryLayout`: `pages: Vec<u8>` (not `Option`), `#[serde(skip)]`, no
  base64 adapter; `SnapshotMemoryLayout::new` replaces `::diff`/`::full`, and
  `page_is_plugged`/`plugged_pages` describe the full copy;
  `SnapshotMemoryResponse` (which added `snapshot_type`) is removed. The JSON
  header of the reply is the struct minus `pages`.
- `api_server/request/snapshot.rs`: `parse_put_snapshot_dirty_pages` and the
  `"dirty-pages"` route are removed; `VmmAction::GetDirtyPages` stays and is
  only produced by the new front end.
- New module, `api_server/mem_backend.rs`: frame codec (`Frame`, `Decoder`,
  limits), `MemBackendConnection` (non-blocking read into the decoder, dispatch
  of `{"request": "DirtyPages"}` to `serve_vmm_action_request`, blocking
  `write_all` of the reply, error replies, close on protocol error, metrics).
  `ApiServer` gains `mem_backend: Option<MemBackendConnection>` and an
  `mpsc::Receiver<UnixStream>` plus eventfd for the handover; `ApiServer::run`
  dispatches on which fd woke the loop. `HttpServer`: either an
  `add_external_fd` /`ServerError::External(fd)` extension in `micro_http`, or
  an outer epoll in `ApiServer::run` around `server.epoll().as_raw_fd()`, the
  handover eventfd and the connection fd.
- `api_server_adapter::run_with_api`: after `build_result` and before the VMM
  seccomp filter is applied, take the stream from the `Vmm`, make it
  non-blocking, send it to the API thread and kick the eventfd.
- `main.rs` / `run_without_api`: reject `machine-config.mem_backend`
  (`RunWithoutApiError::MemBackendRequiresApi`) before building anything.
- Seccomp: `poll` (x86_64) / `ppoll` (aarch64) is added to the API thread's
  filter for the case in which a reply does not fit the socket buffer and the
  write has to wait for the peer to read. Everything else was already allowed:
  the reply is written with `write`/`sendto`, the request read with
  `read`/`recvfrom`, the outer epoll is created before the filter is applied and
  `epoll_ctl`/`epoll_pwait` are in the filter.

#### Example handlers (`src/firecracker/examples/uffd/uffd_utils.rs`)

- Frame codec (encode a request, decode a reply incrementally); `base64_decode`
  and `base64_encode` removed; the local `SnapshotMemoryLayout` mirror loses
  `pages` as a JSON field and gains it from the blob.
- `Runtime`: keep the handshake stream as the request channel; `POLLIN` on it
  after the handshake feeds the reply decoder instead of expecting another
  handshake; `HUP` still ends polling of it. A pending `Copy` control request is
  completed when the reply frame is complete, so faults are served throughout.
- `ControlRequest::Copy { mem_path, full }`,
  `ControlResponse::Done { success, message, set_pages }`.

#### Tests

- Unit: codec round trips and limit rejections; connection handling over a
  `UnixStream::pair` (request → `GetDirtyPages`, reply frame, error reply,
  unknown request, oversize header, blob in a request, hangup); the handler's
  decoder fed one byte at a time and its control socket driven with a fake
  Firecracker; `send_uffd_handshake` keeping or dropping the stream.
- Integration: §10 as revised. `Microvm.dirty_pages` is replaced by
  `Microvm.backend_copy(mem_path, full)` / `precopy_pass`; `make_snapshot` with
  a backend calls `snapshot/create` then `backend_copy`;
  `Microvm.last_backend_copy` carries the handler's `Done` reply (`set_pages`);
  `http_api.request` no longer accepts `200` for `PUT`; `test_memory_backend.py`
  stops importing `base64`. New tests:
  `test_request_before_create_is_not_final`, `test_backend_protocol_errors` (the
  test plays the backend on a raw socket: unknown request → error reply,
  `DirtyPages` → 8 KiB raw bitmap for 256 MiB, blob in a request → connection
  closed, VM and HTTP API unaffected), `test_mem_backend_requires_api`;
  `test_negative_api` checks `204` and no memory file with a dead backend and
  that `/snapshot/dirty-pages` is gone; the 8 GiB variant of
  `test_backend_protocol_errors` produces a 256 KiB reply that exceeds the
  default socket buffer, and was used to show that without `poll` in the API
  filter the write kills Firecracker (bad syscall 7). 60 tests pass on x86_64
  (4K and 2M pages).

#### Documentation

- `shared-memfd.md`: "After the handshake" and the whole "API" section are
  rewritten around the frame protocol (request, reply, header fields, bitmap
  layout as raw bytes, worked example with the frame bytes); the pre-copy and
  snapshot procedures gain the "ask again after `snapshot/create`" step and the
  "keep serving faults while waiting" rule; "Consistency" carries the three
  ordering rules of §4; `PUT /snapshot/create` is documented as vmstate-only,
  `204`.
- `snapshot-support.md` and `handling-page-faults-on-snapshot-resume.md`: the
  sentence that says Firecracker never reads the socket is qualified with
  "`Uffd`"; the `SharedMemfd` pointer stays.
- Swagger: remove `/snapshot/dirty-pages` and the `SnapshotMemory` definition;
  `snapshot/create` responses back to `204` only; `mem_file_path` stays optional
  with its description.
- CHANGELOG: the developer-preview entry describes the socket protocol instead
  of the two HTTP responses.

#### Alternatives weighed for this revision

- **Keep HTTP, add `Accept: application/octet-stream`.** Solves the base64 cost
  and nothing else: the orchestrator still relays, `snapshot/create` still has
  two response shapes, the backend still needs a second protocol. Rejected.
- **Let the backend be an HTTP client of the API socket.** No new transport, and
  the API thread already serves it. Rejected because it hands the backend the
  whole API (reconfigure the VM, pause it, shut it down) when it needs one
  capability, and because the API socket lives where the orchestrator put it,
  not necessarily where the backend can reach it in its jail. The handshake
  connection is a capability Firecracker itself extended to exactly this peer.
- **Push the final set from `snapshot/create`.** See §8, "pull, not push".
- **Service the socket on the VMM thread.** See §8, "serviced by the API
  thread". The paused loop is the reason.
- **Keep `PUT /snapshot/dirty-pages` as a second front end.** Two ways to
  consume the same state, one of which (HTTP, without a backend) has no
  consumer. Rejected; a non-consuming peek can come back on either transport if
  needed.
- **Frame the handshake too.** Would break every existing UFFD handler for
  uniformity's sake. The handshake stays a single unframed `recvmsg`; only what
  follows it is framed, and only `SharedMemfd` peers ever see it.
- **A binary header instead of JSON in the frame.** `unplugged` is variable
  length and the handshake is already JSON; the only bulk item is the bitmap and
  it is already raw. A JSON header costs a few hundred bytes and keeps
  extensions additive.
- **Reconnection.** A backend that loses the connection could reconnect to a
  path Firecracker listens on. Firecracker has no listener today and adding one
  is a much larger change (who may connect, what is re-sent). Deferred; a
  backend that loses its connection is in the same position as a UFFD handler
  that loses its uffd.

#### Settled during implementation

- The connection is made non-blocking on the VMM thread before the handover (the
  VMM filter is not applied yet at that point) so that the API thread never
  needs `ioctl(FIONBIO)` on it.
- An unknown request or unknown fields get an error reply and the connection
  stays open; only framing violations close it. `"not json"` inside a valid
  frame is therefore an error reply, not a close: the stream position is known.
- The backend-side decoder accepts a non-zero `blob_len` in replies only;
  Firecracker's decoder rejects any blob in a request before reading it.
- `snapshot_type` on `PUT /snapshot/create` with a backend is accepted and
  ignored (`Full` and `Diff` both produce a `204` and the same vmstate).
- Two integration assertions from revision 1 that inspected the bitmap (bitmap
  length equals the plugged part, no bits inside `unplugged`) moved to Rust unit
  tests, since the framework no longer sees the bitmap; the Python tests assert
  on `set_pages` counts and on the files.
