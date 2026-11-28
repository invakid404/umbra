#!/usr/bin/env python3
"""Freeze already-built D6 artifacts. This never builds or executes a workload."""
import argparse
import hashlib
import json
import pathlib
import shutil
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--destination", required=True, type=pathlib.Path)
parser.add_argument("--profile", required=True)
parser.add_argument("--artifact", action="append", required=True, help="role=absolute-built-path")
args = parser.parse_args()

def git(*argv):
    return subprocess.check_output(["git", *argv], text=True).strip()

def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

if git("status", "--porcelain"):
    raise SystemExit("freeze requires a clean committed measurement tree")
if not args.destination.is_absolute():
    raise SystemExit("destination must be absolute")
artifacts = {}
for item in args.artifact:
    role, path = item.split("=", 1)
    source = pathlib.Path(path)
    if not role or not all(c.isalnum() or c in "_-" for c in role):
        raise SystemExit("invalid role")
    if role in artifacts or not source.is_absolute() or not source.is_file():
        raise SystemExit("duplicate role or invalid artifact path")
    artifacts[role] = source
for role in ("cli", "platform", "storage", "journal", "namespace", "tree5", "admission_driver"):
    if role not in artifacts:
        raise SystemExit("missing role: " + role)
args.destination.mkdir(parents=False, exist_ok=False)
rows = []
for role, source in sorted(artifacts.items()):
    target = args.destination / role
    shutil.copy2(source, target)
    if digest(source) != digest(target):
        raise SystemExit("copy mismatch: " + role)
    rows.append(dict(role=role, absolute_path=str(target), sha256=digest(target)))
compiler = subprocess.check_output(["rustc", "-vV"], text=True)
target = next(line.removeprefix("host: ") for line in compiler.splitlines() if line.startswith("host: "))
patch = subprocess.check_output(["git", "diff", "b0521d2ef272a75c41da46fa2c3054739adb1199", "HEAD", "--"])
manifest = dict(schema_version=1, source_commit=git("rev-parse", "HEAD"), source_tree=git("rev-parse", "HEAD^{tree}"),
    build_profile=args.profile, rustc_version=compiler, target_triple=target,
    observation_patch_sha256=hashlib.sha256(patch).hexdigest(), artifacts=rows,
    workloads=[dict(name=name, sha256=digest(artifacts["tree5"] if name.endswith("tree5") else
        artifacts["admission_driver"] if name == "admission_only" else pathlib.Path("/usr/bin/true")))
        for name in ("native_true", "registry_true", "native_tree5", "registry_tree5", "admission_only")])
(args.destination / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
print(args.destination / "manifest.json")
