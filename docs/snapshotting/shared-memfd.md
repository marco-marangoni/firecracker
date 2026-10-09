# Shared memfd: guest memory in the hands of a page fault handler

> [!WARNING]
>
> The `SharedMemfd` memory backend is in developer preview. The API described
> here may change in incompatible ways before it is stabilised.

`SharedMemfd` is an advanced feature for users looking for a custom or
high-performance snapshot solution.

Some optimizations unlocked by `SharedMemfd` are, in order of gain:

1. VM post-copy: a VM can be resumed while the snapshot is being downloaded. The
   memory can be faulted in on-demand or pre-fetched according to the
   userfaultfd protocol
1. VM pre-copy: the guest memory can be copied in batches while the VM is still
   running
1. Resuming or creating a snapshot can be implemented without using the disk as
   an intermediary. All the guest memory is either copied via userfaultfd, or
   read/written on the shared `memfd`
1. For a live migration, `/snapshot/prepare` allows pre-initializing the
   destination VM while the source VM is still running

With these optimizations, the guest pause for a snapshot+restore drops by up to
three orders of magnitude.\
For illustrative purposes, on hugetlbfs, guest downtime goes from ~450 ms per
GiB of resident memory to ~10 ms regardless of VM size: no memory is copied
during the pause, and all expensive operations are done either before a pause or
after a resume.

## What it is

`SharedMemfd` is a `backend_type` for `mem_backend`, next to `File` and `Uffd`.
It is similar to the `Uffd` backend with two important distinctions:

- Firecracker allocates guest memory as a single `memfd` and hands it to the
  backend via Unix domain socket as part of the handshake
- it can also be used at boot, in which case there's no UFFD

When `SharedMemfd` is used, the backend can read the guest memory during
snapshot, without any disk I/O.

When used on VM restore, the `SharedMemfd` backend is a
[UFFD page fault handler](handling-page-faults-on-snapshot-resume.md) with extra
duties: everything that document says about serving faults, including the
handling of balloon `remove` events, applies unchanged, and on top of it the
backend produces the memory part of snapshots. Nothing about snapshots adds to
the fault-handling obligations: Firecracker tracks what it discards and reports
it as zero pages, so the backend needs no page-level bookkeeping of its own.

Read carefully the "Consistency" and "Limitations" sections below.

## The handshake

Firecracker hands guest memory to the backend with one message on a Unix domain
socket. The message is sent once, after guest memory is set up and before any
vCPU runs. It is the same message Firecracker sends to a page fault handler when
restoring with `backend_type: Uffd`; a backend just receives one more file
descriptor with it.

### The socket

The backend creates and listens on a `SOCK_STREAM` Unix domain socket which is
then passed to Firecracker (via the config file, `PUT /machine-config`, or
`PUT /snapshot/load`). Firecracker then connects to the path given in the
request (relative to its chroot when jailed) and sends the handshake message. If
nobody is listening, the API call fails.

After that, Firecracker never reads from or writes to the socket, and keeps it
open until it exits. The backend can use the connection to learn Firecracker's
PID with `SO_PEERCRED`, and to notice that Firecracker has exited when the
connection closes. Anything the backend sends on it is ignored.

### The message

A single `sendmsg` with a JSON array as payload and the file descriptors
attached as `SCM_RIGHTS` data. Read it with one `recvmsg`. There is no framing;
the payload is a few hundred bytes and there are at most two descriptors. Do not
wait for end of file: it only comes when Firecracker exits.

The array has one entry per guest memory region, in guest address order:

```json
[
  {
    "base_host_virt_addr": 140160296779776,
    "size": 3221225472,
    "offset": 0,
    "page_size": 4096,
    "page_size_kib": 4096
  },
  {
    "base_host_virt_addr": 140163518005248,
    "size": 1073741824,
    "offset": 3221225472,
    "page_size": 4096,
    "page_size_kib": 4096
  }
]
```

