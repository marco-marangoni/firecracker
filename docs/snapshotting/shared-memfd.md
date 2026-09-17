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

When used on VM restore, the `SharedMemfd` backend is expected to also act as a
[UFFD page fault handler](handling-page-faults-on-snapshot-resume.md).

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
| `offset`              | Offset of the region in the memfd. It is also its offset in a memory snapshot file and in the ranges the API returns. |
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
and `PUT /snapshot/dirty-ranges` return byte ranges of the memfd to copy, and
whoever made the call passes them to the backend however it likes; the example
handler uses a second Unix socket. There is no other protocol between
Firecracker and the backend.

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

The microVM must be `Paused`, as always. With a backend attached,
`mem_file_path` must be **absent** (400 otherwise): Firecracker does not write
guest memory in this mode. Without a backend it stays mandatory and the request
behaves as before, answering `204 No Content`.

With a backend the response is `200 OK` with a body describing the memory part
of the snapshot:

```json
{
  "snapshot_type": "Diff",
  "memory": {
    "total_size": 1073741824,
    "ranges": [
      {
        "offset": 0,
        "len": 8192
      },
      {
        "offset": 1048576,
        "len": 4096
      }
    ],
    "unplugged": [
      {
        "offset": 805306368,
        "len": 268435456
      }
    ]
  }
}
```

- `total_size` is the size of a full memory file (the sum of all region sizes,
  including the hotplug region). Create the target file at this size.
- `ranges` are the bytes to copy from the memfd into the target, at the same
  offset. For `Full` this is every plugged byte; for `Diff` it is exactly what a
  Firecracker-written diff file would contain: every page dirtied since the last
  snapshot, as tracked by KVM's dirty log (or `mincore` when `track_dirty_pages`
  is off) and by Firecracker's own device-write tracking.
- `unplugged` are the bytes to zero in the target: the currently unplugged
  virtio-mem slots. Empty without virtio-mem. Zeroing them makes the result
  identical to a fresh Firecracker-written file, including when a `Diff` is
  merged into a full file in which a slot has been unplugged since.

All ranges are sorted, merged, page-aligned and disjoint from each other.
`copy_file_range(memfd, off, target, off, len)` for every range followed by
zeroing `unplugged` yields a file identical to what Firecracker would have
written. Merging a `Diff` into an existing full memory file is the same
operation applied to that file.

Like writing a memory file, this consumes the dirty tracking state: the pages
returned are no longer considered dirty. The virtqueue pages of every activated
device are marked dirty again afterwards, as today, so that they are part of the
next diff.

### `PUT /snapshot/dirty-ranges`: pre-copy

```json
{}
```

Returns `200 OK` with the same `memory` object as the `/snapshot/create` API
(without `snapshot_type`): the pages dirtied since the last snapshot or the last
call, and resets the tracking. It can be issued while the microVM is `Running`
or `Paused`, and is rejected with 400 when no memory backend is attached,
because then nobody but Firecracker could turn the consumed dirty set into
bytes.

Before resetting the bitmaps, Firecracker returns to the guest any virtio-net RX
buffers it has parsed but not yet filled (they are marked dirty when parsed, not
when written), so that the later write cannot go unnoticed.

In case of a running guest, the returned pages might be modified after this API
returns. In that case, it's guaranteed they will be returned on the next call of
`/snapshot/dirty-ranges` or `/snapshot/create`.

## Consistency

In order to produce a consistent snapshot, the backend needs to adhere to some
simple rules. These rules cannot be enforced by Firecracker.

- The ranges returned by the `/snapshot/dirty-ranges` and `/snapshot/create`
  APIs must be eventually copied into the snapshot
- If a response is lost (e.g. connection dropped before the body was read), that
  dirty information is gone and the next snapshot must be a `Full` snapshot
- After the final `/snapshot/create`, the VM must not be resumed until the
  backend has copied every dirty range out of the memfd.

As an example, without pre-copy, the snapshot process would be something like:

1. Pause the VM
1. Call `/snapshot/create`
1. Copy the dirty ranges into a file
1. VM can be resumed here

For a backend that implements pre-copy:

1. Call `/snapshot/dirty-ranges`
1. Copy the dirty ranges into a file
1. Repeat from step 1 until the dirty ranges are small enough or after a timeout
   or iterations limit
1. Pause the VM
1. Call `/snapshot/create`
1. Final copy of the dirty ranges into a file
1. VM can be resumed here

Note: this algorithm doesn't make sense without dirty tracking. With mincore,
the dirty ranges don't decrease between API calls.

### What the backend should copy

The `ranges` Firecracker returns say *which* pages to copy. They do not say
*where from*, and after a restore that is not always the memfd. The rule is: the
copy must contain what a read through Firecracker's mapping would return at that
moment.

For a backend attached at **boot** the answer is simple: all ranges can be read
from the memfd. Pages the guest never touched are holes and read as zero;
`copy_file_range` copies nothing for them, so the target file must start out
zeroed (a freshly created file at `total_size` is).

For a backend attached at **restore**, the decision process is slightly more
complex, as it involves keeping track of whether each page is UFFD registered
and whether it was populated by the backend. Specifically, memory that is both
uffd-registered _and_ never populated by the backend should be sourced from base
snapshot file (or should be omitted from the resulting snapshot).

Note: this condition is rare, but possible to exist even in case of diff
snapshots. For example, virtIO queues might be marked as dirty before any data
is written to them, so they might be in a dirty, uffd-register, and unpopulated
state.

For both **boot** and **resume**, as an optimization, `unplugged` ranges can be
assumed to be zeroes; reading them from the memfd would be correct but wasteful.

### UFFD unregistration

As mentioned in the previous section, keeping track of which ranges are UFFD
registered is a requirement to produce correct snapshots.

One scenario is when backends unregister themselves from the range, e.g. when
they detect the range as zero-filled, and they issue a `UFFDIO_UNREGISTER`
instead of a `UFFDIO_ZEROPAGE` command.

Another scenario is in response to the balloon-device freeing host memory. In
this case, a UFFD `remove` event is sent to the backend, which must act on it:
either call `UFFDIO_UNREGISTER` on the range or remember to return zero at the
next fault. Either is correct. Ignoring the event is not: the next fault would
be served from the snapshot file, bringing back data the guest has released.

In both scenarios, the backend must consider memfd as the authoritative memory
source when taking a snapshot for those unregistered ranges, as the guest might
have written there without notifying the backend.

## Example handler

The example handlers in
[`src/firecracker/examples/uffd/`](../../src/firecracker/examples/uffd/) work as
memory backends. They require minimal external orchestration to call Firecracker
APIs, and tell the backend which memory ranges to copy.

## Limitations

- Restoring with a memory backend always populates memory through the uffd.
  Populating from a snapshot file while sharing memory (the `File` backend's
  behaviour) is not offered.
- Diff snapshots taken without `track_dirty_pages` (the `mincore` path) do not
  record pages released by the balloon or virtio-mem, so merging them onto a
  base keeps the pre-release bytes there. Like any `mincore` diff, they also
  require swap to be disabled on the host: a page written to swap is not "in
  core" and would be left out. Neither is specific to memory backends.
- Firecracker does not monitor the backend. Killing it leaves the microVM
  running with nobody to produce snapshots (and, after a restore, nobody to
  serve page faults).
