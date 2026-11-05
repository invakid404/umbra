#!/usr/bin/env python3
"""Build rank-12 fixtures from this checkout's already-built libraries."""
import os
from pathlib import Path
import shutil
import subprocess

root = Path(__file__).resolve().parents[2]
target = root / "target" / "debug"
out = target / "slate-r12"
out.mkdir(exist_ok=True)
def build(argv):
    subprocess.run(["df", "-h", "/System/Volumes/Data"], check=True)
    if shutil.disk_usage("/System/Volumes/Data").free < 5 * 1024**3:
        raise SystemExit("less than 5 GiB free; build refused")
    subprocess.run(argv, check=True, cwd=root)
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
libs = list((target / "deps").glob("libserde_json-*.rlib"))
if len(libs) != 1:
    raise SystemExit("ambiguous serde_json artifacts: " + repr(libs))
args += ["--extern", "serde_json=" + str(libs[0])]
# Native static libnfs is built by this crate's build script.
for directory in (target / "build").glob("umbra-storage-nfs-userspace-*/out"):
    for lib in directory.rglob("libnfs.a"):
        args += ["-L", "native=" + str(lib.parent)]
build(args)