| Field                 | Meaning                                                                                                               |
| :-------------------- | :-------------------------------------------------------------------------------------------------------------------- |
| `base_host_virt_addr` | Address of the region in Firecracker's address space. Page fault events use it; the memfd and the API do not.         |
| `size`                | Region size in bytes.                                                                                                 |
| `offset`              | Offset of the region in the memfd. It is also its offset in a memory snapshot file and in the bitmap the API returns. |
| `page_size`           | Page size in bytes: 4096, or 2097152 (2 MiB) with hugetlbfs.                                                          |
| `page_size_kib`       | Deprecated copy of `page_size`. The value is in bytes despite the name. Will be removed in 2.0.                       |

The regions are the guest's physical memory regions (one region, or two on x86
when memory extends past the 32-bit MMIO gap) and the virtio-mem hotplug region
if there is one, plugged or not. They are contiguous in the memfd, and their
sizes add up to the size of the memfd.

The format of the handshake is the same as the `Uffd` backend.

### The file descriptors

| How the microVM was started                      | fds received    |
| :----------------------------------------------- | :-------------- |
| `snapshot/load` with `backend_type: File`        | N/A, no socket  |
| `snapshot/load` with `backend_type: Uffd`        | `[uffd]`        |
| `snapshot/load` with `backend_type: SharedMemfd` | `[uffd, memfd]` |
| At boot with `machine-config.mem_backend`        | `[memfd]`       |
| At boot without `machine-config.mem_backend`     | N/A, no socket  |

The userfaultfd comes first and the memfd last. The order of the fds is part of
the API contract. A properly architected backend should be aware of
Firecracker's lifecycle and can always assert the exact fds it expects.

The memfd is all the guest memory, laid out like a full memory snapshot file:
region *n* starts at its `offset`. Firecracker maps it `MAP_SHARED`, so reading
it (with `pread`, `copy_file_range`, or a mapping of your own) gives the same
bytes the guest sees, and writing it changes what the guest sees. It is sealed
against resizing but not against writing or punching holes; Firecracker itself
punches holes when the guest releases memory (see below). It is shmem, or
hugetlbfs when the microVM uses 2M pages. Firecracker keeps its own descriptor,
so a backend exiting does not invalidate guest memory. (A backend that was
serving page faults and exits does leave the guest stuck on the next fault, but
that is the userfaultfd, not the memfd.)

The userfaultfd, when present, is registered on every region in missing-page
mode with `remove` events enabled. How to serve it is described in
[the page fault handler document](handling-page-faults-on-snapshot-resume.md).
The regions it reports are the same as in the message.

### After the handshake

On a fresh boot nothing else happens: the memfd starts empty and the guest fills
it. On a restore, the backend serves page faults from the snapshot file, and the
memfd fills with what it populates and what the guest writes.

From here on, Firecracker's only involvement is the API. `PUT /snapshot/create`
with `snapshot_type: Backend`, and `PUT /snapshot/dirty-pages`, return two
bitmaps, which pages of the memfd the backend should copy and the pages it
should discard. There is no other protocol between Firecracker and the backend.

Sharing memory this way has a cost: guest memory is a `MAP_SHARED` mapping; page
faults on it are somewhat slower than on anonymous memory. Hugetlbfs mitigates
the performance loss very effectively. Transparent huge pages depend on the
host's `shmem_enabled` setting, and only partially mitigate the performance
loss.

## API

### Attaching a backend at boot: `PUT /machine-config`

```json
{
  "vcpu_count": 2,
  "mem_size_mib": 1024,
  "track_dirty_pages": true,
  "mem_backend": {
    "backend_type": "SharedMemfd",
    "backend_path": "/run/backend.sock"
  }
}
```

Only `SharedMemfd` is accepted here (`File` and `Uffd` describe how memory is
*populated* on restore and have no meaning at boot). The field is also accepted
in the JSON configuration file. When set, guest memory is one memfd and the
handshake carrying `[memfd]` is performed during `InstanceStart`, after all
memory regions have been mapped and before any vCPU runs. Failing to connect to
`backend_path` fails the boot.

### Attaching a backend at restore: `PUT /snapshot/load`

```json
{
  "snapshot_path": "/path/vmstate",
  "mem_backend": {
    "backend_type": "SharedMemfd",
    "backend_path": "/run/backend.sock"
  }
}
```

`SharedMemfd` behaves like `Uffd`: a uffd is created, registered on the
(memfd-backed) guest memory, and the handshake carries `[uffd, memfd]`. The
backend serves page faults exactly as a UFFD handler does.

