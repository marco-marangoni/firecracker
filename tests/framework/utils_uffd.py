# Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""UFFD related utility functions"""

import json
import os
import socket
import stat
import subprocess
import time
from pathlib import Path

from framework.utils import chroot
from host_tools import cargo_build

SOCKET_PATH = "/firecracker-uffd.sock"
CONTROL_SOCKET_PATH = "/firecracker-uffd-control.sock"


class UffdHandler:
    """Describe the UFFD page fault handler process.

    The same example handler binary also acts as a *memory backend*: when
    started with a control socket, it keeps the guest memory memfd Firecracker
    hands over in the handshake and copies snapshot pages out of it on request
    (see `copy`). A handler started for a boot receives only the memfd and needs
    no snapshot memory file.
    """

    def __init__(
        self,
        name,
        socket_path,
        snapshot: "Snapshot",
        chroot_path,
        log_file_name,
        control_socket_path=None,
    ):
        """Instantiate the handler process with arguments.

        `snapshot` may be `None` when the handler is started as a memory backend
        for a boot, in which case there is nothing to populate page faults from.
        """
        self._proc = None
        self._handler_name = name
        self.socket_path = socket_path
        self.control_socket_path = control_socket_path
        self.snapshot = snapshot
        self._chroot = chroot_path
        self._log_file = log_file_name

    def spawn(self, uid, gid):
        """Spawn handler process using arguments provided."""

        with chroot(self._chroot):
            st = os.stat(self._handler_name)
            os.chmod(self._handler_name, st.st_mode | stat.S_IEXEC)

            chroot_log_file = Path("/") / self._log_file
            with open(chroot_log_file, "w", encoding="utf-8") as logfile:
                args = [f"/{self._handler_name}", self.socket_path]
                if self.snapshot is not None:
                    args.append(self.snapshot.mem.name)
                if self.control_socket_path is not None:
                    args += ["--control-sock", self.control_socket_path]
                self._proc = subprocess.Popen(
                    args, stdout=logfile, stderr=subprocess.STDOUT
                )

            # Give it time start and fail, if it really has too (bad things happen).
            time.sleep(1)
            if not self.is_running():
                print(chroot_log_file.read_text(encoding="utf-8"))
                assert False, "Could not start PF handler!"

            # The page fault handler will create the socket path with root rights.
            # Change rights to the jailer's.
            os.chown(self.socket_path, uid, gid)

    @property
    def proc(self):
        """Return UFFD handler process."""
        return self._proc

    def is_running(self):
        """Check if UFFD process is running"""
        return self.proc is not None and self.proc.poll() is None

    @property
    def log_file(self):
        """Return the path to the UFFD handler's log file"""
        return Path(self._chroot) / Path(self._log_file)

    @property
    def log_data(self):
        """Return the log data of the UFFD handler"""
        if self.log_file is None:
            return ""
        return self.log_file.read_text(encoding="utf-8")

    def control(self, request: dict) -> dict:
        """Send a request on the handler's control socket and return its reply.

        The protocol is one JSON request per connection: send, shut down the
        write side, read the JSON reply until EOF.
        """
        assert self.control_socket_path is not None, "handler has no control socket"
        host_path = Path(self._chroot) / self.control_socket_path.lstrip("/")
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
            sock.settimeout(120)
            sock.connect(str(host_path))
            sock.sendall(json.dumps(request).encode())
            sock.shutdown(socket.SHUT_WR)
            raw = b""
            while chunk := sock.recv(65536):
                raw += chunk
        return json.loads(raw)

    def copy(self, memory: dict, mem_path: str):
        """Ask the handler to write the `memory` object returned by Firecracker
        (`PUT /snapshot/create` or `PUT /snapshot/dirty-pages`) into `mem_path`,
        a path inside the handler's chroot. Creates the file at `total_size` or
        merges into an existing one.
        """
        reply = self.control({"Copy": {"mem_path": mem_path, "memory": memory}})
        done = reply["Done"]
        assert done["success"], f"memory backend copy failed: {done['message']}"
        return done

    def kill(self):
        """Kills the uffd handler process"""
        assert self.is_running()

        self.proc.kill()
        self.proc.wait(timeout=5)
        self._proc = None

    def mark_killed(self, timeout=10):
        """Wait for the uffd handler to exit on its own, and mark it dead.

        Used when the handler is expected to die by itself (e.g. the malicious
        handler panics on the first page fault). Process exit after a panic is
        not instantaneous, so wait for a given timeout duration before raising.
        """
        if self._proc is not None:
            try:
                self._proc.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                raise AssertionError(
                    f"UFFD handler '{self._handler_name}' still running "
                    f"{timeout}s after it should have died. Logs:\n{self.log_data}"
                ) from None

        self._proc = None

    def __del__(self):
        """Tear down the UFFD handler process."""
        if self.is_running():
            self.kill()


def spawn_pf_handler(vm, handler_path, jailed_snapshot, mem_backend=False):
    """Spawn page fault handler process.

    `jailed_snapshot` is the snapshot to populate page faults from, or `None`
    for a memory backend started for a boot. With `mem_backend`, the handler
    also listens on a control socket so the test can ask it to copy snapshot
    pages out of the shared guest memory.
    """
    # Copy snapshot memory file into chroot of microVM.
    # Copy the valid page fault binary into chroot of microVM.
    jailed_handler = vm.create_jailed_resource(handler_path)
    handler_name = os.path.basename(jailed_handler)

    uffd_handler = UffdHandler(
        handler_name,
        SOCKET_PATH,
        jailed_snapshot,
        vm.chroot(),
        "uffd.log",
        control_socket_path=CONTROL_SOCKET_PATH if mem_backend else None,
    )
    uffd_handler.spawn(vm.jailer.uid, vm.jailer.gid)

    return uffd_handler


def uffd_handler(handler_name, **kwargs):
    """Retrieves the uffd handler with the given name"""
    return cargo_build.get_example(f"uffd_{handler_name}_handler", **kwargs)
