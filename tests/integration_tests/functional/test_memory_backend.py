# Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Tests for the memory backend: sharing guest memory with a UFFD handler
through a memfd and producing memory snapshots from it, byte-for-byte identical
to the ones Firecracker writes itself.

The test framework plays the orchestrator: it calls `PUT /snapshot/create` (or
`PUT /snapshot/dirty-ranges`), receives the `memory` object and forwards it to
the example handler's control socket, which copies the ranges out of the memfd.
"""

import filecmp
import platform
import shutil
from pathlib import Path
from subprocess import TimeoutExpired

import pytest

from framework.artifacts import GUEST_KERNEL_DEFAULT, pin_guest_kernel
from framework.microvm import SnapshotType
from framework.utils import get_stable_rss_mem, make_guest_dirty_memory
from framework.utils_hugepages import HugePagesConfig
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


def check_layout(memory, total_size):
    """Sanity checks on a `memory` object returned by Firecracker."""
    assert memory["total_size"] == total_size
    for key in ("ranges", "unplugged"):
        ranges = memory[key]
        for rng in ranges:
            assert rng["len"] > 0
            assert rng["offset"] % 4096 == 0
            assert rng["len"] % 4096 == 0
            assert rng["offset"] + rng["len"] <= total_size
        # sorted and merged
        for prev, cur in zip(ranges, ranges[1:]):
            assert prev["offset"] + prev["len"] < cur["offset"]
    for rng in memory["ranges"]:
        for unplugged in memory["unplugged"]:
            assert (
                rng["offset"] + rng["len"] <= unplugged["offset"]
                or unplugged["offset"] + unplugged["len"] <= rng["offset"]
            )


def covered_bytes(ranges):
    """Total number of bytes covered by a range list."""
    return sum(rng["len"] for rng in ranges)


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
def test_boot_full_snapshot_restores(uvm, microvm_factory, huge_pages):
    """
    Boot with a memory backend, take a full snapshot through it and restore
    from the resulting memory file (with the plain `File` backend for 4K pages,
    through a UFFD handler for 2M pages).
    """
    vm = boot_with_mem_backend(uvm, huge_pages=huge_pages)
    make_guest_dirty_memory(vm.ssh, amount_mib=32)

    snapshot = vm.snapshot_full()
    memory = vm.last_snapshot_memory
    check_layout(memory, MEM_SIZE_MIB * 2**20)
    assert not memory["unplugged"]
    assert covered_bytes(memory["ranges"]) == MEM_SIZE_MIB * 2**20
    assert snapshot.mem.stat().st_size == MEM_SIZE_MIB * 2**20
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
    dirty_bytes = covered_bytes(diff_memory["ranges"])
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
def test_precopy_dirty_ranges(uvm, huge_pages):
    """
    Pre-copy: copy dirty ranges repeatedly while the guest runs, then pause
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
        memory = vm.dirty_ranges(copy_to="mem_precopy")
        check_layout(memory, MEM_SIZE_MIB * 2**20)
        assert memory["ranges"], "a running guest dirties something"

    # Final pass while paused, merged into the same file.
    vm.pause()
    memory = vm.api.snapshot_create.put(
        snapshot_path="vmstate", snapshot_type="Diff"
    ).json()["memory"]
    check_layout(memory, MEM_SIZE_MIB * 2**20)
    vm.mem_backend.copy(memory, "/mem_precopy")

    # Reference: everything, still paused.
    full = vm.snapshot_full(mem_path="mem_full", vmstate_path="vmstate_full")
    assert filecmp.cmp(precopy, full.mem, shallow=False)