The choice is per instance and is **not** recorded in the snapshot: a snapshot
made with a memory backend restores fine with `File` or `Uffd`, and vice versa.

### Setting up the microVM ahead of the restore: `PUT /snapshot/prepare`

```json
{
  "mem_backend": {
    "backend_type": "SharedMemfd",
    "backend_path": "/run/backend.sock"
  }
}
```

Most of what `PUT /snapshot/load` does depends only on the microVM's shape, not
on the snapshot: mapping guest memory, registering it with KVM, creating the VM
and its vCPUs, and setting up a backend if applicable. Since `/snapshot/load` is
in the hot path, `PUT /snapshot/prepare` lets you do that part ahead of time,
to prepare a destination VM while the source VM is still running. The resume
workflow is then something like:

```
PUT /machine-config   { ... }
PUT /memory-hotplug   (if the snapshot has a hotpluggable region)
PUT /snapshot/prepare {"mem_backend": {"backend_type": "SharedMemfd",
                                       "backend_path": "/run/backend.sock"}}

...

PUT /snapshot/load    {"snapshot_path": "/path/vmstate", "resume_vm": true}
```

During `/snapshot/prepare`, the backend receives the handshake and can populate
the memfd before the load.

The load then checks the snapshot against the prepared microVM, vCPU count,
memory size, page size, memory layout and KVM capability modifiers (from
`cpu_template`), and rejects a mismatch without touching anything;
`mem_backend`, `track_dirty_pages` and `huge_pages` are fixed by the preparation
and must be left out of the load request. The machine configuration cannot
change once a microVM is prepared, and a prepared microVM cannot be booted.

### `PUT /snapshot/create`

The microVM must be `Paused`, as for any snapshot. `PUT /snapshot/create` takes
a `snapshot_type`: `Full`, `Diff`, or `Backend`.

`Full` and `Diff` behave exactly as without a memory backend: `mem_file_path` is
mandatory and Firecracker writes the whole of guest memory to it through its own
mapping, answering `204 No Content`.

