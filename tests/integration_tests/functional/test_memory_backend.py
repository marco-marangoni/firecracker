# Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Tests for the memory backend: sharing guest memory with a UFFD handler
through a memfd and producing memory snapshots from it, byte-for-byte identical
to the ones Firecracker writes itself.

The test framework plays the orchestrator: it calls `PUT /snapshot/create` (with
`snapshot_type: Backend`) or `PUT /snapshot/dirty-pages`, receives the `memory`
object and forwards it to the example handler's control socket, which copies the
set pages out of the memfd. Full snapshots are Firecracker's own, memory backend
or not.
"""

# pylint: disable=too-many-lines

import base64
import filecmp
import os
import platform
import shutil
import struct
from pathlib import Path
from subprocess import TimeoutExpired

import pytest

import host_tools.cargo_build as build_tools
from framework.artifacts import GUEST_KERNEL_DEFAULT, pin_guest_kernel
from framework.microvm import Snapshot, SnapshotType
from framework.utils import get_stable_rss_mem, make_guest_dirty_memory
from framework.utils_hugepages import HugePagesConfig
from framework.utils_uffd import spawn_pf_handler, uffd_handler
from integration_tests.functional.test_balloon import wait_for_balloon_actual

pytestmark = pin_guest_kernel(GUEST_KERNEL_DEFAULT)

MEM_SIZE_MIB = 256

# Backing page configurations the identity tests run with. Explicit 2M pages
# change the memfd (MFD_HUGETLB), the uffd registration (hugetlbfs), the
# handler's page size and the copy path (`copy_file_range` is not available on
# hugetlbfs, and only whole huge pages can be punched out or unregistered).
PAGE_CONFIGS = [HugePagesConfig.NONE, HugePagesConfig.HUGETLBFS_2MB]


def restore_kwargs(huge_pages):
    """How to restore a snapshot of a microVM with this page configuration.

    Explicit 2M pages cannot be populated by the `File` backend, so those
    snapshots are restored through a UFFD handler.
    """
    if huge_pages == HugePagesConfig.HUGETLBFS_2MB:
        return {"uffd_handler_name": "on_demand"}
    return {}


MEMHP_BOOTARGS = "console=ttyS0 reboot=k panic=1 memhp_default_state=online_movable"


def boot_with_mem_backend(
    vm,
    handler="on_demand",
    balloon=False,
    track_dirty_pages=True,
    mem_size_mib=MEM_SIZE_MIB,
    **kwargs,
):
    """Boot `vm` with an example handler attached as memory backend."""
    vm.spawn()
    if kwargs.get("huge_pages", HugePagesConfig.NONE) != HugePagesConfig.NONE:
        # Huge page mappings are not accounted the way the monitor expects.
        vm.memory_monitor = None
    vm.basic_config(
        vcpu_count=2,
        mem_size_mib=mem_size_mib,
        track_dirty_pages=track_dirty_pages,
        mem_backend=handler,
        **kwargs,
    )
    vm.add_net_iface()
    if balloon:
        vm.api.balloon.put(
            amount_mib=0, deflate_on_oom=True, stats_polling_interval_s=1
        )
    vm.start()
    assert vm.mem_backend is not None
    assert vm.mem_backend.is_running(), vm.mem_backend.log_data
    return vm


PAGE_SIZE = 4096


def roaring_decode(data):
    """Decode a Roaring bitmap in the portable serialization format
    (https://github.com/RoaringBitmap/RoaringFormatSpec) into a set of ints.

    Written out rather than taken from `pyroaring` so that the test checks
    Firecracker's output against the specification independently."""
    serial_cookie_no_runcontainer = 12346
    serial_cookie = 12347
    cookie = struct.unpack_from("<I", data, 0)[0]
    pos = 4
    if (cookie & 0xFFFF) == serial_cookie:
        size = (cookie >> 16) + 1
        run_bytes = (size + 7) // 8
        run_flags = data[pos : pos + run_bytes]
        pos += run_bytes
        has_offsets = size >= 4
    elif cookie == serial_cookie_no_runcontainer:
        size = struct.unpack_from("<I", data, pos)[0]
        pos += 4
        run_flags = bytes(1)
        has_offsets = True
    else:
        raise ValueError(f"bad Roaring cookie {cookie:#x}")
    keys_cards = [struct.unpack_from("<HH", data, pos + 4 * i) for i in range(size)]
    pos += 4 * size
    if has_offsets:
        pos += 4 * size
    out = set()
    for i, (key, card_minus_one) in enumerate(keys_cards):
        card = card_minus_one + 1
        base = key << 16
        if run_flags[i // 8] & (1 << (i % 8)) if i // 8 < len(run_flags) else False:
            nruns = struct.unpack_from("<H", data, pos)[0]
            pos += 2
            for _ in range(nruns):
                start, length = struct.unpack_from("<HH", data, pos)
                pos += 4
                out.update(range(base + start, base + start + length + 1))
        elif card <= 4096:
            out.update(base + v for v in struct.unpack_from(f"<{card}H", data, pos))
            pos += 2 * card
        else:
            for w, word in enumerate(struct.unpack_from("<1024Q", data, pos)):
                while word:
                    out.add(base + 64 * w + (word & -word).bit_length() - 1)
                    word &= word - 1
            pos += 8192
    assert pos == len(data), f"trailing bytes in Roaring bitmap: {len(data) - pos}"
    return out


def pages(memory, field):
    """A bitmap field of a `memory` object as a set of page indices."""
    assert memory["bitmap_encoding"] == "roaring"
    return roaring_decode(base64.b64decode(memory[field], validate=True))


def authoritative_bytes(memory):
    """Bytes the backend must copy from the memfd."""
    return len(pages(memory, "pages_to_copy")) * memory["page_size"]


def discard_bytes(memory):
    """Bytes the backend must zero."""
    return len(pages(memory, "pages_to_discard")) * memory["page_size"]


def is_authoritative(memory, offset):
    """Whether the page at `offset` is to be copied from the memfd."""
    return offset // memory["page_size"] in pages(memory, "pages_to_copy")


def is_zero(memory, offset):
    """Whether the page at `offset` is to be zeroed."""
    return offset // memory["page_size"] in pages(memory, "pages_to_discard")


def class_bytes_in(memory, offset, length):
    """(authoritative, zero) bytes within `[offset, offset + length)`."""
    page_size = memory["page_size"]
    window = set(range(offset // page_size, (offset + length) // page_size))
    return (
        len(pages(memory, "pages_to_copy") & window) * page_size,
        len(pages(memory, "pages_to_discard") & window) * page_size,
    )


def check_layout(memory, total_size):
    """Sanity checks on a `memory` object returned by Firecracker."""
    assert memory["total_size"] == total_size
    assert memory["page_size"] == PAGE_SIZE
    assert memory["bitmap_encoding"] == "roaring"
    authoritative = pages(memory, "pages_to_copy")
    zero = pages(memory, "pages_to_discard")
    # Within the file, and disjoint.
    assert all(p < total_size // PAGE_SIZE for p in authoritative | zero)
    assert not authoritative & zero


def wire_bytes(memory, field):
    """Size of a bitmap field as sent, before base64."""
    return len(base64.b64decode(memory[field], validate=True))


def copied_bytes(vm):
    """Bytes the backend copied from the memfd in its last copy for `vm`."""
    return vm.last_backend_copy["copied_bytes"]


def zeroed_bytes(vm):
    """Bytes the backend zeroed in its last copy for `vm`."""
    return vm.last_backend_copy["zeroed_bytes"]


def differing_pages(path_a, path_b, page_size=4096):
    """Offsets of the pages at which two memory files differ."""
    offsets = []
    with open(path_a, "rb") as file_a, open(path_b, "rb") as file_b:
        offset = 0
        while True:
            page_a = file_a.read(page_size)
            page_b = file_b.read(page_size)
            if not page_a and not page_b:
                break
            if page_a != page_b:
                offsets.append(offset)
            offset += page_size
    return offsets


@pytest.mark.parametrize("huge_pages", PAGE_CONFIGS)
def test_boot_diff_snapshot_restores(uvm, microvm_factory, huge_pages):
    """
    Boot with a memory backend, take a diff snapshot through it into a fresh
    file (against a booted microVM that is a complete memory file: unchanged
    pages are zero) and restore from it (with the plain `File` backend for 4K
    pages, through a UFFD handler for 2M pages). A Full written by Firecracker
    itself over the same paused state is byte-identical.
    """
    vm = boot_with_mem_backend(uvm, huge_pages=huge_pages)
    make_guest_dirty_memory(vm.ssh, amount_mib=32)

    snapshot = vm.snapshot_diff()
    memory = vm.last_snapshot_memory
    check_layout(memory, MEM_SIZE_MIB * 2**20)
    assert snapshot.mem.stat().st_size == MEM_SIZE_MIB * 2**20
    # A freshly booted guest has touched some, but not all, of its memory; everything it
    # touched since boot is dirty and resident: authoritative. What it never touched is
    # unchanged (zero since boot), and nothing was discarded, so nothing is zero.
    authoritative = authoritative_bytes(memory)
    assert 32 * 2**20 <= authoritative < MEM_SIZE_MIB * 2**20, authoritative
    assert discard_bytes(memory) == 0
    assert copied_bytes(vm) == authoritative
    assert zeroed_bytes(vm) == 0
    # Firecracker's own Full of the same state, written through its mapping.
    full = vm.snapshot_full(mem_path="mem_full", vmstate_path="vmstate_full")
    assert not differing_pages(snapshot.mem, full.mem)
    vm.kill()

    restored = microvm_factory.build_from_snapshot(
        snapshot, **restore_kwargs(huge_pages)
    )
    restored.memory_monitor = None
    restored.ssh.check_output("true")
    restored.kill()


@pytest.mark.parametrize("huge_pages", PAGE_CONFIGS)
def test_diff_self_consistency(uvm, huge_pages):
    """
    Backend-side analogue of `test_cmp_full_and_first_diff_mem`: a full copy,
    then a diff, then a full copy taken without the guest running in between.
    Rebasing the diff onto the first full copy must yield the second one.
    Any dirty page missed by the range computation shows up as a mismatch.
    """
    vm = boot_with_mem_backend(uvm, huge_pages=huge_pages)
    make_guest_dirty_memory(vm.ssh, amount_mib=32)

    base = vm.snapshot_full(mem_path="mem_base")
    vm.resume()
    make_guest_dirty_memory(vm.ssh, amount_mib=64)

    diff = vm.snapshot_diff(mem_path="mem_diff")
    diff_memory = vm.last_snapshot_memory
    check_layout(diff_memory, MEM_SIZE_MIB * 2**20)
    dirty_bytes = authoritative_bytes(diff_memory) + discard_bytes(diff_memory)
    # The workload dirtied at least what it wrote, and not everything.
    assert 64 * 2**20 <= dirty_bytes < MEM_SIZE_MIB * 2**20
    assert diff.mem.stat().st_size == MEM_SIZE_MIB * 2**20

    # Still paused: nothing changed since the diff.
    full = vm.snapshot_full(mem_path="mem_full", vmstate_path="vmstate_full")
    assert not filecmp.cmp(base.mem, full.mem, shallow=False)

    rebased = diff.rebase_snapshot(base)
    assert filecmp.cmp(rebased.mem, full.mem, shallow=False)


@pytest.mark.parametrize("huge_pages", PAGE_CONFIGS)
def test_diff_chain_restores(uvm, microvm_factory, huge_pages):
    """
    Several diffs rebased onto a base equal a final full copy, and the rebased
    result restores into a healthy guest.
    """
    vm = boot_with_mem_backend(uvm, huge_pages=huge_pages)
    base = vm.snapshot_full(mem_path="mem_base")

    diffs = []
    for i in range(3):
        vm.resume()
        make_guest_dirty_memory(vm.ssh, amount_mib=16)
        vm.ssh.check_output(f"echo round{i} > /tmp/round")
        diffs.append(vm.snapshot_diff(mem_path=f"mem_diff{i}"))

    full = vm.snapshot_full(mem_path="mem_full", vmstate_path="vmstate_full")
    vm.kill()

    rebased = base
    for diff in diffs:
        rebased = diff.rebase_snapshot(rebased)
    assert filecmp.cmp(rebased.mem, full.mem, shallow=False)

    restored = microvm_factory.build_from_snapshot(
        rebased, **restore_kwargs(huge_pages)
    )
    restored.memory_monitor = None
    assert restored.ssh.check_output("cat /tmp/round").stdout.strip() == "round2"
    restored.kill()


@pytest.mark.parametrize("huge_pages", PAGE_CONFIGS)
def test_precopy_dirty_pages(uvm, huge_pages):
    """
    Pre-copy: copy dirty pages repeatedly while the guest runs, then pause
    and copy the final `Diff` set. The result must equal a `Full` taken right
    after. This exercises the virtio-net `prepare_dirty_tracking_reset` hook,
    since ssh traffic flows during the passes.
    """
    vm = boot_with_mem_backend(uvm, huge_pages=huge_pages)
    make_guest_dirty_memory(vm.ssh, amount_mib=16)

    # Base: a full copy. The pre-copy target starts as a copy of it.
    base = vm.snapshot_full(mem_path="mem_base")
    precopy = Path(vm.chroot()) / "mem_precopy"
    shutil.copyfile(base.mem, precopy)
    vm.resume()

    for _ in range(4):
        make_guest_dirty_memory(vm.ssh, amount_mib=16)
        memory = vm.dirty_pages(copy_to="mem_precopy")
        check_layout(memory, MEM_SIZE_MIB * 2**20)
        assert authoritative_bytes(memory) > 0, "a running guest dirties something"

    # Final pass while paused, merged into the same file, via `snapshot/create` with the
    # `Backend` type (writes the microVM state and ends the lineage).
    vm.pause()
    memory = vm.api.snapshot_create.put(
        snapshot_path="vmstate", snapshot_type="Backend"
    ).json()["memory"]
    check_layout(memory, MEM_SIZE_MIB * 2**20)
    vm.mem_backend.copy(memory, "/mem_precopy")

    # Reference: everything, still paused.
    full = vm.snapshot_full(mem_path="mem_full", vmstate_path="vmstate_full")
    assert filecmp.cmp(precopy, full.mem, shallow=False)


def test_dirty_pages_are_consumed(uvm):
    """Two back-to-back `dirty-pages` calls on a paused guest: the second is empty."""
    vm = boot_with_mem_backend(uvm)
    make_guest_dirty_memory(vm.ssh, amount_mib=16)
    vm.pause()

    first = vm.dirty_pages()
    check_layout(first, MEM_SIZE_MIB * 2**20)
    assert authoritative_bytes(first) >= 16 * 2**20
    second = vm.dirty_pages()
    check_layout(second, MEM_SIZE_MIB * 2**20)
    # Only the virtqueue pages, re-marked after every reset so that they are part of the next
    # set, remain. They are faulted in when marked, so they are authoritative, never zero.
    assert authoritative_bytes(second) < 2**20
    assert discard_bytes(second) == 0
    # On the wire, a nearly empty set is a few hundred bytes (2 per page), not 32 KiB per GiB,
    # and an empty one is the 8-byte header.
    assert wire_bytes(second, "pages_to_copy") < 1024
    assert wire_bytes(second, "pages_to_discard") == 8
    third = vm.dirty_pages()
    assert second["pages_to_copy"] == third["pages_to_copy"]
    # Unknown fields in the body are rejected without consuming anything; `{}` is fine.
    with pytest.raises(RuntimeError, match="zero_chunk_size"):
        vm.api.snapshot_dirty_pages.put(zero_chunk_size=4096)
    assert vm.dirty_pages() == third


def test_pause_invariant(uvm):
    """
    The correctness of copying after `snapshot/create` returned rests on nothing
    in Firecracker writing guest memory while paused. Inject tap traffic into a
    paused microVM and check that neither guest memory nor the net device's RX
    counters change.
    """
    vm = boot_with_mem_backend(uvm)
    vm.pause()

    before = vm.snapshot_full(mem_path="mem_before", vmstate_path="vmstate_before")
    vm.flush_metrics()

    # Every ssh connection attempt sends frames through the tap; while paused, nobody reads
    # them.
    for _ in range(3):
        with pytest.raises(TimeoutExpired):
            vm.ssh.check_output("true", timeout=1)

    net = vm.flush_metrics()["net"]
    assert net["rx_tap_event_count"] == 0
    assert net["rx_bytes_count"] == 0
    assert net["rx_packets_count"] == 0

    after = vm.snapshot_full(mem_path="mem_after", vmstate_path="vmstate_after")
    assert filecmp.cmp(before.mem, after.mem, shallow=False)

    vm.resume()
    vm.ssh.check_output("true")


def test_cross_check_with_firecracker_snapshot(uvm, microvm_factory):
    """
    Restore one microVM with a memory backend and one without from the same
    base snapshot, keep both paused, and compare a backend-made `Diff` rebased
    onto the base, and a `Full` written by Firecracker through the backend
    (which faults in every page the backend has not populated), with a plain
    Firecracker-made `Full`. The guest did not run in either, so this checks
    layout and `total_size` end to end.
    """
    basevm = uvm
    basevm.spawn()
    basevm.basic_config(vcpu_count=2, mem_size_mib=MEM_SIZE_MIB)
    basevm.add_net_iface()
    basevm.start()
    make_guest_dirty_memory(basevm.ssh, amount_mib=32)
    base = basevm.snapshot_full()
    basevm.kill()

    with_backend = microvm_factory.build()
    with_backend.memory_monitor = None
    with_backend.spawn()
    with_backend.restore_from_snapshot(
        base, resume=False, uffd_handler_name="on_demand", mem_backend=True
    )
    assert with_backend.mem_backend is not None
    backend_diff = with_backend.snapshot_diff(mem_path="mem_backend_diff")
    assert backend_diff.mem.stat().st_size == MEM_SIZE_MIB * 2**20
    backend_snap = with_backend.snapshot_full(
        mem_path="mem_backend", vmstate_path="vmstate_full"
    )
    assert backend_snap.mem.stat().st_size == MEM_SIZE_MIB * 2**20
    # Rebasing modifies the base in place; keep the original for the comparisons below.
    base_copy = Path(with_backend.chroot()) / "mem_base_copy"
    shutil.copy(base.mem, base_copy)
    build_tools.run_rebase_snap_bin(base_copy, backend_diff.mem)
    assert filecmp.cmp(base_copy, backend_snap.mem, shallow=False)

    plain = microvm_factory.build()
    plain.spawn()
    plain.restore_from_snapshot(base, resume=False)
    plain_snap = plain.snapshot_full(mem_path="mem_plain")

    # Restoring is not entirely side-effect free on guest memory: KVM writes the kvmclock
    # pages (wall clock, per-vCPU time info) when their MSRs are set, with the current time.
    # Those few pages differ between any two restores, and are the only ones allowed to.
    written_at_restore = set(differing_pages(plain_snap.mem, base.mem))
    assert len(written_at_restore) <= 8, written_at_restore
    assert set(differing_pages(backend_snap.mem, base.mem)) <= written_at_restore
    assert set(differing_pages(backend_snap.mem, plain_snap.mem)) <= written_at_restore

    # Both microVMs still work.
    with_backend.resume()
    with_backend.ssh.check_output("true")
    plain.resume()
    plain.ssh.check_output("true")


@pytest.mark.parametrize("huge_pages", PAGE_CONFIGS)
def test_restore_with_backend_snapshot_after_running(uvm, microvm_factory, huge_pages):
    """
    Restore with a memory backend, run a workload, take a diff through the
    backend, rebase it onto the base (a mix of faulted-in pages from the memfd
    and untouched pages of the base) and restore that with `File` (4K only),
    `Uffd` and `SharedMemfd`. The rebased diff equals Firecracker's own Full.
    """
    basevm = uvm
    basevm.spawn()
    basevm.memory_monitor = None
    basevm.basic_config(vcpu_count=2, mem_size_mib=MEM_SIZE_MIB, huge_pages=huge_pages)
    basevm.add_net_iface()
    basevm.start()
    base = basevm.snapshot_full()
    basevm.kill()

    vm = microvm_factory.build_from_snapshot(
        base, uffd_handler_name="on_demand", mem_backend=True
    )
    vm.memory_monitor = None
    vm.ssh.check_output("echo hello > /tmp/marker")
    make_guest_dirty_memory(vm.ssh, amount_mib=32)
    diff = vm.snapshot_diff(mem_path="mem_from_backend")
    full = vm.snapshot_full(mem_path="mem_full", vmstate_path="vmstate_full")
    vm.kill()
    snapshot = diff.rebase_snapshot(base)
    assert filecmp.cmp(snapshot.mem, full.mem, shallow=False)

    variants = [
        {"uffd_handler_name": "on_demand"},
        {"uffd_handler_name": "on_demand", "mem_backend": True},
    ]
    if huge_pages == HugePagesConfig.NONE:
        variants.insert(0, {})
    for kwargs in variants:
        restored = microvm_factory.build_from_snapshot(snapshot, **kwargs)
        restored.memory_monitor = None
        assert restored.ssh.check_output("cat /tmp/marker").stdout.strip() == "hello"
        if restored.mem_backend is not None:
            # A restored backend instance snapshots correctly in its new mode, too.
            again = restored.snapshot_diff(mem_path="mem_again")
            assert again.mem.stat().st_size == MEM_SIZE_MIB * 2**20
            check_layout(restored.last_snapshot_memory, MEM_SIZE_MIB * 2**20)
        restored.kill()


def test_boot_snapshot_restores_with_uffd(uvm, microvm_factory):
    """A snapshot produced at boot time by a backend restores through a plain UFFD handler."""
    vm = boot_with_mem_backend(uvm)
    vm.ssh.check_output("echo hello > /tmp/marker")
    snapshot = vm.snapshot_full()
    vm.kill()

    restored = microvm_factory.build_from_snapshot(
        snapshot, uffd_handler_name="on_demand"
    )
    restored.memory_monitor = None
    assert restored.ssh.check_output("cat /tmp/marker").stdout.strip() == "hello"
    # A plain UFFD restore attaches no backend.
    assert restored.mem_backend is None
    with pytest.raises(RuntimeError, match="No memory backend"):
        restored.api.snapshot_dirty_pages.put()
    restored.kill()


def test_fault_all_handler_as_backend(uvm, microvm_factory):
    """The other example handler works as a memory backend as well."""
    vm = boot_with_mem_backend(uvm, handler="fault_all")
    make_guest_dirty_memory(vm.ssh, amount_mib=16)
    snapshot = vm.snapshot_full()
    vm.kill()

    restored = microvm_factory.build_from_snapshot(
        snapshot, uffd_handler_name="fault_all", mem_backend=True
    )
    restored.memory_monitor = None
    restored.ssh.check_output("true")
    again = restored.snapshot_full(mem_path="mem_again")
    assert again.mem.stat().st_size == MEM_SIZE_MIB * 2**20
    restored.kill()


@pytest.mark.parametrize("huge_pages", PAGE_CONFIGS)
def test_prepare_then_load(uvm, microvm_factory, huge_pages):
    """`PUT /snapshot/prepare` ahead of `PUT /snapshot/load`: the restored microVM works,
    keeps its backend, and the diff it produces is the same as without preparation."""
    vm = boot_with_mem_backend(uvm, huge_pages=huge_pages)
    make_guest_dirty_memory(vm.ssh, amount_mib=32)
    vm.ssh.check_output("echo hello > /tmp/marker")
    snapshot = vm.snapshot_diff()
    vm.kill()

    restored = microvm_factory.build()
    restored.memory_monitor = None
    restored.spawn()
    restored.restore_from_snapshot(
        snapshot,
        resume=True,
        uffd_handler_name="on_demand",
        mem_backend=True,
        huge_pages=huge_pages,
        prepare=True,
    )
    assert restored.ssh.check_output("cat /tmp/marker").stdout.strip() == "hello"
    assert restored.mem_backend is not None
    # The prepared memory is the backend's: a diff from it rebased onto the snapshot is the
    # guest's memory, and restores.
    make_guest_dirty_memory(restored.ssh, amount_mib=16)
    restored.ssh.check_output("echo again > /tmp/marker")
    diff = restored.snapshot_diff(mem_path="mem_diff")
    check_layout(restored.last_snapshot_memory, MEM_SIZE_MIB * 2**20)
    full = restored.snapshot_full(mem_path="mem_full", vmstate_path="vmstate_full")
    restored.kill()
    rebased = diff.rebase_snapshot(snapshot)
    assert filecmp.cmp(rebased.mem, full.mem, shallow=False)

    chained = microvm_factory.build_from_snapshot(rebased, **restore_kwargs(huge_pages))
    chained.memory_monitor = None
    assert chained.ssh.check_output("cat /tmp/marker").stdout.strip() == "again"
    chained.kill()


def test_prepare_then_load_with_hotplug(uvm, microvm_factory):
    """A prepared VM with a hotpluggable region: the load plugs the slots the snapshot has
    plugged and leaves the rest unplugged, and the hotplug region keeps working."""
    vm = uvm
    vm.memory_monitor = None
    vm.spawn()
    vm.basic_config(
        vcpu_count=2,
        mem_size_mib=MEM_SIZE_MIB,
        track_dirty_pages=True,
        boot_args=MEMHP_BOOTARGS,
        mem_backend="on_demand",
    )
    hotplug = {"total_size_mib": 512, "slot_size_mib": 128, "block_size_mib": 2}
    vm.api.memory_hotplug.put(**hotplug)
    vm.add_net_iface()
    vm.start()
    vm.hotplug_memory(128)
    vm.ssh.check_output("mount -o remount,size=200M -t tmpfs tmpfs /dev/shm")
    vm.ssh.check_output("dd if=/dev/urandom of=/dev/shm/data bs=1M count=150")
    checksum = vm.ssh.check_output("md5sum /dev/shm/data").stdout
    snapshot = vm.snapshot_full()
    vm.kill()

    restored = microvm_factory.build()
    restored.memory_monitor = None
    restored.spawn()
    restored.restore_from_snapshot(
        snapshot,
        resume=True,
        uffd_handler_name="on_demand",
        mem_backend=True,
        prepare=True,
        memory_hotplug=hotplug,
    )
    assert restored.api.memory_hotplug.get().json()["plugged_size_mib"] == 128
    assert restored.ssh.check_output("md5sum /dev/shm/data").stdout == checksum
    # The unplugged slots are reported zero, the plugged one is not.
    restored.pause()
    memory = restored.dirty_pages()
    total_size = (MEM_SIZE_MIB + 512) * 2**20
    check_layout(memory, total_size)
    assert class_bytes_in(
        memory, (MEM_SIZE_MIB + 128) * 2**20, (512 - 128) * 2**20
    ) == (0, (512 - 128) * 2**20)
    restored.resume()
    restored.hotplug_memory(384)
    restored.ssh.check_output("mount -o remount,size=400M -t tmpfs tmpfs /dev/shm")
    restored.ssh.check_output("dd if=/dev/urandom of=/dev/shm/more bs=1M count=200")
    restored.ssh.check_output("rm /dev/shm/more /dev/shm/data; sync")
    restored.hotplug_memory(0)
    restored.ssh.check_output("true")
    restored.kill()


def test_prepare_negative_api(uvm, microvm_factory, guest_kernel, rootfs):
    """What `PUT /snapshot/prepare` and the load after it reject."""
    vm = boot_with_mem_backend(uvm)
    snapshot = vm.snapshot_full()
    vm.kill()

    # A prepared load must match the snapshot. A mismatch is rejected without harm; since the
    # configuration is frozen, the process is then only good for a snapshot that does match.
    dest = microvm_factory.build(guest_kernel, rootfs)
    dest.memory_monitor = None
    dest.spawn()
    jailed = snapshot.copy_to_chroot(Path(dest.chroot()))
    dest.uffd_handler = spawn_pf_handler(
        dest,
        uffd_handler("on_demand", binary_dir=dest.fc_binary_path.parent),
        jailed,
        mem_backend=True,
    )
    backend = {
        "backend_type": "SharedMemfd",
        "backend_path": str(dest.uffd_handler.socket_path),
    }
    vmstate = f"/{jailed.vmstate.name}"
    # Without a backend and without preparation the load has nothing to work with.
    with pytest.raises(RuntimeError, match="mem_backend is required"):
        dest.api.snapshot_load.put(snapshot_path=vmstate)
    # machine-config alone forbids a plain load, as before.
    dest.api.machine_config.put(vcpu_count=1, mem_size_mib=MEM_SIZE_MIB)
    with pytest.raises(RuntimeError, match="not allowed"):
        dest.api.snapshot_load.put(mem_backend=backend, snapshot_path=vmstate)
    # A File backend needs the memory file to exist at full size when preparing.
    with pytest.raises(RuntimeError, match="Cannot open the memory file"):
        dest.api.snapshot_prepare.put(
            mem_backend={"backend_type": "File", "backend_path": "/missing"}
        )
    Path(dest.chroot(), "short").write_bytes(b"\0" * 4096)
    with pytest.raises(RuntimeError, match="4096 bytes"):
        dest.api.snapshot_prepare.put(
            mem_backend={"backend_type": "File", "backend_path": "/short"}
        )
    dest.api.snapshot_prepare.put(mem_backend=backend)
    with pytest.raises(RuntimeError, match="already prepared"):
        dest.api.snapshot_prepare.put(mem_backend=backend)
    with pytest.raises(RuntimeError, match="cannot change"):
        dest.api.machine_config.put(vcpu_count=2, mem_size_mib=MEM_SIZE_MIB)
    with pytest.raises(RuntimeError, match="fixed by"):
        dest.api.snapshot_load.put(mem_backend=backend, snapshot_path=vmstate)
    with pytest.raises(RuntimeError, match="fixed by"):
        dest.api.snapshot_load.put(snapshot_path=vmstate, track_dirty_pages=True)
    with pytest.raises(RuntimeError, match="fixed by"):
        dest.api.snapshot_load.put(snapshot_path=vmstate, huge_pages="2M")
    # The snapshot has two vCPUs: the prepared VM does not match.
    with pytest.raises(RuntimeError, match="2 vCPUs, the prepared VM 1"):
        dest.api.snapshot_load.put(snapshot_path=vmstate)
    with pytest.raises(RuntimeError, match="already prepared"):
        dest.api.actions.put(action_type="InstanceStart")
    dest.kill()

    # A boot-specific resource forbids preparing, as it forbids loading.
    dest = microvm_factory.build(guest_kernel, rootfs)
    dest.spawn()
    dest.basic_config(vcpu_count=2, mem_size_mib=MEM_SIZE_MIB)
    with pytest.raises(RuntimeError, match="not allowed"):
        dest.api.snapshot_prepare.put(
            mem_backend={"backend_type": "Uffd", "backend_path": "/x"}
        )
    dest.kill()

    # Hugetlbfs memory cannot be restored from a file, prepared or not.
    dest = microvm_factory.build(guest_kernel, rootfs)
    dest.spawn()
    dest.api.machine_config.put(
        vcpu_count=2,
        mem_size_mib=MEM_SIZE_MIB,
        huge_pages=HugePagesConfig.HUGETLBFS_2MB,
    )
    with pytest.raises(RuntimeError, match="hugetlbfs"):
        dest.api.snapshot_prepare.put(
            mem_backend={"backend_type": "File", "backend_path": "/mem"}
        )
    dest.kill()


def test_prepare_then_load_from_file(uvm, microvm_factory):
    """`File` can be prepared as well: the (pre-sized) memory file is mapped at prepare
    time and read on fault, so filling it between the prepare and the load, as an
    orchestrator would during the pause, gives the guest the snapshot's memory."""
    vm = boot_with_mem_backend(uvm)
    make_guest_dirty_memory(vm.ssh, amount_mib=32)
    vm.ssh.check_output("echo hello > /tmp/marker")
    snapshot = vm.snapshot_full()
    vm.kill()

    dest = microvm_factory.build()
    dest.memory_monitor = None
    dest.spawn()
    jailed = snapshot.copy_to_chroot(Path(dest.chroot()))
    for disk in jailed.disks.values():
        dest.create_jailed_resource(disk)
    dest.disks = jailed.disks
    dest.ssh_key = jailed.ssh_key
    for iface in jailed.net_ifaces:
        dest.add_net_iface(iface, api=False)
    # An empty file of the right size stands in for the memory until the "pause".
    mem = Path(dest.chroot()) / "mem_late"
    mem.touch()
    os.truncate(mem, MEM_SIZE_MIB * 2**20)
    dest.api.machine_config.put(vcpu_count=2, mem_size_mib=MEM_SIZE_MIB)
    dest.api.snapshot_prepare.put(
        mem_backend={"backend_type": "File", "backend_path": "/mem_late"}
    )
    # The "pause": the snapshot's memory lands in the mapped file, written in place.
    with open(jailed.mem, "rb") as src, open(mem, "r+b") as dst:
        shutil.copyfileobj(src, dst)
    dest.api.snapshot_load.put(snapshot_path=f"/{jailed.vmstate.name}", resume_vm=True)
    assert dest.ssh.check_output("cat /tmp/marker").stdout.strip() == "hello"
    dest.kill()


def test_virtio_mem_unplugged_slots(uvm, microvm_factory):
    """
    With virtio-mem, `total_size` includes the hotplug region; unplugged slots
    are zero pages, and an unplugged region of any size costs a few bytes per
    256 MiB on the wire.
    """
    vm = uvm
    vm.memory_monitor = None
    vm.spawn()
    vm.basic_config(
        vcpu_count=2,
        mem_size_mib=MEM_SIZE_MIB,
        track_dirty_pages=True,
        boot_args=MEMHP_BOOTARGS,
        mem_backend="on_demand",
    )
    vm.api.memory_hotplug.put(total_size_mib=512, slot_size_mib=128, block_size_mib=2)
    vm.add_net_iface()
    vm.start()

    total_size = (MEM_SIZE_MIB + 512) * 2**20

    # Nothing plugged yet: the whole hotplug region is unplugged. It is dirty and never
    # resident, so zero: 512 MiB is 131072 pages, two 65536-page Roaring containers holding
    # one run each, 25 bytes on the wire (4 cookie, 1 run flags, 2 x 4 key/cardinality, 2 x 6
    # run). In the file it is zero. (A diff of a booted microVM
    # into a fresh file is a complete memory file.)
    snapshot = vm.snapshot_diff(mem_path="mem_unplugged")
    memory = vm.last_snapshot_memory
    check_layout(memory, total_size)
    assert wire_bytes(memory, "pages_to_discard") == 25
    assert wire_bytes(memory, "pages_to_copy") < MEM_SIZE_MIB * 2**20 // PAGE_SIZE // 8
    authoritative, zero = class_bytes_in(memory, MEM_SIZE_MIB * 2**20, 512 * 2**20)
    assert authoritative == 0
    assert zero == 512 * 2**20
    assert snapshot.mem.stat().st_size == total_size
    with open(snapshot.mem, "rb") as mem:
        mem.seek(MEM_SIZE_MIB * 2**20)
        assert mem.read(512 * 2**20) == bytes(512 * 2**20)

    # A `Diff` right away: the hotplug region is still all zero pages (the unplug state is
    # reported every time), nothing dirty in DRAM but the virtqueue pages. Merge it into the
    # file so that nothing consumed goes missing from it.
    vm.resume()
    vm.pause()
    memory = vm.dirty_pages(copy_to="mem_unplugged")
    check_layout(memory, total_size)
    assert authoritative_bytes(memory) < 16 * 2**20
    assert discard_bytes(memory) == 512 * 2**20

    # Plug one slot's worth and make the guest use it: what the guest touched in that slot is
    # authoritative, the rest of it unchanged (zero since boot), the unplugged rest zero pages.
    vm.resume()
    vm.api.memory_hotplug.patch(requested_size_mib=128)
    make_guest_dirty_memory(vm.ssh, amount_mib=64)
    # Merge the diff into the first file to get a complete memory file to restore from, and
    # check it against Firecracker's own Full of the same state.
    vm.snapshot_diff(mem_path="mem_unplugged", vmstate_path="vmstate_plugged")
    memory = vm.last_snapshot_memory
    check_layout(memory, total_size)
    authoritative, zero = class_bytes_in(
        memory, (MEM_SIZE_MIB + 128) * 2**20, (512 - 128) * 2**20
    )
    assert (authoritative, zero) == (0, (512 - 128) * 2**20)
    authoritative, zero = class_bytes_in(memory, MEM_SIZE_MIB * 2**20, 128 * 2**20)
    assert authoritative > 0
    assert zero == 0
    snapshot = vm.snapshot_full(mem_path="mem_plugged", vmstate_path="vmstate_plugged")
    assert not differing_pages(snapshot.mem, snapshot.mem.parent / "mem_unplugged")
    vm.kill()

    restored = microvm_factory.build_from_snapshot(snapshot)
    restored.memory_monitor = None
    restored.ssh.check_output("true")
    status = restored.api.memory_hotplug.get().json()
    assert status["plugged_size_mib"] == 128
    restored.kill()


def test_virtio_mem_unplug_after_use(uvm, microvm_factory):
    """
    Unplug memory the guest has used, and merge a `Diff` taken afterwards into
    the base that still holds the old bytes. The unplugged slots are zero
    pages, so the merged file equals a fresh `Full`; a restored VM can plug the
    slots back and use them.
    """
    vm = uvm
    vm.memory_monitor = None
    vm.spawn()
    vm.basic_config(
        vcpu_count=2,
        mem_size_mib=MEM_SIZE_MIB,
        track_dirty_pages=True,
        boot_args=MEMHP_BOOTARGS,
        mem_backend="on_demand",
    )
    vm.api.memory_hotplug.put(total_size_mib=512, slot_size_mib=128, block_size_mib=2)
    vm.add_net_iface()
    vm.start()
    total_size = (MEM_SIZE_MIB + 512) * 2**20

    # Plug two slots and put data in the guest's memory, most of which can only
    # fit in the hotplugged part.
    vm.hotplug_memory(256)
    vm.ssh.check_output("mount -o remount,size=300M -t tmpfs tmpfs /dev/shm")
    vm.ssh.check_output("dd if=/dev/urandom of=/dev/shm/data bs=1M count=200")
    vm.pause()
    assert class_bytes_in(
        vm.dirty_pages(), (MEM_SIZE_MIB + 256) * 2**20, 256 * 2**20
    ) == (0, 256 * 2**20)
    base = vm.snapshot_full(mem_path="mem_base")
    vm.resume()

    # Unplug one slot's worth. The guest migrates the data away first, so the
    # file survives and the slot's old bytes are still in `base`.
    vm.ssh.check_output("rm /dev/shm/data; sync")
    vm.hotplug_memory(128)

    # Merge a diff straight into a copy of the base, as a backend would.
    merged = Path(vm.chroot()) / "mem_merged"
    shutil.copy(base.mem, merged)
    diff = vm.snapshot_diff(mem_path="mem_merged", vmstate_path="vmstate_diff")
    diff_memory = vm.last_snapshot_memory
    check_layout(diff_memory, total_size)
    # The unplugged slots (the one just unplugged, with its old bytes still in `base`, and the
    # never-plugged ones) are zero pages.
    assert class_bytes_in(
        diff_memory, (MEM_SIZE_MIB + 128) * 2**20, (512 - 128) * 2**20
    ) == (0, (512 - 128) * 2**20)
    full = vm.snapshot_full(mem_path="mem_full", vmstate_path="vmstate_full")
    assert filecmp.cmp(diff.mem, full.mem, shallow=False)
    # The unplugged slots are zero in the file, whatever the base had there.
    with open(full.mem, "rb") as mem:
        mem.seek((MEM_SIZE_MIB + 128) * 2**20)
        assert mem.read((512 - 128) * 2**20) == bytes((512 - 128) * 2**20)

    # Unplug the remaining slot and plug it back with no API call in between. It is plugged at
    # the time of the call and dirty from the unplug; whatever the guest has not touched again
    # is a hole, so zero, the rest authoritative. Together they cover the slot.
    vm.resume()
    vm.hotplug_memory(0)
    vm.hotplug_memory(128)
    vm.pause()
    replugged = vm.dirty_pages()
    check_layout(replugged, total_size)
    authoritative, zero = class_bytes_in(replugged, MEM_SIZE_MIB * 2**20, 128 * 2**20)
    assert authoritative + zero == 128 * 2**20, (authoritative, zero)
    assert class_bytes_in(
        replugged, (MEM_SIZE_MIB + 128) * 2**20, (512 - 128) * 2**20
    ) == (0, (512 - 128) * 2**20)
    vm.kill()

    # Restore, plug everything back and use it.
    restored = microvm_factory.build_from_snapshot(full)
    restored.memory_monitor = None
    assert restored.api.memory_hotplug.get().json()["plugged_size_mib"] == 128
    restored.hotplug_memory(512)
    restored.ssh.check_output("mount -o remount,size=600M -t tmpfs tmpfs /dev/shm")
    restored.ssh.check_output("dd if=/dev/urandom of=/dev/shm/data bs=1M count=400")
    restored.kill()


def inflate_balloon(vm, amount_mib):
    """Inflate the balloon and wait for the guest to have released the pages."""
    vm.api.balloon.patch(amount_mib=amount_mib)
    wait_for_balloon_actual(vm, amount_mib, timeout_s=30)


def check_diff_identity_across_inflate(vm, base, amount_mib, min_zeroed_mib):
    """Inflate the balloon (guest is running), then take a `Diff` and a `Full`
    and check that rebasing the diff onto `base` yields the full copy.

    Balloon inflation punches holes into the shared memfd. Those pages are dirty
    (Firecracker marks discarded ranges) and not resident, so they are zero
    pages: the backend zeroes at least `min_zeroed_mib` worth without reading
    guest memory, and the content in both copies is zero. With 2M pages only a
    huge page a single release covers entirely is punched; the rest of each
    release is zero-written instead (dirty, resident, zeros: authoritative). The
    Linux balloon releases at most 1 MiB at a time, so nothing is punched,
    `min_zeroed_mib` is 0, and every released page is still in the dirty set.
    """
    inflate_balloon(vm, amount_mib)
    diff = vm.snapshot_diff(mem_path="mem_diff")
    diff_memory = vm.last_snapshot_memory
    check_layout(diff_memory, MEM_SIZE_MIB * 2**20)
    assert (
        authoritative_bytes(diff_memory) + discard_bytes(diff_memory)
        >= amount_mib * 2**20
    )
    zeroed = zeroed_bytes(vm)
    assert zeroed >= min_zeroed_mib * 2**20, zeroed
    assert zeroed == discard_bytes(diff_memory)
    # The Full right after is Firecracker's own: released pages are unregistered from the uffd
    # and read as zero through its mapping, whatever the base has there.
    full = vm.snapshot_full(mem_path="mem_full", vmstate_path="vmstate_full")
    rebased = diff.rebase_snapshot(base)
    assert filecmp.cmp(rebased.mem, full.mem, shallow=False)


def test_balloon_inflate_at_boot(uvm):
    """
    Boot with a memory backend and a balloon: the pages the guest releases are
    punched out of the memfd, reported in the next diff, and the diff rebased
    onto the base equals a full copy.
    """
    vm = boot_with_mem_backend(uvm, balloon=True)
    vm.memory_monitor = None
    make_guest_dirty_memory(vm.ssh, amount_mib=64)
    base = vm.snapshot_full(mem_path="mem_base")
    vm.resume()

    # 128 MiB of a 256 MiB guest with 64 MiB dirtied: most of the released memory is
    # 4K pages, all of which are punched out.
    check_diff_identity_across_inflate(vm, base, amount_mib=128, min_zeroed_mib=96)

    # The guest keeps working, and can get its memory back.
    vm.resume()
    inflate_balloon(vm, 0)
    make_guest_dirty_memory(vm.ssh, amount_mib=64)


def test_balloon_inflate_reclaims_memory(uvm):
    """
    With guest memory shared through a memfd, inflating the balloon releases
    host memory: the RSS of Firecracker drops. (`madvise(MADV_DONTNEED)` on a
    shared mapping would not achieve that; the pages are punched out instead.)
    """
    vm = boot_with_mem_backend(uvm, balloon=True)
    vm.memory_monitor = None

    # Baseline at a deterministic balloon size, then give the memory back.
    inflate_balloon(vm, 128)
    init_rss = get_stable_rss_mem(vm)
    inflate_balloon(vm, 0)

    make_guest_dirty_memory(vm.ssh, amount_mib=64)
    dirty_rss = get_stable_rss_mem(vm)
    assert dirty_rss > init_rss + 32 * 1024, (init_rss, dirty_rss)

    inflate_balloon(vm, 128)
    inflated_rss = get_stable_rss_mem(vm)
    # The dirtied pages (and more) were handed to the balloon and released.
    assert inflated_rss < dirty_rss - 32 * 1024, (dirty_rss, inflated_rss)
    # Same as the very first baseline, give or take.
    assert abs(inflated_rss - init_rss) <= 20 * 1024, (init_rss, inflated_rss)


@pytest.mark.parametrize("huge_pages", PAGE_CONFIGS)
def test_balloon_inflate_after_restore(uvm, microvm_factory, huge_pages):
    """
    Restore with a memory backend and inflate the balloon. Firecracker punches
    the released pages out of the memfd through `madvise(MADV_REMOVE)`, so the
    handler receives a UFFD `remove` event for each range and knows that those
    pages now read as zero rather than as the snapshot file's content, whether
    it had populated them before or not. Snapshots taken through the backend
    must reflect that.

    With 2M pages the balloon still reports 4K ranges; only a huge page a single
    range covers entirely is punched out, the partial ones are zero-written
    through the mapping (which populates them), so released memory ends up
    resident zeros rather than holes.
    """
    basevm = uvm
    basevm.spawn()
    basevm.memory_monitor = None
    basevm.basic_config(
        vcpu_count=2,
        mem_size_mib=MEM_SIZE_MIB,
        track_dirty_pages=True,
        huge_pages=huge_pages,
    )
    basevm.add_net_iface()
    basevm.api.balloon.put(
        amount_mib=0, deflate_on_oom=True, stats_polling_interval_s=1
    )
    basevm.start()
    # Data the restored guest will release without ever touching it again.
    make_guest_dirty_memory(basevm.ssh, amount_mib=64)
    base = basevm.snapshot_full()
    basevm.kill()

    # Dirty page tracking must be on for diffs to record the discarded pages; a `mincore`
    # based diff only sees resident pages and cannot express "this page became zero".
    vm = microvm_factory.build_from_snapshot(
        base, uffd_handler_name="on_demand", mem_backend=True, track_dirty_pages=True
    )
    vm.memory_monitor = None
    # Touch some memory so that the memfd holds a mix of populated and
    # never-populated pages when the balloon takes them. The new base is the
    # restore base plus a backend diff: a Firecracker Full here would fault in
    # every page and make the whole memfd populated.
    restored_base = Snapshot(
        **(base.__dict__ | {"mem": Path(vm.chroot()) / "mem_base"})
    )
    shutil.copy(base.mem, restored_base.mem)
    vm.snapshot_diff(mem_path="mem_base")
    vm.resume()

    check_diff_identity_across_inflate(
        vm,
        restored_base,
        amount_mib=128,
        min_zeroed_mib=0 if huge_pages == HugePagesConfig.HUGETLBFS_2MB else 64,
    )

    # Deflate, reuse the memory, and make sure the result restores.
    vm.resume()
    inflate_balloon(vm, 0)
    make_guest_dirty_memory(vm.ssh, amount_mib=64)
    vm.ssh.check_output("echo after-balloon > /tmp/marker")
    final = vm.snapshot_full(mem_path="mem_final", vmstate_path="vmstate_final")
    vm.kill()

    restored = microvm_factory.build_from_snapshot(final, **restore_kwargs(huge_pages))
    restored.memory_monitor = None
    assert (
        restored.ssh.check_output("cat /tmp/marker").stdout.strip() == "after-balloon"
    )
    restored.kill()


def test_diff_mincore_self_consistency(uvm):
    """
    Without `track_dirty_pages`, a `Diff` is computed with `mincore` and
    contains every resident page. Rebasing it onto the base must still yield
    the full copy taken right after.
    """
    vm = boot_with_mem_backend(uvm, track_dirty_pages=False)
    make_guest_dirty_memory(vm.ssh, amount_mib=32)
    base = vm.snapshot_full(mem_path="mem_base")
    vm.resume()
    make_guest_dirty_memory(vm.ssh, amount_mib=64)

    diff = vm.make_snapshot(SnapshotType.DIFF_MINCORE, mem_path="mem_diff")
    diff_memory = vm.last_snapshot_memory
    check_layout(diff_memory, MEM_SIZE_MIB * 2**20)
    # With mincore as the dirty source, dirty ⇒ resident: nothing can be zero.
    assert authoritative_bytes(diff_memory) >= 64 * 2**20
    assert discard_bytes(diff_memory) == 0

    full = vm.snapshot_full(mem_path="mem_full", vmstate_path="vmstate_full")
    rebased = diff.rebase_snapshot(base)
    assert filecmp.cmp(rebased.mem, full.mem, shallow=False)


@pytest.mark.skipif(
    platform.machine() != "x86_64", reason="two DRAM regions need the x86 MMIO gap"
)
def test_two_dram_regions(uvm, microvm_factory):
    """
    More than 4 GiB of guest memory on x86 spans two DRAM regions around the
    32-bit MMIO gap. They are contiguous in the memfd, `total_size` is their
    sum, and the result restores.
    """
    mem_size_mib = 4608
    vm = boot_with_mem_backend(uvm, mem_size_mib=mem_size_mib)
    vm.memory_monitor = None
    make_guest_dirty_memory(vm.ssh, amount_mib=32)

    snapshot = vm.snapshot_diff()
    memory = vm.last_snapshot_memory
    check_layout(memory, mem_size_mib * 2**20)
    # Both regions are plugged and adjacent in file space; the second region (past the 32-bit
    # gap, from 3 GiB of file offset on) has authoritative pages since the guest touched some
    # of it, and nothing was discarded so nothing is zero.
    assert discard_bytes(memory) == 0
    authoritative, _ = class_bytes_in(memory, 3 * 2**30, (mem_size_mib - 3072) * 2**20)
    assert authoritative > 0
    assert snapshot.mem.stat().st_size == mem_size_mib * 2**20
    vm.kill()

    restored = microvm_factory.build_from_snapshot(snapshot)
    restored.memory_monitor = None
    restored.ssh.check_output("true")
    restored.kill()


@pytest.mark.parametrize("snapshot_type", [SnapshotType.FULL, SnapshotType.DIFF])
def test_snapshot_right_after_restore(uvm, microvm_factory, snapshot_type):
    """
    Snapshot a restored VM before it has run. Restoring writes to guest memory
    (VMGenID, kvmclock); those pages were populated through the handler during
    the load and must be in the copy, and dirty for a `Diff`.
    """
    basevm = uvm
    basevm.spawn()
    basevm.memory_monitor = None
    basevm.basic_config(vcpu_count=2, mem_size_mib=MEM_SIZE_MIB, track_dirty_pages=True)
    basevm.add_net_iface()
    basevm.start()
    make_guest_dirty_memory(basevm.ssh, amount_mib=32)
    base = basevm.snapshot_full()
    basevm.kill()

    vm = microvm_factory.build_from_snapshot(
        base,
        uffd_handler_name="on_demand",
        mem_backend=True,
        track_dirty_pages=True,
        resume=False,
    )
    vm.memory_monitor = None

    if snapshot_type == SnapshotType.DIFF:
        diff = vm.snapshot_diff(mem_path="mem_diff")
        memory = vm.last_snapshot_memory
        dirty = authoritative_bytes(memory) + discard_bytes(memory)
        # A handful of pages, not nothing and not everything; all of them were written (by the
        # restore) so all are resident: authoritative.
        assert 0 < dirty <= 2**20, dirty
        assert discard_bytes(memory) == 0
        full = vm.snapshot_full(mem_path="mem_full", vmstate_path="vmstate_full")
        assert filecmp.cmp(diff.rebase_snapshot(base).mem, full.mem, shallow=False)
    else:
        full = vm.snapshot_full(mem_path="mem_full", vmstate_path="vmstate_full")
        # Not identical to the base: the restore wrote to a few pages.
        assert not filecmp.cmp(base.mem, full.mem, shallow=False)
        differing = 0
        with open(base.mem, "rb") as a, open(full.mem, "rb") as b:
            for _ in range(MEM_SIZE_MIB * 2**20 // 4096):
                if a.read(4096) != b.read(4096):
                    differing += 1
        assert 0 < differing <= 256, differing
    vm.kill()

    restored = microvm_factory.build_from_snapshot(full)
    restored.memory_monitor = None
    restored.ssh.check_output("true")
    restored.kill()


def test_negative_api(uvm, microvm_factory, guest_kernel, rootfs):
    """Requests that are invalid with, or without, a memory backend."""
    # machine-config.mem_backend accepts only `SharedMemfd`.
    vm = uvm
    vm.spawn()
    for backend_type in ("File", "Uffd"):
        with pytest.raises(RuntimeError, match="SharedMemfd"):
            vm.api.machine_config.put(
                vcpu_count=2,
                mem_size_mib=MEM_SIZE_MIB,
                mem_backend={"backend_type": backend_type, "backend_path": "/x"},
            )

    # An unreachable backend fails the boot; Firecracker stays up in the pre-boot state.
    vm.basic_config(vcpu_count=2, mem_size_mib=MEM_SIZE_MIB)
    vm.api.machine_config.put(
        vcpu_count=2,
        mem_size_mib=MEM_SIZE_MIB,
        mem_backend={"backend_type": "SharedMemfd", "backend_path": "/missing"},
    )
    with pytest.raises(RuntimeError, match="memory backend"):
        vm.api.actions.put(action_type="InstanceStart")
    vm.kill()

    # Without a backend: mem_file_path stays mandatory, dirty-pages and Backend are rejected.
    plain = microvm_factory.build(guest_kernel, rootfs)
    plain.spawn()
    plain.basic_config(vcpu_count=2, mem_size_mib=MEM_SIZE_MIB, track_dirty_pages=True)
    plain.add_net_iface()
    plain.start()
    with pytest.raises(RuntimeError, match="No memory backend"):
        plain.api.snapshot_dirty_pages.put()
    plain.pause()
    for snapshot_type in ("Full", "Diff"):
        with pytest.raises(RuntimeError, match="mem_file_path"):
            plain.api.snapshot_create.put(
                snapshot_path="vmstate", snapshot_type=snapshot_type
            )
    with pytest.raises(RuntimeError, match="requires a memory backend"):
        plain.api.snapshot_create.put(snapshot_path="vmstate", snapshot_type="Backend")
    plain.resume()
    plain.ssh.check_output("true")
    plain.kill()

    # With a backend: `Full`/`Diff` on `snapshot/create` behave exactly as without one
    # (mem_file_path required). `Backend` is the backend path: mem_file_path must be absent,
    # unknown fields are rejected before anything is written or consumed, and a dead backend
    # does not fail it. `dirty-pages` is the pre-copy path.
    backed = boot_with_mem_backend(microvm_factory.build(guest_kernel, rootfs))
    backed.pause()
    for snapshot_type in ("Full", "Diff"):
        with pytest.raises(RuntimeError, match="mem_file_path"):
            backed.api.snapshot_create.put(
                snapshot_path="vmstate", snapshot_type=snapshot_type
            )
    with pytest.raises(RuntimeError, match="mem_file_path"):
        backed.api.snapshot_create.put(
            snapshot_path="vmstate", mem_file_path="mem", snapshot_type="Backend"
        )
    with pytest.raises(RuntimeError, match="zero_chunk_size"):
        backed.api.snapshot_create.put(
            snapshot_path="vmstate_bad", snapshot_type="Backend", zero_chunk_size=4096
        )
    with pytest.raises(RuntimeError, match="zero_chunk_size"):
        backed.api.snapshot_dirty_pages.put(zero_chunk_size=4096)
    assert not (Path(backed.chroot()) / "vmstate_bad").exists()
    backed.resume()
    backed.ssh.check_output("true")

    backed.mem_backend.kill()
    backed.ssh.check_output("true")
    backed.pause()
    memory = backed.api.snapshot_create.put(
        snapshot_path="vmstate", snapshot_type="Backend"
    ).json()["memory"]
    check_layout(memory, MEM_SIZE_MIB * 2**20)
    with pytest.raises((ConnectionError, FileNotFoundError, OSError)):
        backed.mem_backend.control({"Copy": {"mem_path": "/mem", "memory": memory}})
    backed.uffd_handler = None
    backed.resume()
    backed.ssh.check_output("true")


def test_create_with_pause(uvm, microvm_factory):
    """`snapshot/create` with `pause: true` pauses the microVM itself and leaves it paused;
    the snapshot restores; on a paused microVM the flag is a no-op."""
    vm = boot_with_mem_backend(uvm)
    vm.ssh.check_output("echo hello > /tmp/marker")
    assert vm.state == "Running"
    response = vm.api.snapshot_create.put(
        snapshot_path="vmstate", snapshot_type="Backend", pause=True
    )
    assert vm.state == "Paused"
    memory = response.json()["memory"]
    check_layout(memory, MEM_SIZE_MIB * 2**20)
    # The flag on a paused microVM is a no-op; the dirty state was consumed by the first call.
    again = vm.api.snapshot_create.put(
        snapshot_path="vmstate_again", snapshot_type="Backend", pause=True
    )
    assert authoritative_bytes(again.json()["memory"]) < 2 * 2**20
    assert vm.state == "Paused"
    vm.mem_backend.copy(memory, "/mem")
    vm.mem_backend.copy(again.json()["memory"], "/mem")
    vm.resume()
    assert vm.state == "Running"
    vm.ssh.check_output("true")

    snapshot = Snapshot(
        vmstate=Path(vm.chroot()) / "vmstate",
        mem=Path(vm.chroot()) / "mem",
        disks=vm.disks,
        net_ifaces=[x["iface"] for x in vm.iface.values()],
        ssh_key=vm.ssh_key,
        snapshot_type=SnapshotType.DIFF,
        meta={
            "kernel_file": str(vm.guest_kernel.vmlinux),
            "rootfs_file": str(vm.rootfs_file),
            "vcpus_count": vm.vcpus_count,
            "mem_size_mib": MEM_SIZE_MIB,
        },
    )
    vm.kill()
    restored = microvm_factory.build_from_snapshot(snapshot)
    restored.memory_monitor = None
    assert restored.ssh.check_output("cat /tmp/marker").stdout.strip() == "hello"
    restored.kill()


@pytest.mark.parametrize("snapshot_type", [SnapshotType.FULL, SnapshotType.DIFF])
def test_snapshot_types_with_backend(uvm, snapshot_type):
    """The file has the full size either way: a Full is written by Firecracker (204, no
    layout), a Diff by the backend from the layout of a `snapshot/create` with
    `snapshot_type: Backend` (200)."""
    vm = boot_with_mem_backend(uvm)
    snapshot = vm.make_snapshot(snapshot_type)
    assert snapshot.mem.stat().st_size == MEM_SIZE_MIB * 2**20
    if snapshot_type == SnapshotType.DIFF:
        assert vm.last_snapshot_memory["total_size"] == MEM_SIZE_MIB * 2**20
    else:
        assert vm.last_snapshot_memory is None
