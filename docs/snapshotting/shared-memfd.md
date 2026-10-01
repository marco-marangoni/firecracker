# Shared memfd: guest memory in the hands of a page fault handler

> [!WARNING]
>
> The `SharedMemfd` memory backend is in developer preview. The API described
> here may change in incompatible ways before it is stabilised.

## What it is

`SharedMemfd` is a `backend_type` for `mem_backend`, next to `File` and `Uffd`.
It is similar to the `Uffd` backend with two important distinctions:

- Firecracker allocates guest memory as a single `memfd` and hands it to the
  backend via Unix domain socket as part of the handshake
- it can also be used at boot, in which case there's no UFFD

When `SharedMemfd` is used, it is the responsibility of the backend to save the
guest memory during VM snapshot.

When used on VM restore, the `SharedMemfd` backend is a
[UFFD page fault handler](handling-page-faults-on-snapshot-resume.md) with extra
duties: everything that document says about serving faults, including the
handling of balloon `remove` events, applies unchanged, and on top of it the
backend produces the memory part of snapshots. Nothing about snapshots adds to
the fault-handling obligations: Firecracker tracks what it discards and reports
it as zero pages, so the backend needs no page-level bookkeeping of its own.

Some use-cases for `SharedMemfd` are:

- VM post-copy: a VM can be resumed while the snapshot is being downloaded. The
  memory can be faulted in on-demand or pre-fetched according to the userfaultfd
  protocol
- VM pre-copy: the guest memory can be copied in batches while the VM is still
  running
- Resuming or creating a snapshot can be implemented without using the disk as
  an intermediary. All the guest memory is either copied via userfaultfd, or
  read/written on the shared `memfd`

## The handshake

Firecracker hands guest memory to the backend with one message on a Unix domain
socket. The message is sent once, after guest memory is set up and before any
vCPU runs. It is the same message Firecracker sends to a page fault handler when
restoring with `backend_type: Uffd`; a backend just receives one more file
descriptor with it.

### The socket

The backend creates a `SOCK_STREAM` Unix domain socket and listens on it before
making the API call that attaches it (`PUT /machine-config` with `mem_backend`,
or `PUT /snapshot/load` with `backend_type: SharedMemfd`). Firecracker connects
to the path given in the request (relative to its chroot when jailed) and sends
the message. If nobody is listening, the API call fails.

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

The regions are the guest's DRAM (one region, or two on x86 when memory extends
past the 32-bit MMIO gap) and the virtio-mem hotplug region if there is one,
plugged or not. They are contiguous in the memfd, and their sizes add up to the
size of the memfd, which the API calls `total_size`.

The format of the handshake is the same as the `Uffd` backend.

### The file descriptors

| How the microVM was started                      | fds received    |
| :----------------------------------------------- | :-------------- |
| `snapshot/load` with `backend_type: Uffd`        | `[uffd]`        |
| `snapshot/load` with `backend_type: SharedMemfd` | `[uffd, memfd]` |
| At boot with `machine-config.mem_backend`        | `[memfd]`       |

The userfaultfd comes first and the memfd last. The order of the fds is part of
the API contract. A properly architected backend should be aware of
Firecracker's lifecycle and can always assert the exact FDs it expects. It's
also possible (but not recommended) to tell the fds apart using `fstat`: the
memfd is a regular file, the userfaultfd is an anonymous inode with no file
type.

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
and `PUT /snapshot/dirty-pages` return two bitmaps, the pages of the memfd to
copy and the pages to zero, and whoever made the call passes them to the backend
however it likes; the example handler uses a second Unix socket. There is no
other protocol between Firecracker and the backend.

Sharing memory this way has a cost: guest memory is a `MAP_SHARED` mapping of
shmem or hugetlbfs, page faults on it are somewhat slower than on anonymous
memory, and transparent huge pages depend on the host's `shmem_enabled` setting.
vhost-user devices have the same requirement; if both are configured they share
one memfd.

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

### `PUT /snapshot/create` with a backend attached

```json
{
  "snapshot_type": "Diff",
  "snapshot_path": "/path/vmstate"
}
```

The microVM must be `Paused`, as always. The rule is: **Firecracker writes
`Full` snapshots, the backend produces `Diff` snapshots.**

