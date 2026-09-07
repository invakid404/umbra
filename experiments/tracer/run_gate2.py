#!/usr/bin/python3
"""Run Track D with independent host/shadow content checks and saved logs."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

CASES = [
    ("open-libc", "libc open interception + rewrite", b"libc\n"),
    ("open-svc", "raw-svc open interception", b"libc\n"),
    ("fork-write", "fork + immediate child write", b"fork\n"),
    ("posix-spawn-write", "posix_spawn + child write", b"libc\n"),
    ("exec-write", "exec + child write", b"libc\n"),
    ("grandchild-write", "grandchild write", b"grandchild\n"),
]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path, default=Path.home() / "umbra-scratch/fixtures/umbra-test-child")
    parser.add_argument("--redirect-root", default="/tmp/umbra-nfs-stub")
    parser.add_argument("--timeout", type=float, default=10)
    opts = parser.parse_args()
    here = Path(__file__).resolve().parent
    results = here / "results"
    results.mkdir(exist_ok=True)
    rows = []
    for case, label, expected in CASES:
        row = {"case": case, "label": label}
        if not opts.fixture.is_file():
            row.update(status="not tested", reason="Track D fixture unavailable")
        else:
            with tempfile.TemporaryDirectory(prefix="umbra-gate2-", dir="/tmp") as directory:
                host = Path(directory) / case
                shadow_tree = Path(opts.redirect_root).absolute() / directory.lstrip("/")
                shadow = shadow_tree / case
                baseline = Path(directory) / "baseline"
                control = subprocess.run([str(opts.fixture), case, str(baseline)], capture_output=True, timeout=10)
                baseline_ok = control.returncode == 0 and baseline.is_file() and baseline.read_bytes() == expected
                row["baseline_pass"] = baseline_ok
                command = [sys.executable, str(here / "umbra_tracer.py"), "--timeout", str(opts.timeout),
                           "--redirect-root", opts.redirect_root, str(opts.fixture), case, str(host)]
                try:
                    with (results / (case + ".log")).open("w") as output:
                        output.write("COMMAND: " + repr(command) + "\n")
                        output.write(f"BASELINE: {'PASS' if baseline_ok else 'FAIL'}\n")
                        output.flush()
                        run = subprocess.run(command, stdout=output, stderr=subprocess.STDOUT)
                    text = (results / (case + ".log")).read_text()
                    row.update(exit_code=run.returncode, host_exists=os.path.lexists(host),
                               shadow_exists=os.path.lexists(shadow),
                               shadow_content_matches=shadow.is_file() and shadow.read_bytes() == expected)
                    if row["host_exists"]:
                        row.update(status="MISSED", reason="fixture created its host output outside the redirect root")
                    elif baseline_ok and run.returncode == 0 and row["shadow_content_matches"] and "OPEN[new]:" in text:
                        row.update(status="CAPTURED", reason="expected bytes in shadow, host output absent, tracee exited 0")
                    elif run.returncode == 124 and "LAUNCHED:" not in text and "LAUNCH:" in text:
                        row.update(status="PARTIAL", reason="fixture available; LLDB launch timed out before task control; interception not exercised")
                    else:
                        row.update(status="PARTIAL", reason=f"no host output, but capture not proven (exit={run.returncode}); see results/{case}.log")
                finally:
                    # This path includes a fresh mkdtemp name owned only by this run.
                    if shadow_tree.exists():
                        shutil.rmtree(shadow_tree)
        rows.append(row)
        print(f"{label}: {row['status']} — {row['reason']}", flush=True)
    (results / "matrix.json").write_text(json.dumps(rows, indent=2) + "\n")
    (results / "matrix.txt").write_text("\n".join(f"{r['label']}: {r['status']} — {r['reason']}" for r in rows) + "\n")
    return 0 if all(r["status"] == "CAPTURED" for r in rows) else 1


if __name__ == "__main__":
    sys.exit(main())
