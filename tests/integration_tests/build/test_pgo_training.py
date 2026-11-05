# Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0

"""PGO training workload for release builds.

`tools/release.sh --pgo-instrument` builds the release binaries with
`-Cprofile-generate=/pgo`; this module runs it through the boot → snapshot →
restore flow that `test_memory_overhead` measures, plus a little guest I/O, and
collects the `.profraw` files the LLVM profiling runtime writes when the process
exits. `tools/release.sh --pgo-use` then merges them and feeds the result to
the final build. `devtool build --release` and `devtool make_release` drive the
three steps.

The profiling runtime writes the profile from an `atexit` hook, so each
microVM is shut down through a guest `reboot` rather than SIGKILL, and the
production seccomp filter is extended with the syscalls the writer needs. The
jailer clears the environment, so `LLVM_PROFILE_FILE` cannot be used; the
output path is fixed to `/pgo` inside the jail at compile time.

Training covers the PCI transport only. It is not a test and does not run in
CI's test suites (`nonci`).
"""

import json
import os
import platform
import shutil
from pathlib import Path

import pytest

from framework import defs
from framework.artifacts import ACPI_GUEST_KERNELS, pin_guest_kernel, pin_pci
from host_tools.cargo_build import run_seccompiler_bin

# Syscalls the LLVM profiling runtime makes while writing the profile at exit
# (strace of an instrumented `firecracker --version`). musl issues `mkdir` and
# `open` on x86_64 but `mkdirat` and `openat` on aarch64, which has no others.
PROFILER_SYSCALLS = [
    "mkdir",
    "mkdirat",
    "open",
    "openat",
    "getpid",
    "fcntl",
    "ioctl",
    "lseek",
    "ftruncate",
    "writev",
    "close",
    "mmap",
    "munmap",
    "prctl",
]


@pytest.fixture
def profiler_filter(microvm_factory, tmp_path):
    """The production seccomp filter plus the profile writer's syscalls."""
    src = defs.SECCOMP_JSON_DIR / f"{platform.machine()}-unknown-linux-musl.json"
    filters = json.loads(src.read_text())
    for thread in filters.values():
        thread["filter"].extend({"syscall": s} for s in PROFILER_SYSCALLS)
    json_path = tmp_path / "filter.json"
    json_path.write_text(json.dumps(filters))
    bpf_path = tmp_path / "filter.bpf"
    run_seccompiler_bin(bpf_path, json_path, binary_dir=microvm_factory.binary_path)
    return bpf_path


def _install_filter(vm, bpf_path):
    vm.create_jailed_resource(bpf_path)
    vm.jailer.extra_args["seccomp-filter"] = bpf_path.name


def _profile_dir(vm):
    """Make `/pgo` inside the jail writable by the jailed process.

    Called after spawn: the jailer is instrumented too and its daemonizing
    parents, which exit before the API socket comes up, have already written
    their own profiles there. Drop them so only Firecracker's are collected.
    """
    pgo_dir = Path(vm.chroot()) / "pgo"
    pgo_dir.mkdir(exist_ok=True)
    os.chmod(pgo_dir, 0o777)
    for stale in pgo_dir.glob("*.profraw"):
        stale.unlink()
    return pgo_dir


def _collect_profiles(pgo_dir, out_dir, tag):
    out_dir.mkdir(parents=True, exist_ok=True)
    found = list(pgo_dir.glob("*.profraw"))
    assert found, f"no .profraw written in {pgo_dir}"
    for i, f in enumerate(found):
        shutil.copy(f, out_dir / f"{tag}_{i}.profraw")


def _reboot_and_wait(vm):
    """Exit Firecracker gracefully so the atexit profile writer runs."""
    vm.ssh.run("reboot")
    vm.mark_killed(timeout=120)


@pytest.mark.nonci
@pytest.mark.parametrize("vcpu_count,mem_size_mib", [(1, 128), (2, 2048)])
@pin_pci(True)
@pin_guest_kernel(ACPI_GUEST_KERNELS)
def test_pgo_training(
    microvm_factory,
    guest_kernel,
    rootfs,
    vcpu_count,
    mem_size_mib,
    pci_enabled,
    profiler_filter,
):
    """Boot + snapshot + restore training run."""
    tag = f"{guest_kernel.metric_id}_{vcpu_count}_{mem_size_mib}"
    # `--binary-dir` is build/<pgo tag>/instrumented; profiles go next to it.
    out_dir = microvm_factory.binary_path.parent / "profraw"

    vm = microvm_factory.build(
        guest_kernel, rootfs, pci=pci_enabled, monitor_memory=False
    )
    _install_filter(vm, profiler_filter)
    vm.spawn(emit_metrics=True)
    pgo_dir = _profile_dir(vm)
    vm.basic_config(vcpu_count=vcpu_count, mem_size_mib=mem_size_mib)
    vm.add_net_iface()
    vm.api.balloon.put(amount_mib=0, deflate_on_oom=True, stats_polling_interval_s=1)
    vm.start()
    # A little guest I/O so the virtio paths show up in the profile.
    vm.ssh.check_output("dd if=/dev/vda of=/dev/null bs=1M count=32")
    vm.ssh.check_output("sync")
    vm.api.balloon.patch(amount_mib=16)
    vm.api.balloon_stats.get()

    snapshot = vm.snapshot_full()
    vm.resume()
    _reboot_and_wait(vm)
    _collect_profiles(pgo_dir, out_dir, f"{tag}_boot")

    vm2 = microvm_factory.build(monitor_memory=False)
    _install_filter(vm2, profiler_filter)
    vm2.spawn()
    pgo_dir2 = _profile_dir(vm2)
    vm2.restore_from_snapshot(snapshot, resume=True)
    vm2.ssh.check_output("dd if=/dev/vda of=/dev/null bs=1M count=32")
    _reboot_and_wait(vm2)
    _collect_profiles(pgo_dir2, out_dir, f"{tag}_restore")
