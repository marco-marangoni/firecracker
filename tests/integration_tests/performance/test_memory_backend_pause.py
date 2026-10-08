# SPDX-License-Identifier: Apache-2.0
"""Measure how long a guest is paused for a suspend + resume onto a pre-warmed
Firecracker, with the memory written by Firecracker (a mincore `Diff`, no dirty
tracking: resident pages only, no earlier `Full` needed) and with a SharedMemfd
memory backend:

- no pre-copy: the backend copies every resident page during the pause;
- pre-copy + lazy: `dirty-pages` rounds while the guest runs, the residual
  dirty set copied during the pause, faults served from the pre-copy target;
- pre-copy + post-copy: nothing copied during the pause. The destination
  handler gets the final layout and serves faults from the pre-copy target,
  fetching the residual dirty pages from the source backend (over a unix socket
  standing in for the network) on demand and in the background.

Everything that does not need the guest paused is done before the pause: the
destination Firecracker is spawned, the memory file is created at its final
size and hardlinked into the destination chroot, and the destination handler
is started on it (it maps and populates the file before Firecracker connects).
All files are on the /srv tmpfs of the devcontainer: no disk is involved.

Pause = wall clock from the pause request to the resume returning. "ssh" is the
additional time until the guest answers over ssh after the resume.

Run with: tools/devtool -y test -- integration_tests/performance/test_memory_backend_pause.py -s
"""

import os
import re
import time
from pathlib import Path

import pytest

from framework.artifacts import GUEST_KERNEL_DEFAULT, GuestKernel, pin_guest_kernel
from framework.guest import GuestDistro
from framework.microvm import Snapshot, SnapshotType
from framework.utils import make_guest_dirty_memory
from framework.utils_hugepages import HugePagesConfig
from framework.utils_uffd import CONTROL_SOCKET_PATH, spawn_pf_handler, uffd_handler
from integration_tests.functional.test_memory_backend import (
    authoritative_bytes,
    discard_bytes,
)

# Guest sizes; the guest touches all of its memory but `headroom` at boot.
MEM_MIBS = [512, 1024, 2048, 4096, 8192, 16384]


def headroom_mib(mem_mib):
    """What to leave to the guest kernel: 100 MiB plus ~6% (with 3% the OOM killer
    still got `fillmem` now and then at 1-4 GiB)."""
    return 100 + mem_mib // 16


VCPUS = 2
# Guest workload while measuring: keep rewriting the same 64 MiB of tmpfs.
DIRTY_LOOP = (
    "nohup sh -c 'while true; do dd if=/dev/zero of=/dev/shm/dirty bs=1M count=64 "
    "conv=notrunc 2>/dev/null; sleep 0.05; done' >/dev/null 2>&1 </dev/null &"
)
PRECOPY_ROUNDS = 4
# Between the last pre-copy round and the pause, so that the residual dirty set
# is the workload's working set rather than timing noise.
RESIDUAL_GAP_S = 0.5
ITERATIONS = 3
MEM = "mem"
VMSTATE = "vmstate"
SOURCE_SOCK = "/source-control.sock"

pytestmark = pin_guest_kernel(GUEST_KERNEL_DEFAULT)


def ms(seconds):
    """Seconds to a rounded millisecond count."""
    return round(seconds * 1000)


def timed(fn):
    """Run `fn`, return (result, elapsed seconds)."""
    start = time.perf_counter()
    result = fn()
    return result, time.perf_counter() - start


def boot(
    microvm_factory,
    guest_kernel,
    rootfs,
    backend,
    track_dirty_pages,
    huge_pages,
    mem_mib,
):
    """Boot a guest, touch all but `headroom_mib` of its memory, start the dirtying loop."""
    vm = microvm_factory.build(guest_kernel, rootfs)
    vm.memory_monitor = None
    vm.spawn()
    vm.basic_config(
        vcpu_count=VCPUS,
        mem_size_mib=mem_mib,
        track_dirty_pages=track_dirty_pages,
        mem_backend="on_demand" if backend else None,
        huge_pages=huge_pages,
    )
    vm.add_net_iface()
    vm.start()
    make_guest_dirty_memory(vm.ssh, amount_mib=mem_mib - headroom_mib(mem_mib))
    vm.ssh.check_output(DIRTY_LOOP)
    time.sleep(1)
    return vm


