# Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Tests for sharing guest memory with an external memory backend process.

See docs/snapshotting/memory-backend-design.md. The backend used here is the
reference implementation in src/firecracker/examples/memory_backend.
"""

import dataclasses
import filecmp
import time

import pytest

from framework.artifacts import GUEST_KERNEL_DEFAULT, pin_guest_kernel


def dirty_guest_memory(vm, mib=32):
    """Have the guest write to a good chunk of its memory."""
    vm.ssh.check_output(
        f"dd if=/dev/urandom of=/tmp/dirty bs=1M count={mib} conv=fsync && sync"
    )


def boot_with_backend(vm, **config):
    """Boot `vm` with a memory backend attached."""
    vm.spawn()
    vm.basic_config(**config)
    vm.add_net_iface()
    vm.use_memory_backend()
    vm.start()
    assert vm.mem_backend.is_running(), vm.mem_backend.log_data
    return vm


@pin_guest_kernel(GUEST_KERNEL_DEFAULT)
def test_mem_backend_full_and_first_diff(uvm, microvm_factory):
    """
    A backend-made first diff and a backend-made full of the same paused microVM
    are identical (the memory backend analogue of
    test_snapshot_basic.py::test_cmp_full_and_first_diff_mem), and Firecracker
    restores from them.
    """
    vm = boot_with_backend(uvm, vcpu_count=2, mem_size_mib=256, track_dirty_pages=True)
    dirty_guest_memory(vm)

    diff_snapshot = vm.snapshot_diff()
    full_snapshot = vm.snapshot_full()
    assert diff_snapshot.mem != full_snapshot.mem
    assert diff_snapshot.mem.stat().st_size == vm.mem_size_bytes
    assert filecmp.cmp(diff_snapshot.mem, full_snapshot.mem, shallow=False)

    # A file backed restore of a backend-produced snapshot.
    restored = microvm_factory.build()
    restored.spawn()
    restored.restore_from_snapshot(full_snapshot, resume=True)
    restored.ssh.check_output("true")


@pin_guest_kernel(GUEST_KERNEL_DEFAULT)
def test_mem_backend_diff_rebase(uvm, microvm_factory):
    """
    A backend-made diff rebased onto the previous backend-made full equals a
    backend-made full of the current state: no dirty page is missed.
    """
    vm = boot_with_backend(uvm, vcpu_count=2, mem_size_mib=256, track_dirty_pages=True)
    dirty_guest_memory(vm)

    base = vm.snapshot_full()
    vm.resume()
    dirty_guest_memory(vm)

    diff = vm.snapshot_diff()
    # Nothing runs in between: the VM stays paused.
    full = vm.snapshot_full()

    rebased = diff.rebase_snapshot(base)
    assert filecmp.cmp(rebased.mem, full.mem, shallow=False)

    restored = microvm_factory.build()
    restored.spawn()
    restored.restore_from_snapshot(rebased, resume=True)
    restored.ssh.check_output("true")


def differing_pages(a, b, page_size=4096):
    """Return the (page index, differing byte count) of pages that differ between files."""
    assert a.stat().st_size == b.stat().st_size
    diffs = []
    with open(a, "rb") as fa, open(b, "rb") as fb:
        index = 0
        while True:
            pa = fa.read(page_size)
            pb = fb.read(page_size)
            if not pa:
                break
            if pa != pb:
                diffs.append((index, sum(x != y for x, y in zip(pa, pb))))
            index += 1
    return diffs


@pin_guest_kernel(GUEST_KERNEL_DEFAULT)
def test_mem_backend_restore(uvm, microvm_factory):
    """
    Restore through a memory backend (which also serves page faults), take a
    snapshot through it, and check it matches what Firecracker produces for a
    file restore of the same snapshot.
    """
    vm = uvm
    vm.spawn()
    vm.basic_config(vcpu_count=2, mem_size_mib=256)
    vm.add_net_iface()
    vm.start()
    dirty_guest_memory(vm)
    snapshot = vm.snapshot_full()
    vm.kill()

    # Restored through the backend, paused: a full snapshot must reproduce the
    # memory file, with untouched pages filled from it.
    via_backend = microvm_factory.build()
    via_backend.spawn()
    via_backend.restore_from_snapshot(snapshot, mem_backend=True)
    assert via_backend.mem_backend.is_running(), via_backend.mem_backend.log_data
    backend_full = via_backend.snapshot_full()

    via_file = microvm_factory.build()
    via_file.spawn()
    via_file.restore_from_snapshot(snapshot)
    file_full = via_file.snapshot_full()

    # The images are not expected to be strictly identical: each restore writes
    # into guest memory a fresh random 16-byte VMGenID and updates a few vmclock
    # fields, and device emulation keeps running while the VM is paused (a frame
    # arriving on the tap updates a used ring). All of these amount to a handful
    # of bytes; a layout or hole handling bug would show up as megabytes.
    def assert_same_but_restore_writes(a, b):
        diffs = differing_pages(a, b)
        assert len(diffs) <= 4 and sum(count for _, count in diffs) <= 64, diffs

    assert_same_but_restore_writes(backend_full.mem, file_full.mem)
    assert_same_but_restore_writes(backend_full.mem, snapshot.mem)

    # The backend-restored VM is functional and can keep snapshotting after
    # running, with page faults served by the backend.
    via_backend.resume()
    via_backend.ssh.check_output("true")
    dirty_guest_memory(via_backend)
    later = via_backend.snapshot_full()

    restored = microvm_factory.build()
    restored.spawn()
    restored.restore_from_snapshot(later, resume=True)
    restored.ssh.check_output("test -f /tmp/dirty")


@pin_guest_kernel(GUEST_KERNEL_DEFAULT)
def test_mem_backend_model_change_across_restore(uvm, microvm_factory):
    """
    Whether a memory backend is used is not persisted: a snapshot taken through
    a backend restores with plain UFFD, and a UFFD-restored VM snapshots through
    a backend after another restore.
    """
    vm = boot_with_backend(uvm, vcpu_count=2, mem_size_mib=256)
    dirty_guest_memory(vm)
    snapshot = vm.snapshot_full()
    vm.kill()

    uffd_vm = microvm_factory.build()
    uffd_vm.spawn()
    uffd_vm.restore_from_snapshot(snapshot, resume=True, uffd_handler_name="on_demand")
    uffd_vm.ssh.check_output("true")
    assert uffd_vm.api.machine_config.get().json().get("mem_backend") is None
    snapshot2 = uffd_vm.snapshot_full()
    uffd_vm.kill()

    backend_vm = microvm_factory.build()
    backend_vm.spawn()
    backend_vm.restore_from_snapshot(snapshot2, resume=True, mem_backend=True)
    backend_vm.ssh.check_output("true")
    assert (
        backend_vm.api.machine_config.get().json()["mem_backend"]["backend_type"]
        == "MemoryBackend"
    )
    snapshot3 = backend_vm.snapshot_full()
    assert snapshot3.mem.stat().st_size == backend_vm.mem_size_bytes


@pin_guest_kernel(GUEST_KERNEL_DEFAULT)
def test_mem_backend_api_errors(uvm):
    """
    Invalid combinations of the memory backend API are rejected.
    """
    vm = uvm
    vm.spawn()
    vm.basic_config(vcpu_count=1, mem_size_mib=128)

    # Only `MemoryBackend` can be configured in machine-config.
    with pytest.raises(RuntimeError, match="MemoryBackend"):
        vm.api.machine_config.patch(
            mem_backend={"backend_type": "Uffd", "backend_path": "/foo"}
        )

    # Connecting to a socket nobody listens on fails the boot; Firecracker stays
    # up and can be reconfigured.
    vm.api.machine_config.patch(
        mem_backend={"backend_type": "MemoryBackend", "backend_path": "/nonexistent"}
    )
    with pytest.raises(RuntimeError, match="Cannot connect to the memory backend"):
        vm.api.actions.put(action_type="InstanceStart")
    assert vm.api.machine_config.get().json()["mem_backend"]["backend_path"] == (
        "/nonexistent"
    )


@pin_guest_kernel(GUEST_KERNEL_DEFAULT)
def test_mem_backend_snapshot_create_errors(uvm, microvm_factory, guest_kernel, rootfs):
    """
    `PUT /snapshot/create` rejects `MemoryBackend` without a connected backend,
    and `mem_file_path` while one is connected.
    """
    vm = uvm
    vm.spawn()
    vm.basic_config(vcpu_count=1, mem_size_mib=128)
    vm.add_net_iface()
    vm.start()
    vm.pause()
    with pytest.raises(RuntimeError, match="No memory backend is connected"):
        vm.api.snapshot_create.put(
            mem_backend={"backend_type": "MemoryBackend"}, snapshot_path="vmstate"
        )
    # Firecracker still works and can snapshot the classic way.
    vm.snapshot_full()
    vm.kill()

    vm2 = boot_with_backend(
        microvm_factory.build(guest_kernel, rootfs), vcpu_count=1, mem_size_mib=128
    )
    vm2.pause()
    with pytest.raises(RuntimeError, match="A memory backend is connected"):
        vm2.api.snapshot_create.put(mem_file_path="mem", snapshot_path="vmstate")
    # The backend path still works afterwards.
    vm2.snapshot_full()
    assert vm2.mem_backend.is_running(), vm2.mem_backend.log_data


@pin_guest_kernel(GUEST_KERNEL_DEFAULT)
def test_mem_backend_precopy(uvm, microvm_factory):
    """
    Source side of a live migration: the backend pulls dirty ranges while the
    microVM runs (pre-copy passes), then the final diff taken while paused
    completes the image. Pre-copy image + final diff must equal a full snapshot.
    """
    vm = boot_with_backend(uvm, vcpu_count=2, mem_size_mib=256, track_dirty_pages=True)
    backend = vm.mem_backend
    dirty_guest_memory(vm)

    # First pass while running: everything the guest touched since boot.
    ranges, nbytes = backend.request_precopy_pass()
    assert ranges > 0 and 0 < nbytes <= vm.mem_size_bytes
    first_pass_bytes = nbytes
    assert backend.precopy_mem.stat().st_size == vm.mem_size_bytes

    # Keep dirtying; the second pass only carries what changed since the first.
    dirty_guest_memory(vm, mib=8)
    ranges, nbytes = backend.request_precopy_pass()
    assert ranges > 0 and 0 < nbytes < first_pass_bytes

    dirty_guest_memory(vm, mib=4)

    # Quiesce the network before the final pass. Device emulation keeps running
    # while the VM is paused, so a frame arriving on the tap between the two
    # snapshots below (e.g. an ACK of the SSH session) would land in guest RX
    # buffers and make the reference full differ from the diff by design rather
    # than by bug. Frames already queued are drained while the guest still runs,
    # where they are dirty-tracked like any other write.
    taps = [iface["tap"].name for iface in vm.iface.values()]
    for tap in taps:
        vm.netns.check_output(f"ip link set {tap} down")
    time.sleep(1)

    # Final pass: pause, diff through the backend, then a full for reference
    # (nothing runs in between, the VM stays paused).
    final_diff = vm.snapshot_diff()
    full = vm.snapshot_full()
    assert final_diff.mem != backend.precopy_mem
    for tap in taps:
        vm.netns.check_output(f"ip link set {tap} up")

    precopy_base = dataclasses.replace(final_diff, mem=backend.precopy_mem)
    migrated = final_diff.rebase_snapshot(precopy_base)
    assert differing_pages(migrated.mem, full.mem) == []

    # The "destination" boots from the pre-copied image.
    restored = microvm_factory.build()
    restored.spawn()
    restored.restore_from_snapshot(migrated, resume=True)
    restored.ssh.check_output("test -f /tmp/dirty")
