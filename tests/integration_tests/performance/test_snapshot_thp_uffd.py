# Copyright 2024 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Performance benchmark for snapshot restore with on-demand UFFD using THP (2MiB) page resolution."""

import signal
import time

import pytest

from framework.artifacts import GUEST_KERNEL_DEFAULT, pin_guest_kernel
from framework.microvm import HugePagesConfig

# --- PINNED FOR FAST ITERATION (revert to expand test matrix) ---
ITERATIONS = 30
VCPUS = 2
MEM = 1024
PCI = True
# ----------------------------------------------------------------

NS_IN_MSEC = 1_000_000

pytestmark = pin_guest_kernel(GUEST_KERNEL_DEFAULT)


def _boot_vm(microvm_factory, guest_kernel, rootfs, huge_pages):
    """Boot a microvm with pinned vcpu/memory/pci configuration."""
    vm = microvm_factory.build(
        guest_kernel,
        rootfs,
        monitor_memory=False,
        pci=PCI,
    )
    vm.spawn(log_level="Info", emit_metrics=True)
    vm.time_api_requests = False
    vm.basic_config(
        vcpu_count=VCPUS,
        mem_size_mib=MEM,
        rootfs_io_engine="Sync",
        huge_pages=huge_pages,
    )
    for _ in range(3):
        vm.add_net_iface()
    vm.start()
    return vm


@pytest.mark.nonci
@pytest.mark.parametrize(
    ("huge_pages", "uffd_handler"),
    [
        (HugePagesConfig.TRANSPARENT, "on_demand"),
        (HugePagesConfig.TRANSPARENT, "on_demand_512kib"),
        (HugePagesConfig.TRANSPARENT, "on_demand_2mib"),
        (HugePagesConfig.HUGETLBFS_2MB, "on_demand"),
    ],
    ids=[
        "thp_4k_on_demand",
        "thp_512k_on_demand",
        "thp_2m_on_demand",
        "hugetlbfs_2m_on_demand",
    ],
)
def test_thp_uffd_restore_latency(
    microvm_factory,
    rootfs,
    guest_kernel,
    metrics,
    huge_pages,
    uffd_handler,
):
    """Compares post-restore guest memory access latency across UFFD page resolution strategies.

    Uses the fast_page_fault_helper guest binary (touches all memory pages) to measure the
    end-to-end latency from the guest's perspective.

    For transparent huge pages (THP), guest memory is backed by 4KiB base pages that the kernel
    may collapse into 2MiB mappings, so three handlers are compared:
      * 'on_demand' resolves each fault with a 4KiB UFFDIO_COPY.
      * 'on_demand_512kib' batches faults into 512KiB UFFDIO_COPYs, reducing ioctl round-trips
        relative to the 4KiB handler while using a smaller batch than the 2MiB handler.
      * 'on_demand_2mib' responds to each fault with a 2MiB UFFDIO_COPY, so the kernel can create
        THP mappings and satisfy subsequent faults within the same 2MiB region without additional
        ioctl round-trips.

    For hugetlbfs, guest memory is physically backed by 2MiB huge pages, so UFFDIO_COPY must be
    performed at the native 2MiB granularity: sub-2MiB resolution is not possible. The 'on_demand'
    handler already serves each fault at the region's configured page size (2MiB for hugetlbfs, as
    reported by Firecracker in the UFFD mapping), so it is the only meaningful handler here and
    needs no changes to work with hugetlbfs-backed memory.
    """
    vm = _boot_vm(microvm_factory, guest_kernel, rootfs, huge_pages)

    metrics.set_dimensions(
        {
            "performance_test": "test_thp_uffd_restore_latency",
            "huge_pages": str(huge_pages),
            "uffd_handler": uffd_handler,
            **vm.dimensions,
        }
    )

    # Start the helper before snapshotting so it's ready on restore.
    vm.ssh.check_output(
        "nohup /usr/local/bin/fast_page_fault_helper >/dev/null 2>&1 </dev/null &"
    )
    time.sleep(5)

    snapshot = vm.snapshot_full()
    vm.kill()

    for microvm in microvm_factory.build_n_from_snapshot(
        snapshot, ITERATIONS, uffd_handler_name=uffd_handler
    ):
        microvm.memory_monitor = None
        _, pid, _ = microvm.ssh.check_output("pidof fast_page_fault_helper")

        # Signal the helper to start touching memory.
        microvm.ssh.check_output(f"kill -s {signal.SIGUSR1} {pid}")

        # Wait for the helper to report how long it took.
        _, duration, _ = microvm.ssh.check_output(
            "while [ ! -f /tmp/fast_page_fault_helper.out ]; do sleep 1; done; "
            "cat /tmp/fast_page_fault_helper.out"
        )

        metrics.put_metric("fault_latency", int(duration) / NS_IN_MSEC, "Milliseconds")


# --- TO RESTORE FULL TEST MATRIX, replace the pinned constants and test with: ---
# @pytest.mark.parametrize(("vcpus", "mem"), [(2, 1024), (2, 2048), (3, 4096), (4, 6144)])
# @pytest.mark.parametrize(
#     ("huge_pages", "uffd_handler"),
#     [
#         (HugePagesConfig.TRANSPARENT, "on_demand"),
#         (HugePagesConfig.TRANSPARENT, "on_demand_512kib"),
#         (HugePagesConfig.TRANSPARENT, "on_demand_2mib"),
#         (HugePagesConfig.HUGETLBFS_2MB, "on_demand"),
#     ],
# )
# def test_thp_uffd_restore_latency(microvm_factory, rootfs, guest_kernel, pci_enabled, metrics, vcpus, mem, huge_pages, uffd_handler):
#     ...  # pass vcpus, mem, pci_enabled, huge_pages through to _boot_vm