def prepare_destination(microvm_factory, src, lazy):
    """Everything a restore needs that does not need the guest paused.

    The memory file is created at its final size in `src`'s chroot (Firecracker
    and the backend both write into an existing file of the right size without
    truncating it) and hardlinked into the destination chroot together with an
    empty `vmstate` (Firecracker's `O_TRUNC` write keeps the inode). The
    destination Firecracker is spawned; for a lazy restore the handler too.
    """
    root = Path(src.chroot())
    mem = root / MEM
    if not mem.exists():
        mem.touch()
    os.truncate(mem, src.mem_size_bytes)
    (root / VMSTATE).touch()
    # Firecracker runs as the jailer's user.
    for path in (mem, root / VMSTATE):
        os.chown(path, src.jailer.uid, src.jailer.gid)
    snapshot = Snapshot(
        vmstate=root / VMSTATE,
        mem=mem,
        disks=src.disks,
        net_ifaces=[x["iface"] for x in src.iface.values()],
        ssh_key=src.ssh_key,
        snapshot_type=SnapshotType.FULL,
        meta={
            "kernel_file": str(src.guest_kernel.vmlinux),
            "rootfs_file": str(src.rootfs_file) if src.rootfs_file else None,
            "vcpus_count": src.vcpus_count,
        },
    )

    dest = microvm_factory.build()
    dest.memory_monitor = None
    dest.spawn()
    jailed = snapshot.copy_to_chroot(Path(dest.chroot()))
    if lazy:
        dest.uffd_handler = spawn_pf_handler(
            dest,
            uffd_handler("on_demand", binary_dir=dest.fc_binary_path.parent),
            jailed,
            mem_backend=True,
        )
    for disk in jailed.disks.values():
        dest.create_jailed_resource(disk)
    dest.disks = jailed.disks
    dest.ssh_key = jailed.ssh_key
    for iface in jailed.net_ifaces:
        dest.add_net_iface(iface, api=False)
    dest.guest_kernel = GuestKernel.from_vmlinux(Path(jailed.meta["kernel_file"]))
    dest.vcpus_count = jailed.meta["vcpus_count"]
    if jailed.meta["rootfs_file"]:
        dest.rootfs_file = Path(jailed.meta["rootfs_file"])
        dest.distro = GuestDistro.from_rootfs(dest.rootfs_file)
    return dest, jailed


def load(dest, jailed, lazy):
    """`snapshot/load` on the prepared destination, paused."""
    if lazy:
        mem_backend = {
            "backend_type": "Uffd",
            "backend_path": str(dest.uffd_handler.socket_path),
        }
    else:
        mem_backend = {"backend_type": "File", "backend_path": f"/{jailed.mem.name}"}
    dest.api.snapshot_load.put(
        mem_backend=mem_backend,
        snapshot_path=f"/{jailed.vmstate.name}",
        enable_diff_snapshots=False,
        resume_vm=False,
    )


def resume(dest, row):
    """Resume the destination and wait for the guest; fill `row` in."""
    _, resume_s = timed(dest.resume)
    _, ssh_s = timed(lambda: dest.ssh.check_output("true", timeout=60))
    row.update(resume=resume_s, ssh=ssh_s)


def file_diff(vm, dest, jailed, lazy):
    """Firecracker writes the resident pages (mincore, no dirty tracking); the
    destination maps the file (4K) or serves it through UFFD (hugetlbfs)."""
    _, pause_s = timed(vm.pause)
    _, create_s = timed(
        lambda: vm.api.snapshot_create.put(
            mem_file_path=MEM, snapshot_path=VMSTATE, snapshot_type="Diff"
        )
    )
    # A Diff file is sparse: only the written pages are allocated.
    written = (Path(vm.chroot()) / MEM).stat().st_blocks * 512
    _, load_s = timed(lambda: load(dest, jailed, lazy))
    row = {
        "pause": pause_s,
        "create": create_s,
        "copy": 0.0,
        "load": load_s,
        "copied": written,
    }
    resume(dest, row)
    return row


