# Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Tests for the memory backend: sharing guest memory with a UFFD handler
through a memfd and producing memory snapshots from it, byte-for-byte identical
to the ones Firecracker writes itself.

The test framework plays the orchestrator: it calls `PUT /snapshot/create`
(vmstate only) and then tells the example handler, on its control socket, to
copy. The handler asks Firecracker for the dirty pages on its own connection
(the one the handshake arrived on) and copies the set pages out of the memfd;
the framework never sees the bitmap, only the handler's `set_pages` count.
"""

import filecmp
import json
import platform
import shutil
import socket
import struct
import time
from pathlib import Path
from subprocess import TimeoutExpired

import pytest
from tenacity import retry, stop_after_delay, wait_fixed

from framework.artifacts import GUEST_KERNEL_DEFAULT, pin_guest_kernel
from framework.microvm import Snapshot, SnapshotType
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


PAGE_SIZE = 4096


def dirty_bytes(done):
    """Bytes Firecracker reported dirty for a backend copy, from its `Done` reply."""
    return done["set_pages"] * PAGE_SIZE


@retry(wait=wait_fixed(0.2), stop=stop_after_delay(10), reraise=True)
def wait_for_log_message(vm, message):
    """Wait for `message` to show up in Firecracker's log."""
    vm.check_log_message(message)


def assert_no_dirty_pages_endpoint(vm):
    """Dirty pages travel on the backend connection; there is no HTTP endpoint for them."""
    res = vm.api.session.request(
        "PUT", vm.api.endpoint + "/snapshot/dirty-pages", json={}
    )
    assert res.status_code == 400, res.text
    assert "Invalid request method and/or path" in res.json()["fault_message"]


def differing_pages(path_a, path_b, page_size=4096):
    """Offsets of the pages at which two memory files differ."""
    with open(path_a, "rb") as file_a, open(path_b, "rb") as file_b:
        pages = zip(
            iter(lambda: file_a.read(page_size), b""),
            iter(lambda: file_b.read(page_size), b""),
        )
        return [i * page_size for i, (a, b) in enumerate(pages) if a != b]


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
    # A full copy consumes the dirty state as well; a booted guest has dirtied plenty.
    assert dirty_bytes(vm.last_backend_copy) >= 32 * 2**20
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
    # The workload dirtied at least what it wrote, and not everything.
    assert 64 * 2**20 <= dirty_bytes(vm.last_backend_copy) < MEM_SIZE_MIB * 2**20
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
        done = vm.precopy_pass("mem_precopy")
        assert dirty_bytes(done) > 0, "a running guest dirties something"

    # Final pass while paused, after the vmstate is written, merged into the same file.
    vm.pause()
    vm.api.snapshot_create.put(snapshot_path="vmstate", snapshot_type="Diff")
    vm.backend_copy("mem_precopy", full=False)

    # Reference: everything, still paused.
    full = vm.snapshot_full(mem_path="mem_full", vmstate_path="vmstate_full")
    assert filecmp.cmp(precopy, full.mem, shallow=False)


def test_dirty_pages_are_consumed(uvm):
    """Two back-to-back `DirtyPages` requests on a paused guest: the second is (almost) empty."""
    vm = boot_with_mem_backend(uvm)
    make_guest_dirty_memory(vm.ssh, amount_mib=16)
    vm.pause()

    first = vm.precopy_pass("mem_a")
    assert dirty_bytes(first) >= 16 * 2**20
    second = vm.precopy_pass("mem_b")
    # Only the virtqueue pages, re-marked after every reset so that they are part of the next
    # set, remain.
    assert dirty_bytes(second) < 2**20
    assert dirty_bytes(second) == dirty_bytes(vm.precopy_pass("mem_c"))