A `Full` snapshot behaves exactly as without a backend: `mem_file_path` is
mandatory, Firecracker writes the whole of guest memory to it through its own
mapping and answers `204 No Content`. Reading through the mapping faults in
every page the backend has not populated yet (the backend serves them from its
snapshot file, as for any other fault), so a `Full` after a lazy restore is
complete and self-contained but costs a full population of the memfd. Take it
when you need a file that does not depend on anything else, notably after losing
a `Diff` response (see below); otherwise prefer a `Diff` merged into the
previous file, which the backend produces without reading unchanged memory.

For a `Diff` snapshot with a backend attached, `mem_file_path` must be
**absent** (400 otherwise): Firecracker does not write guest memory, it tells
the backend what changed. The response is `200 OK` with a body describing the
memory part of the diff:

```json
{
  "snapshot_type": "Diff",
  "memory": {
    "total_size": 1048576,
    "page_size": 4096,
    "bitmap_encoding": "roaring",
    "memfd_authoritative_pages": "OzAAAAEAAB8ACAAAAAEACQADACgAAAA6AAkAZAAAAIIAAQCdAAAAqgAKAA==",
    "zero_pages": "OzAAAAEAAD8AAQDAAD8A"
  }
}
```

Every page of the memory file is in exactly one of three classes:

- **memfd authoritative**: its content is in the memfd; copy it from there into
  the file at the same offset.
- **zero**: it must read as zero in the file you produce. How you get there is
  up to you: write zeros, punch a hole in a fresh full file, skip a range your
  storage already knows to be zero.
- **unchanged** (neither bit set): its content did not change since the dirty
  state was last consumed. It is not part of the diff; leave it out. Whatever
  the diff is applied to (the previous memory file, or a fresh zero-filled file
  for a microVM that was booted rather than restored) already holds it.

Both classes are given as sets of page indices (page `i` is the `page_size`
bytes at file offset `i * page_size`), in the same encoding:

- `total_size` is the size of a full memory file (the sum of all region sizes,
  including the hotplug region, plugged or not). Create the target file at this
  size.
- `page_size` is the granularity of both sets, in bytes. It is the host page
  size (usually 4096), also when the guest memory is backed by 2 MiB hugetlbfs
  pages.
- `bitmap_encoding` names the serialization of the two sets. It is always
  `roaring` today. Check it and refuse anything else: the field exists so that,
  should a future Firecracker change the binary format, an old backend fails
  cleanly on the name rather than decode garbage.
