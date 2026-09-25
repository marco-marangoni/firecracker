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
and `PUT /snapshot/dirty-pages` return a bitmap of the pages of the memfd to
copy, and whoever made the call passes it to the backend however it likes; the
example handler uses a second Unix socket. There is no other protocol between
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
    "total_size": 1048576,
    "page_size": 4096,
    "pages": "Ax4AAAABAPwPAAAAEAAAAAwAACAA/B8A",
    "unplugged": [
      {
        "offset": 786432,
        "len": 262144
      }
    ]
  }
}
```

The example is a 1 MiB guest (256 pages) whose last 256 KiB is an unplugged
virtio-mem slot. The bitmap covers the 192 plugged pages and decodes to 24
bytes. Bit `b` of byte `i` is page `8 * i + b`, so in the usual binary notation
the lowest page of a byte is its rightmost bit:

```text
index  pages    hex   binary    dirty pages
    0    0-7    0x03  00000011  0-1
    1    8-15   0x1e  00011110  9-12
    2   16-23   0x00  00000000  -
    3   24-31   0x00  00000000  -
    4   32-39   0x00  00000000  -
    5   40-47   0x01  00000001  40
    6   48-55   0x00  00000000  -
    7   56-63   0xfc  11111100  58-63
    8   64-71   0x0f  00001111  64-67
    9   72-79   0x00  00000000  -
   10   80-87   0x00  00000000  -
   11   88-95   0x00  00000000  -
   12   96-103  0x10  00010000  100
   13  104-111  0x00  00000000  -
   14  112-119  0x00  00000000  -
   15  120-127  0x00  00000000  -
   16  128-135  0x0c  00001100  130-131
   17  136-143  0x00  00000000  -
   18  144-151  0x00  00000000  -
   19  152-159  0x20  00100000  157
   20  160-167  0x00  00000000  -
   21  168-175  0xfc  11111100  170-175
   22  176-183  0x1f  00011111  176-180
   23  184-191  0x00  00000000  -
```

Pages 0–1, 9–12, 40, 58–67, 100, 130–131, 157 and 170–180 are to be copied.
Pages 192–255 are unplugged and not covered by the bitmap; `unplugged` says to
zero them. A 1 GiB guest with all its memory plugged has a 32 KiB bitmap.

- `total_size` is the size of a full memory file (the sum of all region sizes,
  including the hotplug region). Create the target file at this size.
- `page_size` is the granularity of `pages`, in bytes. It is the host page size
  (usually 4096), also when the guest memory is backed by 2 MiB hugetlbfs pages.
- `pages` is the set of pages to copy from the memfd into the target, at the
  same offset, as a bitmap: standard base64 (RFC 4648, with padding). Byte `i`,
  bit `b` (least significant bit first) is the page at offset
  `(8 * i + b) * page_size`. Read little-endian, the bytes are also an array of
  64-bit words in which bit `j` of word `w` is page `64 * w + j`. The bitmap
  covers the file from offset 0 up to the end of the last plugged slot,
  `ceil(plugged_end / page_size / 8)` bytes, whatever is dirty: its length is a
  function of the plug state alone, and unplugged memory at the end of the file
  (an unplugged hotplug region, however large) costs nothing. Pages past its end
  are clear, and so are the bits of unplugged slots. For `Diff` every plugged
  page dirtied since the last snapshot is set, as tracked by KVM's dirty log (or
  `mincore` when `track_dirty_pages` is off) and by Firecracker's own
  device-write tracking. Pages that became zero because the guest released them
  (balloon), or because their virtio-mem slot was unplugged and plugged again
  since, count as dirty. **For `Full` the field is absent**: every page outside
  `unplugged` is to be copied.
- `unplugged` are the currently unplugged virtio-mem slots, as sorted,
  page-aligned, non-overlapping `{offset, len}` pairs. Empty without virtio-mem.
  **Every byte of these ranges must be zero in the snapshot produced by the
  backend.**

Copying every set page, followed by zeroing the `unplugged` range, yields a file
identical to what Firecracker would have written. Merging a `Diff` into an
existing full memory file is the same operation applied to that file.

The bitmap is at most 32 KiB per GiB of guest memory (43 KiB as base64).

Like writing a memory file, this consumes the dirty tracking state: the pages
returned are no longer considered dirty. The virtqueue pages of every activated
device are marked dirty again afterwards, as today, so that they are part of the
next diff.

### `PUT /snapshot/dirty-pages`: pre-copy

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
`/snapshot/dirty-pages` or `/snapshot/create`.

## Consistency

In order to produce a consistent snapshot, the backend needs to adhere to some
simple rules. These rules cannot be enforced by Firecracker.

- The pages returned by the `/snapshot/dirty-pages` and `/snapshot/create` APIs
  must be eventually copied into the snapshot
- The `unplugged` ranges returned by `/snapshot/create` must be zero in the
  resulting memory file
- If a response is lost (e.g. connection dropped before the body was read), that
  dirty information is gone and the next snapshot must be a `Full` snapshot
- After the final `/snapshot/create`, the VM must not be resumed until the
  backend has copied every set page out of the memfd.

As an example, without pre-copy, the snapshot process would be something like:

1. Pause the VM
1. Call `/snapshot/create`
1. Copy the set pages into a new file
1. VM can be resumed here

For a backend that implements pre-copy:

1. Call `/snapshot/dirty-pages`
1. Copy the set pages into a file
1. Zero unplugged ranges
1. Repeat from step 1 until the dirty set is small enough or after a timeout or
   iterations limit
1. Pause the VM
1. Call `/snapshot/create`
1. Final copy of the set pages into a file
1. Final zero-ing of unplugged ranges
1. VM can be resumed here

Note: this algorithm doesn't make sense without dirty tracking. With mincore,
the dirty set doesn't decrease between API calls.

### What the backend should copy

The `pages` Firecracker returns say *which* pages to copy. They do not say
*where from*, and after a restore that is not always the memfd. The rule is: the
copy must contain what a read through Firecracker's mapping would return at that
moment.

For a backend attached at **boot** the answer is simple: all pages can be read
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

For both **boot** and **resume**, `unplugged` ranges must end up zero in the
target.

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
APIs, and tell the backend which pages to copy.

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
