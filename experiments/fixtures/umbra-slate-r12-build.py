#!/usr/bin/env python3
"""Build rank-12 fixtures from this checkout's already-built libraries."""
import json
from pathlib import Path
import shutil
import subprocess

root = Path(__file__).resolve().parents[2]
target = root / "target" / "debug"
out = target / "slate-r12"
out.mkdir(exist_ok=True)
def build(argv, **kwargs):
    subprocess.run(["df", "-h", "/System/Volumes/Data"], check=True)
    if shutil.disk_usage("/System/Volumes/Data").free < 5 * 1024**3:
        raise SystemExit("less than 5 GiB free; build refused")
    return subprocess.run(argv, check=True, cwd=root, **kwargs)
build(["clang", "-arch", "arm64", "-O1", "-Wall", "-Wextra", "-Werror",
       "experiments/fixtures/umbra-slate-r12-publication.c", "-o", str(out / "publication")])
args = ["rustc", "--edition=2021", "experiments/fixtures/umbra-slate-r12-provider.rs",
        "-L", "dependency=" + str(target / "deps"), "-o", str(out / "provider")]
for crate in ["umbra_core", "umbra_storage", "umbra_storage_nfs_userspace"]:
    lib = target / ("lib" + crate + ".rlib")
    if not lib.exists():
        matches = list((target / "deps").glob("lib" + crate + "-*.rlib"))
        if len(matches) != 1:
            raise SystemExit("ambiguous library artifacts: " + repr(matches))
        lib = matches[0]
    args += ["--extern", crate + "=" + str(lib)]
artifacts = build(["cargo", "build", "-p", "umbra-storage-nfs-userspace",
                   "--features", "transport-raw", "--message-format=json"],
                  stdout=subprocess.PIPE, text=True)
libs = set()
for line in artifacts.stdout.splitlines():
    message = json.loads(line)
    if message.get("reason") == "compiler-artifact" and message["target"]["name"] == "serde_json":
        libs.update(path for path in message["filenames"] if path.endswith(".rlib"))
if len(libs) != 1:
    raise SystemExit("expected one current serde_json artifact: " + repr(libs))
args += ["--extern", "serde_json=" + next(iter(libs))]
# Native static libnfs is built by this crate's build script.
for directory in (target / "build").glob("umbra-storage-nfs-userspace-*/out"):
    for lib in directory.rglob("libnfs.a"):
        args += ["-L", "native=" + str(lib.parent)]
build(args)