def test_dirty_ranges_are_consumed(uvm):
    """Two back-to-back `dirty-ranges` calls on a paused guest: the second is empty."""
    vm = boot_with_mem_backend(uvm)
    make_guest_dirty_memory(vm.ssh, amount_mib=16)
    vm.pause()

    first = vm.dirty_ranges()
    assert covered_bytes(first["ranges"]) >= 16 * 2**20
    second = vm.dirty_ranges()
    # Only the virtqueue pages, re-marked after every reset so that they are part of the next
    # set, remain.
    assert covered_bytes(second["ranges"]) < 2**20
    assert covered_bytes(second["ranges"]) == covered_bytes(vm.dirty_ranges()["ranges"])


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
    base snapshot, keep both paused, and compare a backend-made `Full` with a
    Firecracker-made one. The guest did not run in either, so this checks
    layout and `total_size` end to end (including pages the backend never
    faulted in, which it has to source from the snapshot file).
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
    backend_snap = with_backend.snapshot_full(mem_path="mem_backend")
    assert backend_snap.mem.stat().st_size == MEM_SIZE_MIB * 2**20

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
    Restore with a memory backend, run a workload, snapshot through the backend
    (a mix of faulted-in pages from the memfd and untouched pages from the
    snapshot file) and restore that with `File` (4K only), `Uffd` and
    `SharedMemfd`.
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
    snapshot = vm.snapshot_full(mem_path="mem_from_backend")
    vm.kill()

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
            again = restored.snapshot_full(mem_path="mem_again")
            assert again.mem.stat().st_size == MEM_SIZE_MIB * 2**20
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
        restored.api.snapshot_dirty_ranges.put()
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