- `memfd_authoritative_pages` and `zero_pages` are each the standard base64 (RFC
  4648, with padding) of a [Roaring bitmap](https://roaringbitmap.org) in the
  [portable serialization format](https://github.com/RoaringBitmap/RoaringFormatSpec),
  32-bit members, each member a page index. The two sets are disjoint and no
  member is at or past `total_size / page_size`.

Roaring libraries exist for every mainstream language (CRoaring, `roaring` for
Rust and Go, RoaringBitmap for Java, `pyroaring`); deserialize the two strings
and you have sets with constant-time membership and ordered iteration. In the
example, `memfd_authoritative_pages` decodes to pages 0–1, 9–12, 40, 58–67, 100,
130–131, 157 and 170–180, and `zero_pages` to pages 192–255, the unplugged slot.

**The classification relies on `mincore` being accurate, with or without dirty
tracking.** A page that holds content but is not "in core" would be reported as
zero and lost from the snapshot. On tmpfs-backed memory that happens if the host
swaps memfd pages out, so swap must be disabled on the host, as the
[production host setup](../prod-host-setup.md) already requires; on hugetlbfs,
which is never swapped, `mincore` reports whether Firecracker has the huge page
mapped, which it does for every page it or the backend has populated.

Applying the layout (copying every authoritative page, zeroing every zero page,
leaving every other page alone) to the previous memory file yields a file
identical to the `Full` Firecracker would have written; applied to a fresh
zero-filled file it yields a diff file `rebase-snap` accepts, or, for a microVM
that was booted rather than restored, a complete memory file. Write zeros rather
than punching holes when merging a diff, and when producing a diff file for
`rebase-snap`: to those a hole means "not in the diff".

Sizes: each set is never larger than a plain bitmap of the file, 32 KiB per GiB
of guest memory plus 0.1%, whatever the dirty pattern; that is the cost of a
guest rewriting its memory at random. Everything else is smaller: a run of
consecutive pages, however long (a released balloon, an unplugged region, a
freshly written buffer), costs 4 bytes, and an isolated page 2 bytes, so an idle
guest's response is a few KiB.

Like writing a memory file, this consumes the dirty tracking state: the pages
returned are no longer considered dirty. The virtqueue pages of every activated
device are marked dirty again afterwards, as today, so that they are part of the
next diff.

### `PUT /snapshot/dirty-pages`: pre-copy

The request has no parameters; the body may be empty or `{}`. The response is
`200 OK` with the same `memory` object as `/snapshot/create`, without
`snapshot_type`:

```json
{
  "memory": {
    "total_size": 1048576,
    "page_size": 4096,
    "bitmap_encoding": "roaring",
    "memfd_authoritative_pages": "OzAAAAEAAAoAAwAAAAEAQAAHAGQAAAA=",
    "zero_pages": "OzAAAAEAAD8AAQDAAD8A"
  }
}
```

It returns the pages dirtied since the last snapshot or the last call, and
resets the tracking. It can be issued while the microVM is `Running` or
`Paused`, and is rejected with 400 when no memory backend is attached, because
then nobody but Firecracker could turn the consumed dirty set into bytes.

In case of a running guest, the returned pages might be modified after this API
returns. In that case, it's guaranteed they will be returned on the next call of
`/snapshot/dirty-pages` or `/snapshot/create`.

## Consistency

In order to produce a consistent snapshot, the backend needs to adhere to some
simple rules. These rules cannot be enforced by Firecracker.

- The authoritative pages returned by the `/snapshot/dirty-pages` and
  `/snapshot/create` APIs must be eventually copied into the snapshot, and the
  zero pages must read as zero in it. Responses must be applied in the order
  they were received: a page zeroed by one response and copied by the next would
  otherwise end up zero, with nothing left to correct it. Applying them late is
  fine; the memfd may hold a newer value by then, and the write that made it
  newer is in a later response
- If a response is lost (e.g. connection dropped before the body was read), that
  dirty information is gone and the next snapshot must be a `Full` snapshot,
  which Firecracker writes itself
- After the final `/snapshot/create`, the VM must not be resumed until the
  backend has copied every authoritative page out of the memfd.

As an example, without pre-copy, the snapshot process would be something like:

1. Pause the VM
1. Call `/snapshot/create`
1. Copy the authoritative pages into a new file, zero the zero pages
1. VM can be resumed here

For a backend that implements pre-copy:

1. Call `/snapshot/dirty-pages`
1. Copy the authoritative pages into a file, zero the zero pages
1. Repeat from step 1 until the dirty set is small enough or after a timeout or
   iterations limit
1. Pause the VM
1. Call `/snapshot/create`
1. Final copy of the authoritative pages, final zeroing of the zero pages
1. VM can be resumed here

Note: this algorithm doesn't make sense without dirty tracking. With mincore,
the dirty set doesn't decrease between API calls.

## Example handler

The example handlers in
[`src/firecracker/examples/uffd/`](../../src/firecracker/examples/uffd/) work as
memory backends. They require minimal external orchestration to call Firecracker
APIs, and tell the backend which pages to copy.

## Limitations

- Restoring with a memory backend always populates memory through the uffd.
  Populating from a snapshot file while sharing memory (the `File` backend's
  behaviour) is not offered.
- The host must not swap: the classification uses `mincore` to tell a punched
  page from one with content, with or without dirty tracking, and a swapped-out
  page looks like a punched one. (Firecracker's own `mincore` diffs have the
  same requirement.) Diff snapshots taken without `track_dirty_pages` also miss
  pages released by the balloon or virtio-mem, so merging them onto a base keeps
  the pre-release bytes there; that is not specific to memory backends.
- Firecracker does not monitor the backend. Killing it leaves the microVM
  running with nobody to produce snapshots (and, after a restore, nobody to
  serve page faults; a `Full` snapshot then blocks on the first fault).
- A `Full` snapshot with a backend is not cheaper than without one: it reads all
  of guest memory through Firecracker's mapping, which after a lazy restore
  populates the memfd entirely. Producing full memory files by merging diffs
  into the previous file is the backend's job.
