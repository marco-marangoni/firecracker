# Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Memory backend process related utilities.

A memory backend is an external process Firecracker shares the guest memory
with (see docs/snapshotting/memory-backend-design.md). The reference
implementation lives in src/firecracker/examples/memory_backend.
"""

import os
import signal
import stat
import subprocess
import time
from pathlib import Path

from framework.utils import chroot
from host_tools import cargo_build

SOCKET_PATH = "/firecracker-mem-backend.sock"
OUTPUT_DIR = "mem_backend"


class MemoryBackend:
    """Describe the memory backend process."""

    def __init__(self, name, socket_path, chroot_path, log_file_name, backing_mem):
        """Instantiate the backend process with arguments."""
        self._proc = None
        self._binary_name = name
        self.socket_path = socket_path
        self._chroot = Path(chroot_path)
        self._log_file = log_file_name
        # Memory file (inside the chroot) to serve page faults from, if any.
        self._backing_mem = backing_mem
        self.snapshots_taken = 0
        self.precopy_passes = 0

    @property
    def output_dir(self):
        """Host path of the directory the backend writes snapshots to."""
        return self._chroot / OUTPUT_DIR

    def spawn(self, uid, gid):
        """Spawn the backend process."""
        with chroot(self._chroot):
            st = os.stat(self._binary_name)
            os.chmod(self._binary_name, st.st_mode | stat.S_IEXEC)

            chroot_log_file = Path("/") / self._log_file
            with open(chroot_log_file, "w", encoding="utf-8") as logfile:
                args = [f"/{self._binary_name}", self.socket_path, f"/{OUTPUT_DIR}"]
                if self._backing_mem is not None:
                    args.append(str(self._backing_mem))
                self._proc = subprocess.Popen(
                    args, stdout=logfile, stderr=subprocess.STDOUT
                )

            # Give it time to start and fail, if it really has to.
            time.sleep(1)
            if not self.is_running():
                print(chroot_log_file.read_text(encoding="utf-8"))
                assert False, "Could not start the memory backend!"

            # The backend creates the socket with root rights; make it the jailer's.
            os.chown(self.socket_path, uid, gid)
            os.chown(f"/{OUTPUT_DIR}", uid, gid)

    def next_snapshot_mem(self) -> Path:
        """Return the host path of the memory file the next snapshot will produce.

        Call it right before requesting the snapshot; it advances the counter.
        """
        path = self.output_dir / f"mem.{self.snapshots_taken}"
        self.snapshots_taken += 1
        return path

    @property
    def precopy_mem(self) -> Path:
        """Host path of the image accumulating the pre-copy passes."""
        return self.output_dir / "precopy.mem"

    def request_precopy_pass(self, timeout=30):
        """Ask the backend for a pre-copy pass and wait for it to complete.

        The backend requests the dirty ranges from Firecracker (`GetDirtyRanges`,
        while the microVM may be running) and copies them into `precopy_mem`.
        Returns `(ranges, bytes)` copied by this pass.
        """
        self.precopy_passes += 1
        marker = self.output_dir / f"precopy.{self.precopy_passes}.done"
        assert self.is_running(), self.log_data
        self.proc.send_signal(signal.SIGUSR1)
        deadline = time.time() + timeout
        while not marker.exists():
            assert self.is_running(), self.log_data
            assert time.time() < deadline, f"pre-copy pass timed out\n{self.log_data}"
            time.sleep(0.05)
        ranges, nbytes = marker.read_text(encoding="utf-8").split()
        return int(ranges), int(nbytes)

    @property
    def proc(self):
        """Return the backend process."""
        return self._proc

    def is_running(self):
        """Check if the backend process is running."""
        return self.proc is not None and self.proc.poll() is None

    @property
    def log_data(self):
        """Return the log data of the backend."""
        return (self._chroot / self._log_file).read_text(encoding="utf-8")

    def kill(self):
        """Kill the backend process."""
        if self.is_running():
            self.proc.kill()
            self.proc.wait(timeout=5)
        self._proc = None

    def __del__(self):
        """Tear down the backend process."""
        if self.is_running():
            self.kill()


def spawn_memory_backend(vm, backing_snapshot=None):
    """Spawn a memory backend process for `vm`.

    `backing_snapshot` is the jailed snapshot page faults are served from when
    restoring; `None` when booting.
    """
    binary = cargo_build.get_example(
        "memory_backend", binary_dir=vm.fc_binary_path.parent
    )
    jailed_binary = vm.create_jailed_resource(binary)
    backing_mem = (
        Path("/") / backing_snapshot.mem.name if backing_snapshot is not None else None
    )
    backend = MemoryBackend(
        os.path.basename(jailed_binary),
        SOCKET_PATH,
        vm.chroot(),
        "mem_backend.log",
        backing_mem,
    )
    backend.spawn(vm.jailer.uid, vm.jailer.gid)
    return backend
