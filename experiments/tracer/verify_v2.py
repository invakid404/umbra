#!/usr/bin/python3
"""TASK-v2 fixed-path verification, including bytes and process exit status."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys


CASES = [
    ("open-libc", b"libc\n"),
    ("open-svc", b"libc\n"),
    ("fork-write", b"fork\n"),
    ("posix-spawn-write", b"libc\n"),
    ("exec-write", b"libc\n"),
    ("grandchild-write", b"grandchild\n"),
]


def main():
    here = Path(__file__).resolve().parent
    tracer = here / "umbra_tracer.py"
    fixture = Path.home() / "umbra-scratch/fixtures/umbra-test-child"
    root = Path("/tmp/umbra-nfs-stub")
    root.mkdir(parents=True, exist_ok=True)
    results = here / "results/v2"
    results.mkdir(parents=True, exist_ok=True)
    rows = []
    for case, expected in CASES:
        host = Path("/tmp/umbra-gate2-v2-" + case)
        shadow = root / str(host).lstrip("/")
        for path in (host, shadow):
            path.unlink(missing_ok=True)
        argv = ["/usr/bin/python3", str(tracer), "--timeout", "25",
                str(fixture), case, str(host)]
        output = results / (case + ".log")
        with output.open("w") as stream:
            stream.write("COMMAND: " + repr(argv) + "\n")
            stream.flush()
            run = subprocess.run(argv, stdout=stream, stderr=subprocess.STDOUT)
        log = output.read_text()
        row = {"case": case, "exit_code": run.returncode,
               "host": str(host), "shadow": str(shadow),
               "host_present": os.path.lexists(host),
               "shadow_present": os.path.lexists(shadow),
               "content_matches": shadow.is_file() and shadow.read_bytes() == expected}
        passed = (run.returncode == 0 and not row["host_present"] and
                  row["content_matches"] and "OPEN[new]: " + str(shadow) in log)
        row["status"] = "CAPTURED" if passed else "MISSED"
        rows.append(row)
        print("\n" + case, flush=True)
        print("\n".join(log.splitlines()[-20:]), flush=True)
        print(f"HOST present? {'YES' if row['host_present'] else 'NO'}", flush=True)
        print(f"SHADOW present? {'YES' if row['shadow_present'] else 'NO'}", flush=True)
        print(f"RESULT: {row['status']}; exit={run.returncode}; bytes_match={row['content_matches']}", flush=True)
    report = {"tracer_sha256": hashlib.sha256(tracer.read_bytes()).hexdigest(), "cases": rows}
    (results / "matrix.json").write_text(json.dumps(report, indent=2) + "\n")
    return 0 if all(row["status"] == "CAPTURED" for row in rows) else 1


if __name__ == "__main__":
    sys.exit(main())
