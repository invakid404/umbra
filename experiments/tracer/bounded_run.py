#!/usr/bin/python3
"""Bound an experiment, including LLDB calls that block before returning.

Only signal this subprocess and descendants observed while it was alive.
Validate each PID's start time and command again before cleanup.
"""
import argparse
import os
import signal
import subprocess
import sys
import time


def snapshot():
    output = subprocess.check_output(
        ["/bin/ps", "-axo", "pid=,ppid=,lstart=,command="], text=True)
    rows = {}
    for line in output.splitlines():
        parts = line.split(None, 7)
        if len(parts) == 8:
            rows[int(parts[0])] = (int(parts[1]), " ".join(parts[2:7]), parts[7])
    return rows


def run(argv, timeout):
    snapshot()  # Fail before launch if process-tree inspection is unavailable.
    child = subprocess.Popen(argv, start_new_session=True)
    known = {}
    deadline = time.monotonic() + timeout
    try:
        while True:
            rows = snapshot()
            roots = {child.pid} | {pid for pid in known if pid in rows and rows[pid][1:] == known[pid]}
            changed = True
            while changed:
                changed = False
                for pid, row in rows.items():
                    if pid == child.pid or row[0] in roots:
                        if pid not in roots:
                            roots.add(pid)
                            changed = True
                        known[pid] = row[1:]
            status = child.poll()
            if status is not None:
                return status
            if time.monotonic() >= deadline:
                print(f"TIMEOUT: {timeout}s: {argv[0]}", flush=True)
                return 124
            time.sleep(0.1)
    finally:
        rows = snapshot()
        for pid in reversed(list(known)):
            if pid in rows and rows[pid][1:] == known[pid]:
                try:
                    os.kill(pid, signal.SIGKILL)
                    print(f"CLEANUP: killed owned pid={pid}", flush=True)
                except ProcessLookupError:
                    pass
        if child.poll() is None:
            child.kill()
        child.wait()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--timeout", type=float, default=30)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    sys.exit(run(args.command, args.timeout))
