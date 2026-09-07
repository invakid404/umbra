#!/usr/bin/env python3
"""Mount-table checks avoid touching a possibly unresponsive NFS filesystem."""
import os
import re
import subprocess
import sys


def source(path):
    table = subprocess.check_output(["/sbin/mount"], text=True)
    for line in table.splitlines():
        match = re.fullmatch(r"(.+) on (.+) \((.+)\)", line)
        if match and match[2] == path:
            return match[1], match[3]
    return "", ""


def verify(path):
    remote, flags = source(path)
    if remote != "127.0.0.1:/umbra" or "nfs" not in flags.split(", "):
        raise RuntimeError(f"expected NFS export is not mounted at {path}")
    info = subprocess.check_output(["/usr/bin/nfsstat", "-m"], text=True)
    blocks = re.split(r"\n\s*\n", info)
    block = next((b for b in blocks if b.startswith(path + " from ")), "")
    current = block.partition("-- Current mount parameters:")[2]
    match = re.search(r"NFS parameters: (.+)", current)
    options = set(match[1].split(",")) if match else set()
    required = {"vers=4.0", "tcp", "port=2049", "hard", "intr", "noresvport",
                "nocallback", "nonegnamecache", "acregmin=0", "acregmax=0",
                "acdirmin=0", "acdirmax=0", "acrootdirmin=0", "acrootdirmax=0"}
    if not required <= options:
        raise RuntimeError("unexpected NFS mount settings; missing " +
                           ",".join(sorted(required - options)) +
                           "; stop mount users, run down.sh, then up.sh")
    if "read-only" in flags:
        raise RuntimeError("export is mounted read-only")
    return block


if __name__ == "__main__":
    try:
        action = sys.argv[1]
        if action == "source":
            print(source(sys.argv[2])[0])
        elif action == "verify":
            print(verify(sys.argv[2]))
        elif action == "probe":
            os.stat(sys.argv[2])
            os.listdir(sys.argv[2])
        elif action == "timeout":
            child = subprocess.Popen(sys.argv[3:])
            try:
                sys.exit(child.wait(timeout=float(sys.argv[2])))
            except subprocess.TimeoutExpired:
                child.kill()
                print("FAIL operation timed out: " + sys.argv[3], file=sys.stderr)
                sys.exit(124)
        else:
            raise RuntimeError("unknown action")
    except (OSError, RuntimeError, subprocess.SubprocessError) as exc:
        print(f"FAIL {exc}", file=sys.stderr)
        sys.exit(1)
