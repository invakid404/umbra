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
# Cargo 1.100 places dependencies in per-package build directories. Use the
# current build's artifact stream for both externs and transitive search paths.
artifacts = build(["cargo", "build", "-p", "umbra-storage-nfs-userspace",
                   "--features", "transport-raw", "--message-format=json"],
                  stdout=subprocess.PIPE, text=True)
required = {name: set() for name in
            ["umbra_core", "umbra_storage", "umbra_storage_nfs_userspace", "serde_json"]}
dependency_dirs = set()
native_paths = set()
for line in artifacts.stdout.splitlines():
    message = json.loads(line)
    if message.get("reason") == "compiler-artifact":
        for filename in message["filenames"]:
            path = Path(filename)
            if path.suffix in {".rlib", ".rmeta", ".dylib", ".so"}:
                dependency_dirs.add(str(path.parent))
            name = message["target"]["name"]
            if name in required and path.suffix == ".rlib":
                required[name].add(filename)
    elif message.get("reason") == "build-script-executed":
        native_paths.update(message["linked_paths"])
args = ["rustc", "--edition=2021", "experiments/fixtures/umbra-slate-r12-provider.rs",
        "-o", str(out / "provider")]
for crate, paths in required.items():
    if len(paths) != 1:
        raise SystemExit("expected one current " + crate + " artifact: " + repr(paths))
    args += ["--extern", crate + "=" + next(iter(paths))]
for directory in sorted(dependency_dirs):
    args += ["-L", "dependency=" + directory]
for path in sorted(native_paths):
    args += ["-L", path]
build(args)
