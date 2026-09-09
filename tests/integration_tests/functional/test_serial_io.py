# Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Tests scenario for the Firecracker serial console."""

import fcntl
import os
import platform
import re
import signal
import termios
import time
from pathlib import Path

import pytest

from framework import utils
from framework.artifacts import GUEST_KERNEL_DEFAULT, pin_guest_kernel
from framework.microvm import Serial
from framework.utils_cpu_templates import ALL_CPU_TEMPLATES, pin_cpu_template

PLATFORM = platform.machine()


@pin_guest_kernel(GUEST_KERNEL_DEFAULT)
def test_serial_after_snapshot(uvm, microvm_factory):
    """
    Serial I/O after restoring from a snapshot.
    """
    microvm = uvm
    microvm.help.enable_console()
    microvm.spawn(serial_out_path=None)
    microvm.basic_config(
        vcpu_count=2,
        mem_size_mib=256,
    )
    serial = Serial(microvm)
    serial.open()
    microvm.start()

    # looking for the # prompt at the end
    serial.rx(microvm.distro.shell_prompt)

    # Create snapshot.
    snapshot = microvm.snapshot_full()
    # Kill base microVM.
    microvm.kill()

    # Load microVM clone from snapshot.
    vm = microvm_factory.build()
    vm.help.enable_console()
    vm.spawn(serial_out_path=None)
    vm.restore_from_snapshot(snapshot, resume=True)
    serial = Serial(vm)
    serial.open()
    # After restore, the kernel may emit messages (e.g. crng reseeded on 6.1)
    # that hold the console lock. Wait for those to finish before sending input.
    serial.drain_until_idle()
    serial.tx("")
    serial.rx(vm.distro.shell_prompt)
    serial.tx("pwd")
    res = serial.rx("#")
    assert "/root" in res


# The VM can become unresponsive in the test due to the interrupt storm .
@pytest.mark.flaky(reruns=2)
@pin_guest_kernel(GUEST_KERNEL_DEFAULT)
def test_serial_active_tx_snapshot(uvm, microvm_factory):
    """
    Snapshot a guest that is actively transmitting on the serial console and
    test that the transmission continues after snapshot restore.
    """
    microvm = uvm
    microvm.help.enable_console()
    microvm.spawn(serial_out_path=None)
    microvm.basic_config(
        vcpu_count=2,
        mem_size_mib=256,
    )
    serial = Serial(microvm)
    serial.open()
    microvm.start()

    # looking for the # prompt at the end
    serial.rx(microvm.distro.shell_prompt)

    # Start an unbounded serial transmission from inside the guest such that
    # there will be an active transmission at the point of pausing the VM to
    # take the snapshot. This will saturate the TX buffer of the UART and it
    # might make the guest driver enable TX interrupts.
    serial.tx("cat /dev/zero")
    # Give the guest time to start the transmission
    time.sleep(1)

    # Create snapshot.
    snapshot = microvm.snapshot_full()
    # Kill base microVM.
    microvm.kill()

    # Load microVM clone from snapshot.
    vm = microvm_factory.build()
    vm.help.enable_console()
    vm.spawn(serial_out_path=None)
    vm.restore_from_snapshot(snapshot, resume=True)
    serial = Serial(vm)
    serial.open()

    # Send Ctrl-C to the guest to stop the ongoing transmission and regain the shell
    serial.tx("\x03", end="")
    # looking for the # prompt at the end
    serial.rx(vm.distro.shell_prompt)
    serial.tx("pwd")
    res = serial.rx("#")
    assert "/root" in res


def test_serial_console_login(uvm):
    """
    Test serial console login.
    """
    microvm = uvm
    microvm.help.enable_console()
    microvm.spawn(serial_out_path=None)

    # We don't need to monitor the memory for this test because we are
    # just rebooting and the process dies before pmap gets the RSS.
    microvm.memory_monitor = None

    # Set up the microVM with 1 vCPU and a serial console.
    microvm.basic_config(vcpu_count=1)

    microvm.start()

    serial = Serial(microvm)
    serial.open()
    serial.rx(microvm.distro.shell_prompt)
    serial.tx("id")
    serial.rx("uid=0(root) gid=0(root) groups=0(root)")


# A printk line as it appears in the screen log: "[   12.345678] xxxx\r\r\n".
# serial8250_console_write() emits each message atomically under the port lock,
# so these never get split, but they may split the shell's own output.
KMSG_LINE_RE = re.compile(r"\[ *\d+\.\d+\] x*\r*\n")