def test_request_before_create_is_not_final(uvm):
    """
    A `DirtyPages` request between `Pause` and `snapshot/create` is legal but not
    the snapshot's set: `snapshot/create` may still write guest memory (block
    drain) and marks what it wrote dirty, so a request after it completes the
    snapshot. Rebasing both diffs onto the base must equal a full copy taken
    afterwards.
    """
    vm = boot_with_mem_backend(uvm)
    make_guest_dirty_memory(vm.ssh, amount_mib=16)
    base = vm.snapshot_full(mem_path="mem_base")
    vm.resume()
    make_guest_dirty_memory(vm.ssh, amount_mib=16)
    vm.pause()

    # Too early: consumes what the guest dirtied, but not what `snapshot/create` will touch.
    early = vm.backend_copy("mem_early", full=False)
    assert dirty_bytes(early) >= 16 * 2**20
    vm.api.snapshot_create.put(snapshot_path="vmstate", snapshot_type="Diff")
    late = vm.backend_copy("mem_late", full=False)
    assert dirty_bytes(late) > 0
    full = vm.snapshot_full(mem_path="mem_full", vmstate_path="vmstate_full")

    # base <- early <- late must equal the full copy.
    root = Path(vm.chroot())

    def diff_snapshot(mem_path):
        return Snapshot(
            vmstate=root / "vmstate",
            mem=root / mem_path,
            disks=base.disks,
            net_ifaces=base.net_ifaces,
            ssh_key=base.ssh_key,
            snapshot_type=SnapshotType.DIFF,
            meta=base.meta,
        )

    rebased = diff_snapshot("mem_early").rebase_snapshot(base)
    rebased = diff_snapshot("mem_late").rebase_snapshot(rebased)
    assert filecmp.cmp(rebased.mem, full.mem, shallow=False)


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
    # A plain UFFD restore attaches no backend, and there is no HTTP way to get dirty pages.
    assert restored.mem_backend is None
    assert_no_dirty_pages_endpoint(restored)
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
    With virtio-mem, the memory file covers the hotplug region and the unplugged
    slots are zero in it (the backend zeroes the `unplugged` ranges Firecracker
    reports). The shape of the reply itself is checked by the Rust unit tests;
    here we look at the files and at the dirty page counts.
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

    # Nothing plugged yet: the whole hotplug region is unplugged. The file is
    # `total_size` long and the hotplug region is zero in it. No page of the
    # unplugged region can be reported dirty: at most DRAM is.
    snapshot = vm.snapshot_full(mem_path="mem_unplugged")
    assert dirty_bytes(vm.last_backend_copy) <= MEM_SIZE_MIB * 2**20
    assert snapshot.mem.stat().st_size == total_size
    with open(snapshot.mem, "rb") as mem:
        mem.seek(MEM_SIZE_MIB * 2**20)
        assert mem.read(512 * 2**20) == bytes(512 * 2**20)

    # A diff after a brief run: a few pages, nothing like the 512 MiB unplugged region.
    vm.resume()
    vm.pause()
    assert dirty_bytes(vm.precopy_pass("mem_scratch")) < 16 * 2**20

    # Plug one slot's worth and make the guest use it. The plugged slot is part
    # of the file; the rest of the hotplug region is still zero.
    vm.resume()
    vm.api.memory_hotplug.patch(requested_size_mib=128)
    make_guest_dirty_memory(vm.ssh, amount_mib=64)
    snapshot = vm.snapshot_full(mem_path="mem_plugged", vmstate_path="vmstate_plugged")
    assert snapshot.mem.stat().st_size == total_size
    with open(snapshot.mem, "rb") as mem:
        mem.seek(MEM_SIZE_MIB * 2**20)
        assert mem.read(128 * 2**20) != bytes(128 * 2**20)
        assert mem.read((512 - 128) * 2**20) == bytes((512 - 128) * 2**20)
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

    # Plug two slots and put data in the guest's memory, most of which can only
    # fit in the hotplugged part.
    vm.hotplug_memory(256)
    vm.ssh.check_output("mount -o remount,size=300M -t tmpfs tmpfs /dev/shm")
    vm.ssh.check_output("dd if=/dev/urandom of=/dev/shm/data bs=1M count=200")
    base = vm.snapshot_full(mem_path="mem_base")
    with open(base.mem, "rb") as mem:
        mem.seek((MEM_SIZE_MIB + 256) * 2**20)
        assert mem.read(256 * 2**20) == bytes(256 * 2**20)
    vm.resume()

    # Unplug one slot's worth. The guest migrates the data away first, so the
    # file survives and the slot's old bytes are still in `base`.
    vm.ssh.check_output("rm /dev/shm/data; sync")
    vm.hotplug_memory(128)

    # Merge a diff straight into a copy of the base, as a backend would.
    merged = Path(vm.chroot()) / "mem_merged"
    shutil.copy(base.mem, merged)
    diff = vm.snapshot_diff(mem_path="mem_merged", vmstate_path="vmstate_diff")
    # The unplug marked the slot dirty, but as long as it is unplugged Firecracker
    # describes it through `unplugged` only: no bit is set for it, so the dirty
    # set is smaller than the slot that went away plus DRAM.
    assert dirty_bytes(vm.last_backend_copy) < (MEM_SIZE_MIB + 128) * 2**20
    full = vm.snapshot_full(mem_path="mem_full", vmstate_path="vmstate_full")
    assert filecmp.cmp(diff.mem, full.mem, shallow=False)
    # The unplugged slots are zero in the file, whatever the base had there.
    with open(full.mem, "rb") as mem:
        mem.seek((MEM_SIZE_MIB + 128) * 2**20)
        assert mem.read((512 - 128) * 2**20) == bytes((512 - 128) * 2**20)

    # Dirty tracking is independent of reporting: unplug the remaining slot and
    # plug it back with no API call in between. It is plugged at the time of the
    # call, so the marks the unplug left are reported: its pages became zero.
    vm.resume()
    vm.hotplug_memory(0)
    vm.hotplug_memory(128)
    vm.pause()
    replugged = vm.precopy_pass("mem_replugged")
    assert dirty_bytes(replugged) >= 128 * 2**20
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
    assert dirty_bytes(vm.last_backend_copy) >= amount_mib * 2**20
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
    assert dirty_bytes(vm.last_backend_copy) >= 64 * 2**20

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

    # The reply for 4.5 GiB is a 144 KiB bitmap, more than a default socket buffer: this
    # exercises the API thread's blocking write to the backend.
    snapshot = vm.snapshot_full()
    assert snapshot.mem.stat().st_size == mem_size_mib * 2**20
    # A diff right after, still paused, is tiny, and the file still has full size.
    diff = vm.snapshot_diff(mem_path="mem_diff", vmstate_path="vmstate_diff")
    assert dirty_bytes(vm.last_backend_copy) < 2**20
    assert diff.mem.stat().st_size == mem_size_mib * 2**20
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
        dirty = dirty_bytes(vm.last_backend_copy)
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

    # Without a backend: mem_file_path stays mandatory; there is no dirty-pages endpoint.
    plain = microvm_factory.build(guest_kernel, rootfs)
    plain.spawn()
    plain.basic_config(vcpu_count=2, mem_size_mib=MEM_SIZE_MIB, track_dirty_pages=True)
    plain.add_net_iface()
    plain.start()
    assert_no_dirty_pages_endpoint(plain)
    plain.pause()
    with pytest.raises(RuntimeError, match="mem_file_path"):
        plain.api.snapshot_create.put(snapshot_path="vmstate", snapshot_type="Full")
    plain.resume()
    plain.ssh.check_output("true")
    plain.kill()

    # With a backend: mem_file_path must be absent; `snapshot/create` answers 204 and
    # writes no memory; a dead backend does not fail the request or the HTTP API.
    backed = boot_with_mem_backend(microvm_factory.build(guest_kernel, rootfs))
    assert_no_dirty_pages_endpoint(backed)
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
    res = backed.api.snapshot_create.put(snapshot_path="vmstate", snapshot_type="Full")
    assert res.status_code == 204
    assert (Path(backed.chroot()) / "vmstate").exists()
    assert not (Path(backed.chroot()) / "mem").exists()
    with pytest.raises((ConnectionError, FileNotFoundError, OSError)):
        backed.mem_backend.control({"Copy": {"mem_path": "/mem", "full": True}})
    backed.uffd_handler = None
    backed.resume()
    backed.ssh.check_output("true")
    wait_for_log_message(backed, "Memory backend closed its connection")


