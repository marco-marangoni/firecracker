# Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Performance benchmark for guest memory allocation speed."""

from dataclasses import dataclass

import pytest

from framework.artifacts import ACPI_GUEST_KERNELS, pin_guest_kernel

pytestmark = pin_guest_kernel(ACPI_GUEST_KERNELS)

# Guest needs enough memory to allocate the largest block (1GB) plus OS overhead
GUEST_MEM_MIB = 2048


@dataclass
class AllocParams:
    """Parameters for a memory allocation benchmark run."""
    block_size: int
    iterations: int


WORKLOADS = ["fill", "fault", "random", "stride"]


@pytest.mark.nonci
@pytest.mark.parametrize(
    "params",
    [
        pytest.param(AllocParams(block_size=4096, iterations=32 * 1024), id="4KB"),
        pytest.param(AllocParams(block_size=2 * 1024 * 1024, iterations=32 * 1024), id="2MB"),
        pytest.param(AllocParams(block_size=16 * 1024 * 1024, iterations=4 * 1024), id="16MB"),
        pytest.param(AllocParams(block_size=128 * 1024 * 1024, iterations=256), id="128MB"),
        pytest.param(AllocParams(block_size=1024 * 1024 * 1024, iterations=32), id="1GB"),
    ],
)
@pytest.mark.parametrize("guest_hugepage", [False, True], ids=["", "hugepage"])
def test_memory_allocation_speed(
    microvm_factory,
    guest_kernel,
    rootfs,
    params,
    guest_hugepage,
    alloc_bench_bin,
    metrics,
):
    """Measure how fast the guest can allocate and fill memory blocks."""

    vm = microvm_factory.build(guest_kernel, rootfs, monitor_memory=False)
    vm.spawn()
    vm.basic_config(vcpu_count=2, mem_size_mib=GUEST_MEM_MIB)
    vm.add_net_iface()
    vm.start()

    metrics.set_dimensions(
        {
            "performance_test": "test_memory_allocation_speed",
            "block_size": str(params.block_size),
            "guest_hugepage": str(guest_hugepage),
            **vm.dimensions,
        }
    )

    vm.ssh.scp_put(alloc_bench_bin, "/tmp/alloc_bench")
    vm.ssh.check_output("chmod +x /tmp/alloc_bench")

    hugepage_arg = " hugepage" if guest_hugepage else ""

    for workload in WORKLOADS:
        _, stdout, _ = vm.ssh.check_output(
            f"/tmp/alloc_bench {params.block_size} {params.iterations}"
            f" {workload}{hugepage_arg}"
        )

        for line in stdout.strip().splitlines():
            elapsed_ns = int(line)
            metrics.put_metric(f"{workload}_time", elapsed_ns / 1000, "Microseconds")

    vm.kill()


def test_check_thp_enabled_in_guest(
    microvm_factory,
    guest_kernel,
    rootfs,
):
    """Measure how fast the guest can allocate and fill memory blocks."""

    vm = microvm_factory.build(guest_kernel, rootfs)
    vm.spawn()
    vm.basic_config(vcpu_count=2, mem_size_mib=GUEST_MEM_MIB)
    vm.add_net_iface()
    vm.start()
    _, stdout, _ = vm.ssh.check_output("cat /sys/kernel/mm/transparent_hugepage/enabled")
    assert stdout.strip() == "always [madvise] never"
