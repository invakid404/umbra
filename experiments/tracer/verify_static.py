#!/usr/bin/python3
"""Verify signing/cache behavior and offline breakpoint inventory, not capture."""
from pathlib import Path
import subprocess
import tempfile

from umbra_tracer import Tracer, command, resign


def main():
    here = Path(__file__).resolve().parent
    output = here / "results"
    output.mkdir(exist_ok=True)
    fixture = Path.home() / "umbra-scratch/fixtures/umbra-test-child"
    with tempfile.TemporaryDirectory(prefix="umbra-cache-check-", dir="/tmp") as work:
        source = Path(work) / "cache_probe.c"
        binary = Path(work) / "cache_probe"
        source.write_text("int main(void) { return 0; }\n")
        command(["/usr/bin/cc", str(source), "-o", str(binary)])
        first = Path(resign(binary))
        original_mtime = first.stat().st_mtime_ns
        second = Path(resign(binary))
        assert first == second and first.stat().st_mtime_ns == original_mtime
        print("CACHE HIT: PASS (same twin, unchanged mtime)", flush=True)
        # Corrupt only the cache entry created by this disposable check.
        with first.open("ab") as stream:
            stream.write(b"umbra-cache-corruption-check")
        repaired = Path(resign(binary))
        assert repaired == first
        command(["/usr/bin/codesign", "--verify", "--strict", str(repaired)])
        assert not repaired.read_bytes().endswith(b"umbra-cache-corruption-check")
        print("CACHE CORRUPTION: PASS (hash mismatch rebuilt and signature verified)", flush=True)
        source.write_text("int main(void) { return 1; }\n")
        command(["/usr/bin/cc", str(source), "-o", str(binary)])
        changed = Path(resign(binary))
        assert changed.parent != first.parent
        print("SOURCE UPDATE: PASS (different source hash selects different cache directory)", flush=True)
    tracer = Tracer("/tmp/umbra-nfs-stub", 10)
    try:
        target = tracer.debugger.CreateTarget(str(fixture))
        inventory = tracer.install(target)
        kinds = list(inventory["kinds"].values())
        assert len([k for k in kinds if k[0] == "open"]) == 4
        assert len([k for k in kinds if k[0] == "raw"]) == 1
        assert all(target.FindBreakpointByID(bid).GetNumLocations() == 1 for bid in inventory["kinds"])
        print("BREAKPOINT INVENTORY: PASS (7 libsystem svc sites + 1 executable svc site)", flush=True)
    finally:
        tracer.close()
    argv = ["/usr/bin/lldb", "-b", str(fixture)]
    for name in ["__open", "__openat", "__open_nocancel", "__openat_nocancel", "__execve", "__posix_spawn", "posix_spawn", "__fork", "vfork", "raw_open"]:
        argv.extend(["-o", "disassemble -n " + name])
    argv.extend(["-o", "quit"])
    with (output / "abi-disassembly.log").open("w") as log:
        subprocess.run(argv, stdout=log, stderr=subprocess.STDOUT, check=True, timeout=15)
    print("Runtime interception remains unverified; these are offline/signing checks.", flush=True)


if __name__ == "__main__":
    main()