def backend_final_layout(vm):
    """Pause and take the `Backend` snapshot: state + final layout, no memory."""
    _, pause_s = timed(vm.pause)
    response, create_s = timed(
        lambda: vm.api.snapshot_create.put(
            snapshot_path=VMSTATE, snapshot_type="Backend"
        )
    )
    memory = response.json()["memory"]
    return memory, {"pause": pause_s, "create": create_s}


def backend_lazy(vm, dest, jailed, memory, row):
    """The backend copies the final layout during the pause; faults are served
    from the complete pre-copy target."""
    _, copy_s = timed(lambda: vm.mem_backend.copy(memory, f"/{MEM}"))
    _, load_s = timed(lambda: load(dest, jailed, lazy=True))
    row.update(
        copy=copy_s,
        load=load_s,
        copied=authoritative_bytes(memory) + discard_bytes(memory),
    )
    resume(dest, row)


def backend_post_copy(vm, dest, jailed, memory, row):
    """Nothing is copied during the pause: the destination handler gets the
    layout and fetches the residual pages from the source backend after the
    resume. The layout must reach the handler before `snapshot/load`: Firecracker
    touches guest memory while restoring (kvmclock, vmgenid, virtqueues) and
    those faults must already be served post-copy."""
    # The destination handler reaches the source backend's control socket
    # through a hardlink into its chroot (both chroots are on the same tmpfs).
    source_sock = Path(vm.chroot()) / CONTROL_SOCKET_PATH.lstrip("/")
    os.link(source_sock, Path(dest.chroot()) / SOURCE_SOCK.lstrip("/"))
    _, post_copy_s = timed(
        lambda: dest.uffd_handler.control(
            {"PostCopy": {"memory": memory, "source_sock": SOURCE_SOCK}}
        )
    )
    _, load_s = timed(lambda: load(dest, jailed, lazy=True))
    row.update(
        copy=post_copy_s,
        load=load_s,
        copied=0,
        residual=authoritative_bytes(memory) + discard_bytes(memory),
    )
    resume(dest, row)
    # Wait for the background pull to drain the residual set.
    deadline = time.time() + 60
    while time.time() < deadline:
        match = re.search(
            r"Post-copy complete: (\d+) pages, (\d+) on demand, (\d+) in the background, "
            r"(\d+) ms",
            dest.uffd_handler.log_data,
        )
        if match:
            total, on_demand, background, elapsed_ms = map(int, match.groups())
            row.update(
                post_copy_pages=total,
                on_demand_pages=on_demand,
                background_pages=background,
                drain_ms=elapsed_ms,
            )
            return
        time.sleep(0.01)
    raise AssertionError(f"post-copy did not drain:\n{dest.uffd_handler.log_data}")


SCENARIOS = [
    # (label, track_dirty_pages, method)
    ("file Diff (mincore, no tracking) → File / UFFD on 2M", False, "diff"),
    ("memfd, no pre-copy (mincore, no tracking) → lazy", False, "lazy"),
    ("memfd, pre-copy → lazy, residual copied in pause", True, "precopy-lazy"),
    ("memfd, pre-copy → post-copy, residual fetched after", True, "precopy-postcopy"),
]