`Backend` is the mode where the backend produces guest memory instead of
Firecracker: `mem_file_path` must be **absent** (400 otherwise), no guest memory
is written, and the response is `200 OK` with a `memory` object (see "The memory
layout" below) telling the backend which pages of the memfd make up the
snapshot. Firecracker still writes the microVM state to `snapshot_path`. It is
rejected with 400 when no memory backend is attached. After the call the microVM
must not be resumed until the backend has copied every `pages_to_copy` page out
of the memfd.

When the backend follows the rules explained in the "Consistency" section below,
the resulting snapshots are identical to the ones produced using the `Diff` or
`Full` snapshot types, without any disk I/O for the guest memory.

### `PUT /snapshot/dirty-pages`: optional pre-copy

Pre-copy is optional and only useful if you want to shorten the pause of a
`/snapshot/create Backend`. While the guest runs, `dirty-pages` returns the
pages that changed since the last snapshot or the last call (the `memory` object
described in the next section) and consumes the tracking state, so the backend
can copy them in the background. Repeating it tracks an ever-smaller working
set; a final `/snapshot/create Backend` (while paused) then copies only what
changed since the last pass. The request has no parameters (empty body or `{}`),
returns `200 OK` with the `memory` object, and leaves the microVM running. It is
rejected with 400 when no memory backend is attached, because then nobody but
Firecracker could turn the consumed sets into bytes.

In case of a running guest, the returned pages might be modified after the call
returns. In that case, it is guaranteed they will be returned on the next call
(a later `dirty-pages`, or the final `/snapshot/create Backend`).

### The memory layout

`PUT /snapshot/dirty-pages` and `PUT /snapshot/create` with `Backend` return a
`memory` object classifying every page of guest memory. For example:

```json
{
  "memory": {
    "page_size": 4096,
    "bitmap_encoding": "roaring",
    "pages_to_copy": "OzAAAAEAAB8ACAAAAAEACQADACgAAAA6AAkAZAAAAIIAAQCdAAAAqgAKAA==",
    "pages_to_discard": "OzAAAAEAAD8AAQDAAD8A"
  }
}
```

Every page of the memory file is in exactly one of three classes:

- **to copy** (`pages_to_copy`): its content is in the memfd; copy it from there
  into the file at the same offset.
- **to discard** (`pages_to_discard`): the guest discarded it (balloon, free
  page reporting, virtio-mem unplug) and now reads it as zero. The recommended
  action is to make it read as zero in the file you produce — write zeros, punch
  a hole in a fresh full file, or skip a range your storage already knows to be
  zero — so the file matches guest memory exactly. You *may* instead leave
  whatever the page previously held: a well-behaved Linux guest discarded these
  pages and will zero-initialize them before reading on the next allocation, so
  the stale content is never observed by the guest.
- **unchanged** (in neither set): its content did not change since the dirty
  state was last consumed. It is not part of the diff; leave it out. Whatever
  the diff is applied to (the previous memory file, or a fresh zero-filled file
  for a microVM that was booted rather than restored) already holds it.

The two sets are given as page indices (page `i` is the `page_size` bytes at
file offset `i * page_size`); the full object is:

- `page_size` is the granularity of both sets, in bytes. It is the host page
  size (usually 4096), also when the guest memory is backed by 2 MiB hugetlbfs
  pages.
- `bitmap_encoding` names the serialization of the two sets. It is always
  `roaring` today. Check it and refuse anything else: the field exists so that,
  should a future Firecracker change the binary format, an old backend fails
  cleanly on the name rather than decode garbage.
- `pages_to_copy` and `pages_to_discard` are each the standard base64 (RFC 4648,
  with padding) of a [Roaring bitmap](https://roaringbitmap.org) in the
  [portable serialization format](https://github.com/RoaringBitmap/RoaringFormatSpec),
  32-bit members, each member a page index. The two sets are disjoint, and they
  span the whole memfd.

Roaring libraries exist for every mainstream language (CRoaring, `roaring` for
Rust and Go, RoaringBitmap for Java, `pyroaring`); deserialize the two strings
and you have sets with constant-time membership and ordered iteration. In the
example, `pages_to_copy` decodes to pages 0–1, 9–12, 40, 58–67, 100, 130–131,
157 and 170–180, and `pages_to_discard` to pages 192–255, the unplugged slot.

With dirty tracking on, the classification comes from KVM's dirty log,
Firecracker's bitmap and the record of discarded pages alone. Without dirty
tracking it relies on `mincore`, and **swap must be disabled** for the API to
return correct information.

A backend that stores memory in chunks (512 KiB, 2 MiB, ...) applies a response
chunk by chunk. When using 2M hugetlbfs, with chunks aligned to and no larger
than 2 MiB, a chunk with a page in either set can be read whole from the memfd.
This holds for chunks with pages in `pages_to_discard` too, but such a chunk
must not simply be zero-filled: a freed huge page the guest partially rewrote
has pages in `pages_to_copy` and pages in `pages_to_discard` in the same
response. For all other configurations, the changes need to be applied on top of
the previous version of the chunk.

Sizes: a set costs at most 43 KiB per GiB of guest memory, base64 included,
whatever the dirty pattern, so a response is at most 86 KiB per GiB. That is the
cost of a guest rewriting its memory at random; runs of consecutive pages, such
as a released balloon or a freshly written buffer, cost a few bytes each
regardless of their length, and an idle guest's response is a few KiB.

Each `/snapshot/dirty-pages` or `/snapshot/create` call consumes the dirty
tracking state: the pages returned are no longer considered dirty.

## Consistency

In order to produce a consistent snapshot, the backend needs to adhere to some
simple rules. These rules cannot be enforced by Firecracker.

- The `pages_to_copy` returned by `/snapshot/dirty-pages` and by a `Backend`
  `/snapshot/create` must be eventually copied into the snapshot, and the
  `pages_to_discard` should read as zero in it (or keep their previous content,
  if the file is only ever read back by the guest). Responses must be applied in
  the order they were received: a page discarded by one response and copied by
  the next would otherwise end up zeroed, with nothing left to correct it.
  Applying them late is fine; the memfd may hold a newer value by then, and the
  write that made it newer is in a later response
- If a response is lost (e.g. connection dropped before the body was read), that
  tracking information is gone and the next snapshot must be a `Full` through
  `/snapshot/create`, which Firecracker writes itself
- Starting a new lineage (a `Full`, or a fresh pre-copy chain) invalidates the
  responses received before it: responses from before must not be applied to it
  or to anything derived from it
- After the final `/snapshot/create`, the microVM must not be resumed until the
  backend has copied every page in `pages_to_copy` out of the memfd

### Preparing the destination

To resume from a snapshot as fast as possible, it's optionally possible to start
your backend and prepare Firecracker while the source VM is running using
`PUT /machine-config`, `PUT /memory-hotplug`, `PUT /cpu-config` and
`PUT /snapshot/prepare`. The backend can start pre-populating guest memory into
the memfd at this point.

These APIs do most of the initialization work, and leave Firecracker in a state
where `/snapshot/load` can resume a VM in a few milliseconds.

### Workflows

To produce a snapshot, with no pre-copy, the workflow is as follows:

1. (optional) prepare the destination VM
1. Pause the microVM
1. Call `/snapshot/create` with `snapshot_type: Backend`
1. Copy `pages_to_copy` into a file, zero `pages_to_discard`
1. The microVM can be resumed here, or the file restored on the destination VM

A backend that wants to shorten the pause can add pre-copy around that final
call, so that only the pages that changed since the last round are copied while
the guest is paused:

1. (optional) prepare the destination VM
1. Call `/snapshot/dirty-pages`
1. Copy `pages_to_copy` into a file, zero `pages_to_discard`
1. Repeat from step 2 until the changed set is small enough, or after a timeout
   or iteration limit
1. Pause the microVM
1. Call `/snapshot/create` with `snapshot_type: Backend`
1. Final copy of `pages_to_copy`, final zeroing of `pages_to_discard`
1. The microVM can be resumed here, or the file restored on the destination VM

Note: the pre-copy loop only makes progress with dirty tracking enabled. Without
it, with just `mincore`, the changed set does not shrink between calls.

For a VM migration, it's also possible to shorten the pause time by deferring
the final copy of `pages_to_copy` using a post-copy approach. The pause then no
longer depends on how much the guest writes, or the size of the VM:

1. Prepare the destination VM
1. Run the pre-copy loop as above, copying each response into the target
1. Pause the microVM
1. Call `/snapshot/create` with `snapshot_type: Backend` on the source. The
   response is the residual changes since the last round.
1. Hand the `pages_to_copy` and `pages_to_discard` bitmaps to the destination's
   backend, together with a way to reach the source's backend, which keeps the
   source memfd and serves pages out of it on request.
1. Call `PUT /snapshot/load` with the source VMM state file on the destination,
   resume it. The guest now runs.\
   A fault on a page of the residual's `pages_to_copy` is served by fetching it
   from the source backend; a page of its `pages_to_discard` is served as zero
   without a fetch; every other page comes from the target as before. In the
   background, while no fault is waiting, the backend pulls the remaining
   residual pages and writes them into the target, so that the target ends up a
   complete snapshot of the final state
1. Once nothing is pending, release the source: its microVM and backend can be
   killed. Until then the source microVM must stay paused and its memfd intact

## Example backend

The example backends in
[`src/firecracker/examples/uffd/`](../../src/firecracker/examples/uffd/)
implement examples of both `Uffd` and `SharedMemfd` backends.

They require a minimal external orchestration to call Firecracker APIs, and tell
the backend which pages to copy.

## Limitations

- Guest memory is a `MAP_SHARED` mapping, which results in slower page faults.
  Please benchmark your use-case before enabling this feature. With hugetlbfs,
  the regression effectively disappears.
- Restoring with a memory backend always populates memory through the uffd.
  Populating from a snapshot file while sharing memory (the `File` backend's
  behaviour) is not offered.
- Without dirty tracking the host must not swap: the classification then uses
  `mincore` to tell a punched page from one with content, and a swapped-out page
  looks like a punched one. (Firecracker's own `mincore` diffs have the same
  requirement.) With dirty tracking on, `mincore` is not used.
- Firecracker does not monitor the backend. Killing it leaves the microVM
  running, and, in case of a restore, with nobody to serve page faults.
- Firecracker cannot know nor control what the vhost-user server writes to guest
  memory. When using vhost-user, it's the backend's responsibility to coordinate
  with it and to exchange information about which pages it modified.
