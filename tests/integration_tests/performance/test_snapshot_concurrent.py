# Copyright 2023 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Performance benchmark for snapshot restore."""

import concurrent.futures
import tempfile
import threading

import pytest

import host_tools.drive as drive_tools
from framework.artifacts import GUEST_KERNEL_DEFAULT, pin_guest_kernel

USEC_IN_MSEC = 1000
NS_IN_MSEC = 1_000_000
ITERATIONS = 30
CONCURRENT_VM_RESTORE = 250

pytestmark = pin_guest_kernel(GUEST_KERNEL_DEFAULT)

@pytest.mark.nonci
def test_restore_latency_concurrent(
    microvm_factory, guest_kernel, rootfs, metrics
):
    """
    Measures snapshot restore latency under concurrent restore pressure,
    mimicking production setups where multiple VMs are restored simultaneously
    on the same host. Uses a lot of PCI devices to simulate
    the configuration used by Lambda.

    Note: this test doesn't run by default on the perf pipeline, as the measurements
    have an high variance.
    """

    vm = microvm_factory.build(guest_kernel, rootfs, pci=True)
    vm.spawn(log_level="Info", emit_metrics=False)
    vm.time_api_requests = False
    vm.basic_config(
        vcpu_count=1,
        mem_size_mib=128,
        rootfs_io_engine="Sync",
        enable_entropy_device=True,
    )

    # 12 PCI devices matching Lambda config: 6 block + 3 net + 1 vsock + 1 balloon + 1 rng
    # Total MSI-X vectors: 6×2 + 3×3 + 1×4 + 1×3 + 1×2 = 30
    scratch_drives = []
    for i in range(5):
        scratch = drive_tools.FilesystemFile(tempfile.mktemp(), size=8)
        scratch_drives.append(scratch)
        vm.add_drive(f"scratch{i}", scratch.path, io_engine="Sync")

    for _ in range(3):
        vm.add_net_iface()

    vm.api.vsock.put(vsock_id="vsock0", guest_cid=3, uds_path="/v.sock")
    vm.api.balloon.put(amount_mib=0, deflate_on_oom=True, stats_polling_interval_s=1)

    vm.start()
    snapshot = vm.snapshot_full()
    vm.kill()

    metrics.set_dimensions(
        {
            "concurrent_vms": str(CONCURRENT_VM_RESTORE),
            "performance_test": "test_restore_latency_concurrent",
            **vm.dimensions,
        }
    )

    # Restore batches concurrently. The KVM_SET_GSI_ROUTING calls will
    # contend with SRCU readers from the background VMs.
    batch = []
    for _ in range(CONCURRENT_VM_RESTORE):
        microvm = microvm_factory.build()
        microvm.spawn()
        microvm.time_api_requests = False
        batch.append(microvm)

    barrier = threading.Barrier(CONCURRENT_VM_RESTORE)

    def do_restore(vm):
        barrier.wait()
        vm.restore_from_snapshot(snapshot, resume=True)
        vm.flush_metrics()
        for data_point in vm.get_all_metrics():
            cur_value = data_point["latencies_us"]["load_snapshot"]
            if cur_value > 0:
                return {"load_snapshot": cur_value / USEC_IN_MSEC}
        return None

    with concurrent.futures.ThreadPoolExecutor(
        max_workers=CONCURRENT_VM_RESTORE
    ) as executor:
        futures = [executor.submit(do_restore, vm) for vm in batch]
        for future in concurrent.futures.as_completed(futures):
            result = future.result()
            assert result is not None, "Latency metric not found"
            metrics.put_metric("latency", result["load_snapshot"], "Milliseconds")