def run_scenario(microvm_factory, guest_kernel, rootfs, scenario, huge_pages, mem_mib):
    """One measurement of one scenario."""
    _, track_dirty_pages, method = scenario
    backend = method != "diff"
    # Huge page snapshots can only be restored through UFFD.
    file_lazy = huge_pages != HugePagesConfig.NONE
    vm = boot(
        microvm_factory,
        guest_kernel,
        rootfs,
        backend,
        track_dirty_pages,
        huge_pages,
        mem_mib,
    )
    rounds = []
    if method.startswith("precopy"):
        # The pre-copy target is the memory file; the first round creates it, so the
        # destination can only be prepared once it exists at full size.
        memory, round_s = timed(lambda: vm.dirty_pages(copy_to=MEM))
        rounds.append((authoritative_bytes(memory), round_s))
        dest, jailed = prepare_destination(microvm_factory, vm, lazy=True)
        for _ in range(PRECOPY_ROUNDS - 1):
            memory, round_s = timed(lambda: vm.dirty_pages(copy_to=MEM))
            rounds.append((authoritative_bytes(memory), round_s))
        time.sleep(RESIDUAL_GAP_S)
    else:
        dest, jailed = prepare_destination(
            microvm_factory, vm, lazy=backend or file_lazy
        )

    if method == "diff":
        row = file_diff(vm, dest, jailed, file_lazy)
    else:
        memory, row = backend_final_layout(vm)
        if method == "precopy-postcopy":
            backend_post_copy(vm, dest, jailed, memory, row)
        else:
            backend_lazy(vm, dest, jailed, memory, row)
    row["rounds"] = rounds
    vm.kill()
    dest.kill()
    # 2 GiB each on the /srv tmpfs; the chroots live until the end of the test.
    for root in (vm.chroot(), dest.chroot()):
        for path in Path(root).glob("mem*"):
            path.unlink()
    row["total"] = (
        row["pause"] + row["create"] + row["copy"] + row["load"] + row["resume"]
    )
    return row


@pytest.mark.parametrize(
    "huge_pages", [HugePagesConfig.NONE, HugePagesConfig.HUGETLBFS_2MB]
)
@pytest.mark.parametrize("mem_mib", MEM_MIBS)
def test_pause_measurements(microvm_factory, guest_kernel, rootfs, huge_pages, mem_mib):
    """Print a markdown table of pause times per scenario."""
    results = []
    for scenario in SCENARIOS:
        rows = sorted(
            (
                run_scenario(
                    microvm_factory, guest_kernel, rootfs, scenario, huge_pages, mem_mib
                )
                for _ in range(ITERATIONS)
            ),
            key=lambda r: r["total"],
        )
        results.append((scenario, rows[len(rows) // 2]))
        print(
            f"{scenario[0]}: totals {[ms(r['total']) for r in rows]} ms, "
            f"ssh {[ms(r['ssh']) for r in rows]} ms"
        )

    print()
    print(
        f"{VCPUS} vCPUs, {mem_mib} MiB, {huge_pages}, {mem_mib - headroom_mib(mem_mib)} MiB touched, 64 MiB "
        f"rewritten continuously; destination spawned before the pause; medians of "
        f"{ITERATIONS}; ms"
    )
    print()
    print(
        "| scenario | pause req | create | copy / post-copy setup | load | resume "
        "| total pause | ssh after | bytes copied in pause |"
    )
    print("| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")
    for (label, _, method), r in results:
        copy = "" if method == "diff" else ms(r["copy"])
        print(
            f"| {label} | {ms(r['pause'])} | {ms(r['create'])} | {copy} | "
            f"{ms(r['load'])} | {ms(r['resume'])} | **{ms(r['total'])}** | "
            f"{ms(r['ssh'])} | {r['copied'] / 2**20:.0f} MiB |"
        )
    print()
    for (label, _, _), r in results:
        if r["rounds"]:
            rounds = ", ".join(
                f"{b / 2**20:.0f} MiB in {ms(s)} ms" for b, s in r["rounds"]
            )
            print(f"pre-copy rounds ({label}): {rounds}")
        if "drain_ms" in r:
            print(
                f"post-copy ({label}): residual {r['residual'] / 2**20:.0f} MiB = "
                f"{r['post_copy_pages']} pages; {r['on_demand_pages']} fetched on "
                f"demand, {r['background_pages']} in the background; drained "
                f"{r['drain_ms']} ms after the resume"
            )