@pin_guest_kernel(GUEST_KERNEL_DEFAULT)
def test_serial_input_during_console_printk(uvm):
    """
    Serial input must not be lost while the guest kernel writes to the console.

    serial8250_console_write() masks the UART interrupts (IER=0) for the
    duration of a printk and restores them afterwards, relying on the UART to
    re-assert the RX interrupt if data arrived meanwhile. Flood the console
    with printk messages so that IER is masked most of the time, and check
    that input injected from the host is still processed by the guest.

    Regression test for the intermittent `test_serial_console_login` /
    `test_serial_after_snapshot` failures where journald output raced with
    the test's input (FC-OPS-3173).
    """
    vm = uvm
    vm.help.enable_console()
    vm.spawn(serial_out_path=None)
    vm.memory_monitor = None
    # 1 vCPU: the same vCPU that writes the printk would have to take the RX
    # interrupt, which is the configuration the flaky tests use.
    vm.basic_config(vcpu_count=1)
    vm.add_net_iface()
    vm.start()

    serial = Serial(vm)
    serial.open()
    serial.rx(vm.distro.shell_prompt)

    # Lift the per-fd rate limit on /dev/kmsg and start a background flood of
    # 200-byte printk messages. Each one keeps IER masked for the whole
    # console write of that message.
    vm.ssh.check_output("echo on > /proc/sys/kernel/printk_devkmsg")
    vm.ssh.check_output(
        'nohup sh -c \'msg=$(head -c 200 /dev/zero | tr "\\0" x); '
        "while :; do echo $msg; done > /dev/kmsg' >/dev/null 2>&1 </dev/null &"
    )
    # Give the flood time to start before injecting input.
    time.sleep(0.5)

    # Serial.rx_char() reads one byte per poll(), which cannot keep up with
    # the flood. Read the console log in chunks instead, starting from the
    # current end of the file.
    log_fd = os.open(vm.screen_log, os.O_RDONLY)
    os.lseek(log_fd, 0, os.SEEK_END)

    def rx_until(token, timeout):
        """Read the console until `token` shows up in the non-printk output."""
        buf = ""
        deadline = time.time() + timeout
        while time.time() < deadline:
            chunk = os.read(log_fd, 1 << 16)
            if not chunk:
                time.sleep(0.05)
                continue
            buf += chunk.decode("utf-8", errors="ignore")
            if token in KMSG_LINE_RE.sub("", buf):
                return True
            # Keep the tail only; a printk line is < 256 bytes so the token
            # cannot straddle more than that.
            buf = buf[-4096:]
        return False

    trials = 10
    lost = []
    for i in range(trials):
        # The echoed command line reads `echo pi''ng<i>`, so the token can
        # only come from the command actually being executed.
        serial.tx(f"echo pi''ng{i}")
        # Under the printk flood the shell is slow, but a response within a
        # few seconds is normal. No echo at all means the input was never
        # delivered to the guest.
        if not rx_until(f"ping{i}", timeout=5):
            lost.append(i)

    vm.ssh.check_output("pkill -f 'while :'")
    assert not lost, f"guest lost serial input in {len(lost)}/{trials} trials: {lost}"


def get_total_mem_size(pid):
    """Get total memory usage for a process."""
    cmd = f"pmap {pid} | tail -n 1 | sed 's/^ //' | tr -s ' ' | cut -d' ' -f2"
    _, stdout, stderr = utils.check_output(cmd)
    assert stderr == ""

    # This assumes that the pmap returns something in the form of
    # 123456789K (which is typically the case for us)
    return float(stdout.strip()[:-1] * 1000)


def send_bytes(tty, bytes_count, timeout=60):
    """Send data to the terminal."""
    start = time.time()
    for _ in range(bytes_count):
        fcntl.ioctl(tty, termios.TIOCSTI, "\n")
        current = time.time()
        if current - start > timeout:
            break


def test_serial_dos(uvm):
    """
    Test serial console behavior under DoS.
    """
    microvm = uvm
    microvm.help.enable_console()
    microvm.spawn()

    # Set up the microVM with 1 vCPU and a serial console.
    microvm.basic_config(
        vcpu_count=1,
    )
    microvm.add_net_iface()
    microvm.start()

    # Open an fd for firecracker process terminal.
    tty_path = f"/proc/{microvm.firecracker_pid}/fd/0"
    tty_fd = os.open(tty_path, os.O_RDWR)

    # Check if the total memory size changed.
    before_size = get_total_mem_size(microvm.firecracker_pid)
    send_bytes(tty_fd, 100000000, timeout=1)
    after_size = get_total_mem_size(microvm.firecracker_pid)
    # Give the check a bit of tolerance (1%) since sometimes random unrelated
    # allocations break it.
    assert after_size <= (
        before_size * 1.01
    ), "The memory size of the Firecracker process changed from {} to {}.".format(
        before_size, after_size
    )


