# Memory backend: what changes for UFFD handlers

Status: design. Not yet implemented. This is the short version, focused on what
a UFFD page-fault handler and the orchestrator driving Firecracker have to do
differently; the full rationale is in `memory-backend-design-v3.md`.

Terms: the *handler* is the process listening on the UDS that receives the
handshake (today's UFFD handler); the *orchestrator* is whatever issues HTTP
requests to Firecracker's API socket. They may be the same process.

## Goal

Let the process that already serves page faults for a microVM also hold the
guest memory itself, so that it can write full and differential memory
snapshots, byte-for-byte identical to Firecracker's, without Firecracker
writing guest memory to disk. The same process works on first boot, when there
are no page faults to serve, and can drive iterative pre-copy (dirty ranges
while the guest runs, one final consistent pass while it is paused).

Firecracker's part: allocate guest memory as a single memfd whose layout is the
snapshot file layout, hand it to the handler over the existing handshake, and
report over the HTTP API which byte ranges of it a snapshot consists of.
Copying bytes is the handler's job; Firecracker never talks to it again after
the handshake.

## What does not change

- The handshake message. It is the same JSON array of `GuestRegionUffdMapping`
  (`base_host_virt_addr`, `size`, `offset`, `page_size`, `page_size_kib`),
  sent once over the handler's UDS, with fds attached via `SCM_RIGHTS`.
  `offset` is the region's offset in the memfd, which is also its offset in a
  memory snapshot file.
- Fault serving. `UFFDIO_COPY`/`UFFDIO_ZEROPAGE` on the mapping work as today;
  the mapping is now shmem-backed (or hugetlbfs), which the uffd supports in
  MISSING mode on Linux 5.10.
- A handler started for `backend_type: Uffd` receives exactly what it receives
  today and needs no change.
- Firecracker never reads from the handshake socket. The handler can keep
  using it to detect Firecracker exiting and to read its PID with
  `SO_PEERCRED`.

## What changes in the handshake: the fds

| How the microVM was started                        | fds received    |
| :------------------------------------------------- | :-------------- |
| restore, `mem_backend.backend_type: Uffd` (today)  | `[uffd]`        |
| restore, `mem_backend.backend_type: MemoryBackend` | `[uffd, memfd]` |
| boot, `machine-config.mem_backend`                 | `[memfd]`       |

- The uffd, when present, is first; the memfd is last. The handler must read
  all fds from the `recvmsg`, not just one.
- The handler knows which case it was started for. To verify,
  `readlink /proc/self/fd/N` gives `anon_inode:[userfaultfd]` or `/memfd:...`.
- The memfd is sealed against resizing (`F_SEAL_SHRINK|GROW|SEAL`). Its size is
  the sum of all region sizes, including the virtio-mem hotplug region if one
  is configured, and equals `total_size` in the snapshot responses below.
- With only a memfd (boot) there are no faults to serve; the handler keeps the
  memfd and waits for the orchestrator to ask for copies.

## API changes (orchestrator side)

### `PUT /machine-config`: new optional field `mem_backend`

```json
{
  "mem_backend": {
    "backend_type": "MemoryBackend",
    "backend_path": "/run/h.sock"
  }
}
```

Pre-boot only; also accepted in the JSON config file. Only `MemoryBackend` is
valid here. Effect: guest memory is one memfd, and the handshake with `[memfd]`
is performed during boot, before vCPUs start. Not persisted in snapshots.

### `PUT /snapshot/load`: new `backend_type` value

```json
{
  "snapshot_path": "/path/vmstate",
  "mem_backend": {
    "backend_type": "MemoryBackend",
    "backend_path": "/run/h.sock"
  }
}
```

`File` and `Uffd` behave as today. `MemoryBackend` behaves as `Uffd`, with
memfd-backed memory and the handshake carrying `[uffd, memfd]`. The choice is
per instance: a snapshot made with a memory backend restores fine with `File`
or `Uffd`, and vice versa.

### `PUT /snapshot/create`: `mem_file_path` optional, response body

```json
{ "snapshot_type": "Full" | "Diff", "snapshot_path": "/path/vmstate" }
```

With a memory backend, `mem_file_path` must be absent (Firecracker does not
write memory; 400 if present). Without one it stays mandatory. Still requires
`Paused`.

Response with a memory backend: `200 OK` instead of `204`, with

```json
{
  "snapshot_type": "Diff",
  "memory": {
    "total_size": 1073741824,
    "ranges": [
      { "offset": 0, "len": 8192 },
      { "offset": 1048576, "len": 4096 }
    ],
    "unplugged": [{ "offset": 805306368, "len": 268435456 }]
  }
}
```

- `total_size`: size of a full memory file. The target is created at this
  size.
- `ranges`: bytes to copy from the memfd into the target, same offsets. For
  `Full` this is every plugged byte; for `Diff` it is what a Firecracker diff
  file would contain. Sorted, merged, page-aligned.
- `unplugged`: bytes to zero in the target (currently unplugged virtio-mem
  slots). Disjoint from `ranges`. Empty without virtio-mem.

`copy_file_range(memfd, off, target, off, len)` per range, then zero
`unplugged`, yields a file identical to what Firecracker would have written.
Merging a `Diff` into an existing full file is the same operation on that file.

### `PUT /snapshot/dirty-ranges`: new, for pre-copy

```json
{}
```

Same `200 OK` body as above, `snapshot_type` omitted. Callable while `Running`
or `Paused`. Consuming: the pages returned are cleared from Firecracker's dirty
tracking. Rejected with 400 when no memory backend is connected.

## Consistency: when the handler may copy

Firecracker does not coordinate with the handler at snapshot time. There is no
message to acknowledge and no window to hold open. Correctness rests on what
`Paused` means in Firecracker: after `PATCH /vm {"state": "Paused"}`, the vCPUs
are stopped *and* the VMM thread stops running device emulation; it only serves
API requests until `Resumed`. Once `PUT /snapshot/create` has returned, nothing
in Firecracker writes guest memory until the resume.

The sequence the orchestrator must follow for a snapshot:

1. `PATCH /vm {"state": "Paused"}`
1. `PUT /snapshot/create` → vmstate file plus `memory` body
1. hand `memory` to the handler; it copies from the memfd
1. only after the copy is complete: `PATCH /vm {"state": "Resumed"}`

Rules:

- The dirty set returned by `snapshot/create` is the one that matches the
  vmstate file and the one to use for the snapshot, not a `dirty-ranges` result
  taken earlier in the pause: `snapshot/create` drains in-flight block I/O
  first and is the only call whose ranges are complete.
- `dirty-ranges` while `Running` is for pre-copy. Pages can change between the
  response and the copy; they are then dirty again and come back in the next
  set. Every set is consuming, so every set must be copied; a `Diff`
  `snapshot/create` after pre-copy passes returns only what changed since the
  last pass.
- If the orchestrator loses a `Diff` response (connection dropped before the
  body was read), that dirty information is gone. The next snapshot must be a
  `Full`.
- Resuming before the copy finishes produces a memory image that does not match
  the vmstate. Firecracker cannot detect this.

## Handler checklist

- [ ] Receive all fds from the handshake `recvmsg`; uffd first (if any), memfd
      last (if any).
- [ ] Run without a uffd when started for a boot.
- [ ] Keep the memfd for the life of the microVM.
- [ ] Implement "copy these ranges from the memfd to this file, zero these
      ranges" from the `memory` object, creating the file at `total_size` or
      merging into an existing one. How the orchestrator passes the `memory`
      object to the handler is up to them; Firecracker is not involved.
- [ ] Have the orchestrator issue `PATCH /vm Paused`, `PUT /snapshot/create`,
      copy, `PATCH /vm Resumed` in that order, and fall back to `Full` after a
      lost `Diff` response.
