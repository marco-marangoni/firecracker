# Temporary experiment, not for merging.
"""aarch64: serial TX deferred to a kworker when the port is runtime-suspended.
Phases: A = original single read (expect misses), B = PM forced on (control),
D = explicit drain via stty/tcsetattr(TCSADRAIN) (expect 0 misses)."""

import time

from framework.artifacts import kernel_params, pin_guest_kernel

PORT_DEV = "$(readlink -f /sys/class/tty/ttyS0/device)"
PM_DIR = f"{PORT_DEV}/power"
SPIN_S = 2
DRAIN = "stty -F /dev/ttyS0 $(stty -F /dev/ttyS0 -g)"


def _write(vm, marker, drain):
    """SCHED_FIFO writer on guest CPU 0 that keeps spinning for SPIN_S after
    write() (and drain, if any) so normal-prio kworkers on CPU 0 can't run.
    ssh returns as soon as the write (+drain) is done; the spinner is left
    running in the background."""
    body = f"echo '{marker}' > /dev/ttyS0" + (f"; {DRAIN}" if drain else "")
    cmd = (
        f"sleep 0.7; cat {PM_DIR}/runtime_status; "
        f"chrt -f 50 taskset -c 0 sh -c \"{body}; "
        f"t=\\$(date +%s); while [ \\$(( \\$(date +%s) - t )) -lt {SPIN_S} ]; do :; done\" "
        "</dev/null >/dev/null 2>&1 & sleep 0.2"
    )
    return vm.ssh.check_output(cmd).stdout.strip().splitlines()[-1]


def _trials(vm, n, label, drain=False):
    misses, late = 0, []
    for i in range(n):
        marker = f"{label}{i:02d}"
        status = _write(vm, marker, drain)
        t0 = time.monotonic()
        if marker.encode() not in vm.serial_out_path.read_bytes():
            misses += 1
            while marker.encode() not in vm.serial_out_path.read_bytes():
                time.sleep(0.01)
                assert time.monotonic() - t0 < 30
            late.append(round((time.monotonic() - t0) * 1000))
        time.sleep(SPIN_S + 0.2)
    print(f"\n[{label}] pm_status_before_write={status} drain={drain} misses={misses}/{n} late_ms={late}")
    return misses


@pin_guest_kernel(list(kernel_params("vmlinux-6.18*")))
def test_repro(uvm_booted):
    vm = uvm_booted
    vm.ssh.check_output("echo -1 > /proc/sys/kernel/sched_rt_runtime_us")
    print("\n" + vm.ssh.check_output(
        f"phys={PORT_DEV}/../..; echo phys_driver=$(basename $(readlink -f $phys/driver)) "
        f"phys_pm=$(cat $phys/power/runtime_status) port_delay_ms=$(cat {PM_DIR}/autosuspend_delay_ms)"
    ).stdout)
    a = _trials(vm, 10, "A")                 # PM auto, single read
    d = _trials(vm, 10, "D", drain=True)     # PM auto, drain, single read
    vm.ssh.check_output(f"echo on > {PM_DIR}/control")
    b = _trials(vm, 5, "B")                  # PM on, single read (control)
    print(f"\nRESULT A(auto,no drain)={a}/10  D(auto,drain)={d}/10  B(pm on)={b}/5 misses")
    assert a > 0 and d == 0 and b == 0