def test_serial_block(uvm):
    """
    Test that writing to stdout never blocks the vCPU thread.
    """
    test_microvm = uvm
    test_microvm.help.enable_console()
    test_microvm.spawn(serial_out_path=None)
    # Set up the microVM with 1 vCPU so we make sure the vCPU thread
    # responsible for the SSH connection will also run the serial.
    test_microvm.basic_config(
        vcpu_count=1,
        mem_size_mib=512,
    )
    test_microvm.add_net_iface()
    test_microvm.start()

    # Get an initial reading of missed writes to the serial.
    fc_metrics = test_microvm.flush_metrics()
    init_count = fc_metrics["uart"]["missed_write_count"]

    # Stop `screen` process which captures stdout so we stop consuming stdout.
    os.kill(test_microvm.screen_pid, signal.SIGSTOP)

    # Generate a random text file.
    test_microvm.ssh.check_output(
        "base64 /dev/urandom | head -c 100000 > /tmp/file.txt"
    )

    # Dump output to terminal
    test_microvm.ssh.check_output("cat /tmp/file.txt > /dev/ttyS0")

    # Check that the vCPU isn't blocked.
    test_microvm.ssh.check_output("cd /")

    # Check the metrics to see if the serial missed bytes.
    fc_metrics = test_microvm.flush_metrics()
    last_count = fc_metrics["uart"]["missed_write_count"]

    # Should be significantly more than before the `cat` command.
    assert last_count - init_count > 10000


REGISTER_FAILED_WARNING = "Failed to register serial input fd: event_manager: failed to manage epoll file descriptor: Operation not permitted (os error 1)"


@pin_guest_kernel(GUEST_KERNEL_DEFAULT)
def test_no_serial_fd_error_when_daemonized(uvm):
    """
    Tests that when running firecracker daemonized, the serial device
    does not try to register stdin to epoll (which would fail due to stdin no
    longer being pointed at a terminal).

    Regression test for #4037.
    """

    test_microvm = uvm
    test_microvm.spawn()
    test_microvm.add_net_iface()
    test_microvm.basic_config(
        vcpu_count=1,
        mem_size_mib=512,
    )
    test_microvm.start()

    assert REGISTER_FAILED_WARNING not in test_microvm.log_data


@pin_cpu_template(ALL_CPU_TEMPLATES)
def test_serial_file_output(uvm_any):
    """Test that redirecting serial console output to a file works for booted and restored VMs"""
    uvm_any.ssh.check_output("echo 'hello' > /dev/ttyS0")

    assert b"hello" in uvm_any.serial_out_path.read_bytes()


@pin_guest_kernel(GUEST_KERNEL_DEFAULT)
def test_serial_rate_limiting(uvm):
    """Test that serial output is rate-limited when a rate limiter is configured."""
    microvm = uvm
    microvm.spawn()
    microvm.add_net_iface()
    microvm.basic_config(vcpu_count=1, mem_size_mib=256)

    # Configure serial output to a file with a rate limiter:
    # 1 KiB/sec sustained, 64 KiB one-time burst.
    serial_path = Path(microvm.path) / "serial.log"
    serial_path.touch()
    microvm.create_jailed_resource(serial_path)
    microvm.api.serial.put(
        serial_out_path="serial.log",
        rate_limiter={"size": 1024, "one_time_burst": 65536, "refill_time": 1000},
    )
    microvm.start()

    size_before = serial_path.stat().st_size

    # Write a large payload (~1MB) from the guest to the serial port.
    microvm.ssh.check_output("base64 /dev/urandom | head -c 1000000 > /dev/ttyS0")

    # Wait for any in-flight writes to settle.
    time.sleep(2)

    # With 64 KiB burst + ~2s at 1 KiB/sec, output should be well under 80 KB.
    new_bytes = serial_path.stat().st_size - size_before
    assert (
        new_bytes < 80000
    ), f"Serial output is {new_bytes} bytes, expected under 80000 due to rate limiting"

    # Verify the rate_limiter_dropped_bytes metric was incremented.
    fc_metrics = microvm.flush_metrics()
    assert fc_metrics["uart"]["rate_limiter_dropped_bytes"] > 0
