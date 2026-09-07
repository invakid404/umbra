#!/usr/bin/env python3
"""Exercise the real macOS mount and independently inspect container bytes."""
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile

mount = Path(sys.argv[1])
task = Path(sys.argv[2])
spec = importlib.util.spec_from_file_location("host_check", task / "host-check.py")
checks = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checks)
compose = ["docker", "compose", "--project-directory", str(task), "-f",
           str(task / "docker-compose.yml"), "exec", "-T", "nfs"]
path = None
assertion = "expected read/write NFSv4 mount"
try:
    checks.verify(str(mount))
    print(f"PASS {assertion}", flush=True)
    assertion = "NFSv4 reachable; NFSv3 rejected"
    subprocess.run([sys.executable, str(task / "rpc-check.py")], check=True,
                   stdout=subprocess.DEVNULL, timeout=10)
    print(f"PASS {assertion}", flush=True)
    assertion = "Mac write and fsync"
    payload = b"umbra NFS host smoke\n" + os.urandom(65536) + b"\x00\xff\n"
    fd, name = tempfile.mkstemp(prefix=".umbra-smoke-", dir=mount)
    path = Path(name)
    with os.fdopen(fd, "wb") as stream:
        stream.write(payload)
        stream.flush()
        os.fsync(stream.fileno())
    print(f"PASS {assertion}", flush=True)
    assertion = "Mac readback matches all bytes"
    if path.read_bytes() != payload:
        raise RuntimeError("content mismatch")
    print(f"PASS {assertion}", flush=True)
    assertion = "container /export/umbra file matches all bytes"
    remote = "/export/umbra/" + path.name
    observed = subprocess.check_output(compose + ["cat", remote], timeout=10)
    if observed != payload:
        raise RuntimeError("container content mismatch")
    print(f"PASS {assertion}", flush=True)
    assertion = "container update visible on Mac"
    replacement = b"updated inside container\n"
    subprocess.run(compose + ["python3", "-c",
        "import os,sys; f=open(sys.argv[1],'wb'); f.write(sys.stdin.buffer.read()); "
        "f.flush(); os.fsync(f.fileno()); f.close()", remote],
        input=replacement, check=True, timeout=10)
    if path.read_bytes() != replacement:
        raise RuntimeError("stale client content")
    print(f"PASS {assertion}", flush=True)
    assertion = "Mac unlink visible in container"
    path.unlink()
    subprocess.run(compose + ["test", "!", "-e", remote], check=True, timeout=10)
    print(f"PASS {assertion}", flush=True)
    print("PASS", flush=True)
except (OSError, RuntimeError, subprocess.SubprocessError) as exc:
    print(f"FAIL {assertion}: {exc}", flush=True)
    sys.exit(1)
finally:
    if path is not None:
        try:
            path.unlink(missing_ok=True)
        except OSError:
            pass