def test_virtio_mem_unplugged_slots(uvm, microvm_factory):
    """
    With virtio-mem, `total_size` includes the hotplug region and the unplugged
    slots are reported in `unplugged` and zeroed in the memory file.
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

    # Nothing plugged yet: the whole hotplug region is unplugged.
    snapshot = vm.snapshot_full(mem_path="mem_unplugged")
    memory = vm.last_snapshot_memory
    check_layout(memory, total_size)
    assert covered_bytes(memory["unplugged"]) == 512 * 2**20
    assert covered_bytes(memory["ranges"]) == MEM_SIZE_MIB * 2**20
    assert snapshot.mem.stat().st_size == total_size
    with open(snapshot.mem, "rb") as mem:
        mem.seek(MEM_SIZE_MIB * 2**20)
        assert mem.read(512 * 2**20) == bytes(512 * 2**20)

    # Plug one slot's worth and make the guest use it.
    vm.resume()
    vm.api.memory_hotplug.patch(requested_size_mib=128)
    make_guest_dirty_memory(vm.ssh, amount_mib=64)
    snapshot = vm.snapshot_full(mem_path="mem_plugged", vmstate_path="vmstate_plugged")
    memory = vm.last_snapshot_memory
    check_layout(memory, total_size)
    assert covered_bytes(memory["unplugged"]) == (512 - 128) * 2**20
    assert covered_bytes(memory["ranges"]) == (MEM_SIZE_MIB + 128) * 2**20
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
    the base that still holds the old bytes. `unplugged` tells the backend to
    zero those slots, so the merged file equals a fresh `Full`; a restored VM
    can plug the slots back and use them.
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
    base = vm.snapshot_full(mem_path="mem_base")
    assert covered_bytes(vm.last_snapshot_memory["unplugged"]) == 256 * 2**20
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
    assert covered_bytes(diff_memory["unplugged"]) == (512 - 128) * 2**20
    full = vm.snapshot_full(mem_path="mem_full", vmstate_path="vmstate_full")
    assert filecmp.cmp(diff.mem, full.mem, shallow=False)
    # The unplugged slots are zero in the file, whatever the base had there.
    with open(full.mem, "rb") as mem:
        mem.seek((MEM_SIZE_MIB + 128) * 2**20)
        assert mem.read((512 - 128) * 2**20) == bytes((512 - 128) * 2**20)
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


def check_diff_identity_across_inflate(vm, base, amount_mib):
    """Inflate the balloon (guest is running), then take a `Diff` and a `Full`
    and check that rebasing the diff onto `base` yields the full copy.

    Balloon inflation punches holes into the shared memfd. Those pages must be
    part of the diff (Firecracker marks discarded ranges dirty), and their
    content in both copies must be zero; a missing or stale page shows up as a
    mismatch.
    """
    inflate_balloon(vm, amount_mib)
    diff = vm.snapshot_diff(mem_path="mem_diff")
    diff_memory = vm.last_snapshot_memory
    check_layout(diff_memory, MEM_SIZE_MIB * 2**20)
    assert covered_bytes(diff_memory["ranges"]) >= amount_mib * 2**20
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

    check_diff_identity_across_inflate(vm, base, amount_mib=128)

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

    With 2M pages the balloon still reports 4K ranges; only huge pages the
    guest released entirely are punched out, and the handler has to round the
    `remove` ranges inward the same way.
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
    # never-populated pages when the balloon takes them.
    make_guest_dirty_memory(vm.ssh, amount_mib=32)
    restored_base = vm.snapshot_full(mem_path="mem_base")
    vm.resume()

    check_diff_identity_across_inflate(vm, restored_base, amount_mib=128)

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
    assert covered_bytes(diff_memory["ranges"]) >= 64 * 2**20

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

    snapshot = vm.snapshot_full()
    memory = vm.last_snapshot_memory
    check_layout(memory, mem_size_mib * 2**20)
    # Both regions are plugged and adjacent in file space: a single range.
    assert memory["ranges"] == [{"offset": 0, "len": mem_size_mib * 2**20}]
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
        dirty = covered_bytes(vm.last_snapshot_memory["ranges"])
        # A handful of pages, not nothing and not everything.
        assert 0 < dirty <= 2**20, dirty
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

    # Without a backend: mem_file_path stays mandatory, dirty-ranges is rejected.
    plain = microvm_factory.build(guest_kernel, rootfs)
    plain.spawn()
    plain.basic_config(vcpu_count=2, mem_size_mib=MEM_SIZE_MIB, track_dirty_pages=True)
    plain.add_net_iface()
    plain.start()
    with pytest.raises(RuntimeError, match="No memory backend"):
        plain.api.snapshot_dirty_ranges.put()
    plain.pause()
    with pytest.raises(RuntimeError, match="mem_file_path"):
        plain.api.snapshot_create.put(snapshot_path="vmstate", snapshot_type="Full")
    plain.resume()
    plain.ssh.check_output("true")
    plain.kill()

    # With a backend: mem_file_path must be absent; a dead backend does not fail the request.
    backed = boot_with_mem_backend(microvm_factory.build(guest_kernel, rootfs))
    backed.pause()
    with pytest.raises(RuntimeError, match="mem_file_path"):
        backed.api.snapshot_create.put(
            snapshot_path="vmstate", mem_file_path="mem", snapshot_type="Full"
        )
    backed.resume()
    backed.ssh.check_output("true")

    backed.mem_backend.kill()
    backed.ssh.check_output("true")
    backed.pause()
    memory = backed.api.snapshot_create.put(
        snapshot_path="vmstate", snapshot_type="Full"
    ).json()["memory"]
    check_layout(memory, MEM_SIZE_MIB * 2**20)
    with pytest.raises((ConnectionError, FileNotFoundError, OSError)):
        backed.mem_backend.control({"Copy": {"mem_path": "/mem", "memory": memory}})
    backed.uffd_handler = None
    backed.resume()
    backed.ssh.check_output("true")


@pytest.mark.parametrize("snapshot_type", [SnapshotType.FULL, SnapshotType.DIFF])
def test_snapshot_types_with_backend(uvm, snapshot_type):
    """`snapshot_type` is echoed in the response and the file has the full size either way."""
    vm = boot_with_mem_backend(uvm)
    snapshot = vm.make_snapshot(snapshot_type)
    assert snapshot.mem.stat().st_size == MEM_SIZE_MIB * 2**20
    assert vm.last_snapshot_memory["total_size"] == MEM_SIZE_MIB * 2**20