@pytest.mark.parametrize("mem_size_mib", [MEM_SIZE_MIB, 8192])
def test_backend_protocol_errors(uvm, mem_size_mib):
    """
    A backend that speaks garbage on its connection gets it closed; the microVM
    and the HTTP API are unaffected. An unknown request gets an error reply and
    the connection stays usable. The test plays the backend itself with a plain
    listening socket, so that it can send arbitrary frames. With 8 GiB the
    reply is 256 KiB, more than the socket buffer: read late, it makes the API
    thread wait for writability, under its seccomp filter.
    """
    if mem_size_mib > 4096 and platform.machine() != "x86_64":
        pytest.skip("large guest variant is for the x86 two-region layout")
    vm = uvm
    vm.spawn()
    vm.memory_monitor = None
    sock_path = Path(vm.chroot()) / "raw_backend.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(sock_path))
    # The jailed Firecracker runs as another user.
    sock_path.chmod(0o777)
    listener.listen(1)
    vm.basic_config(vcpu_count=2, mem_size_mib=mem_size_mib, track_dirty_pages=True)
    vm.api.machine_config.put(
        vcpu_count=2,
        mem_size_mib=mem_size_mib,
        track_dirty_pages=True,
        mem_backend={
            "backend_type": "SharedMemfd",
            "backend_path": "/raw_backend.sock",
        },
    )
    vm.add_net_iface()
    vm.start()
    conn, _ = listener.accept()
    conn.settimeout(30)
    # The handshake: one message with the memfd attached. We ignore the fd.
    handshake, ancdata, _, _ = conn.recvmsg(65536, socket.CMSG_SPACE(4))
    assert handshake.startswith(b"[")
    assert ancdata, "no memfd in the handshake"
    vm.ssh.check_output("true")

    def frame(json_bytes, blob=b""):
        return struct.pack("<II", len(json_bytes), len(blob)) + json_bytes + blob

    def read_frame():
        header = b""
        while len(header) < 8:
            header += conn.recv(8 - len(header))
        json_len, blob_len = struct.unpack("<II", header)
        body = b""
        while len(body) < json_len + blob_len:
            body += conn.recv(json_len + blob_len - len(body))
        return body[:json_len], body[json_len:]

    # An unknown request is answered with an error; the connection stays usable.
    conn.sendall(frame(b'{"request":"Frobnicate"}'))
    json_part, blob = read_frame()
    assert b"error" in json_part and not blob
    conn.sendall(frame(b'{"request":"DirtyPages"}'))
    # Let the reply pile up in the socket buffer (and beyond it, for the large
    # guest, which stalls the API thread until we read) before reading it.
    time.sleep(1)
    json_part, blob = read_frame()
    vm.api.describe.get()
    assert b"total_size" in json_part
    assert len(blob) == mem_size_mib * 2**20 // PAGE_SIZE // 8
    # A booted guest has dirtied something.
    assert sum(byte.bit_count() for byte in blob) > 0

    # A frame with a blob in a request breaks framing: Firecracker closes the connection.
    conn.sendall(frame(b'{"request":"DirtyPages"}', b"x"))
    assert conn.recv(1) == b""
    conn.close()
    listener.close()
    wait_for_log_message(vm, "closing")
    # MicroVM and HTTP API are fine; snapshot/create still writes vmstate.
    vm.ssh.check_output("true")
    vm.pause()
    vm.api.snapshot_create.put(snapshot_path="vmstate", snapshot_type="Full")
    vm.resume()
    vm.ssh.check_output("true")


def test_mem_backend_requires_api(uvm):
    """`--no-api` rejects a memory backend: nothing would service its connection."""
    vm = uvm
    vm.create_jailed_resource(vm.kernel_file)
    vm.create_jailed_resource(vm.rootfs_file)
    config = {
        "boot-source": {
            "kernel_image_path": vm.kernel_file.name,
            "boot_args": "console=ttyS0 reboot=k panic=1",
        },
        "drives": [
            {
                "drive_id": "rootfs",
                "path_on_host": vm.rootfs_file.name,
                "is_root_device": True,
                "is_read_only": True,
            }
        ],
        "machine-config": {
            "vcpu_count": 1,
            "mem_size_mib": 128,
            "mem_backend": {
                "backend_type": "SharedMemfd",
                "backend_path": "/backend.sock",
            },
        },
    }
    vm_config = Path(vm.chroot()) / "vm_config.json"
    vm_config.write_text(json.dumps(config))
    vm.jailer.extra_args = {"config-file": vm_config.name, "no-api": None}
    vm.spawn(serial_out_path=None)
    vm.mark_killed()
    vm.check_log_message("requires the API server")
    assert vm.get_exit_code() != 0
