#!/usr/bin/env python3
"""Validate the original D6 baseline and prepare a frozen CI head; never run it."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

BASE_COMMIT = "c85722bf4bb6f6818f42172e65b4bfce2cae2b17"
BASE_PATCH = "dec3fad489421167d41350af20afd3174ea6f97dfbdc24613e18a0b168d7c52e"
BASE_HASHES = {
    "admission_driver": "9a6144e5fded359a854f7aa0e512b7b6f80a7540d466be149464a8880ffcad19",
    "cli": "18b9d28a116c3379589f13cdab579ef13927fff2628710e6af0f3e3502730c5c",
    "namespace": "18b9d28a116c3379589f13cdab579ef13927fff2628710e6af0f3e3502730c5c",
    "journal": "8f209ae1747d17de28a88e7e30a6d1933596b48cd63115c64b5b5ebc53792cc9",
    "platform": "fd354cae1a3c7f7b37bb5ec6eba2dc3e287aa5c691e4e3aefcef308dd58a729e",
    "registry_storage": "323848c96ae9a7dbbdd28c8bcb717b466708df5927091f47e06a79e61a79e9a4",
    "storage": "e058462c7ac9b091665bf9ca0b1ba4232a4470f76eeadbd6117cfbd1b9250ed6",
    "tree5": "06518cc5273e152e4b20392c77f8d3f0a27c524222cd9cbad17f21b67f2848c1",
}
ROLES = ("cli", "platform", "storage", "journal", "namespace", "tree5", "admission_driver")
WORKLOADS = ("native_true", "registry_true", "registry_tree5", "admission_only")


def require(condition, message):
    if not condition:
        raise SystemExit(message)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def absolute_file(value, label):
    require(bool(value), f"missing {label}: provision the original frozen baseline on the runner")
    path = Path(value)
    require(path.is_absolute() and path.is_file(), f"{label} must name an existing absolute file")
    return path.resolve()


def baseline(workload):
    routed = workload.startswith("registry_")
    key = "UMBRA_D6_BASE_REGISTRY_MANIFEST" if routed else "UMBRA_D6_BASE_MANIFEST"
    path = absolute_file(os.environ.get(key), key)
    manifest = json.loads(path.read_text())
    require(manifest["schema_version"] == 1, "baseline schema changed")
    require(manifest["source_commit"] == BASE_COMMIT, "original baseline commit required")
    require(manifest["source_tree"] == "8d90414784dd8539855f824b3668c4f0748ffa37",
            "original baseline source tree required")
    require(manifest["observation_patch_sha256"] == BASE_PATCH, "baseline observation patch changed")
    require(manifest["build_profile"] == "debug", "baseline must use debug profile")
    require(manifest["target_triple"] == "aarch64-apple-darwin", "baseline target mismatch")
    compiler = subprocess.check_output(["rustc", "-vV"], text=True)
    require(manifest["rustc_version"] == compiler, "baseline/head compiler mismatch")
    artifacts = {}
    for row in manifest["artifacts"]:
        artifact = absolute_file(row["absolute_path"], row["role"])
        require("target" not in artifact.parts, "baseline artifact is in a mutable build directory")
        require(row["role"] not in artifacts, "duplicate baseline artifact role")
        role = "registry_storage" if routed and row["role"] == "storage" else row["role"]
        require(row["sha256"] == BASE_HASHES.get(role), f"original baseline hash changed: {role}")
        require(digest(artifact) == row["sha256"], f"baseline hash mismatch: {row['role']}")
        artifacts[row["role"]] = artifact
    require(all(role in artifacts for role in ROLES), "baseline artifact role missing")
    workloads = {row["name"]: row["sha256"] for row in manifest["workloads"]}
    require(workloads["native_true"] == digest(Path("/usr/bin/true")), "true workload changed")
    require(workloads["registry_tree5"] == digest(artifacts["tree5"]), "tree5 workload changed")
    registry = None
    if routed:
        registry = absolute_file(os.environ.get("UMBRA_D6_BASE_REGISTRY"), "UMBRA_D6_BASE_REGISTRY")
        value = json.loads(registry.read_text())
        require(set(value["providers"]) == {"platform", "storage", "journal"}, "registry roles changed")
        for role, descriptor in value["providers"].items():
            require(bytes(descriptor["executable"]) == os.fsencode(artifacts[role]),
                    f"baseline registry executable mismatch: {role}")
            require("mounted-nfsv4-v1" not in descriptor["capabilities"], "mounted storage forbidden")
        options = json.loads(bytes(value["providers"]["storage"]["options"]))
        require(bytes(options["host"]) == b"127.0.0.1" and options["port"] == 12105,
                "registry must use the CI raw fixture")
    return path, artifacts, registry


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=("preflight", "prepare"))
    parser.add_argument("--workload", required=True, choices=WORKLOADS)
    parser.add_argument("--session", required=True, choices=("own_session", "sid1"))
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--artifacts", type=Path)
    args = parser.parse_args()
    require(args.output.is_absolute(), "output must be absolute")
    base, artifacts, registry = baseline(args.workload)
    if args.phase == "preflight":
        print(f"Validated original baseline: {base}")
        return
    require(args.artifacts is not None, "missing Cargo artifact stream")
    drivers = {
        row["executable"]
        for line in args.artifacts.read_text().splitlines()
        if (row := json.loads(line)).get("reason") == "compiler-artifact"
        and row["target"]["name"] == "cleanup_performance"
        and row.get("executable")
    }
    require(len(drivers) == 1, "expected exactly one cleanup_performance driver")
    args.output.mkdir(parents=True, exist_ok=False)
    (args.output / "baseline-manifest.json").write_bytes(base.read_bytes())
    if registry:
        (args.output / "baseline-registry.json").write_bytes(registry.read_bytes())
    head = args.output / "head"
    binaries = Path("target/debug").resolve()
    selected = {
        "cli": binaries / "umbra",
        "namespace": binaries / "umbra",
        "platform": binaries / "umbra-platform-macos",
        "storage": binaries / ("umbra-storage-nfs-userspace" if registry else "umbra-storage-local"),
        "journal": binaries / "umbra-journal-file",
        "tree5": artifacts["tree5"],
        "admission_driver": Path(next(iter(drivers))).resolve(),
    }
    command = [sys.executable, str(Path(__file__).with_name("freeze.py")),
               "--destination", str(head), "--profile", "debug"]
    for role, path in selected.items():
        command.extend(("--artifact", f"{role}={path}"))
    subprocess.run(command, check=True)
    environment = {
        "UMBRA_F299_PERF_BASE_MANIFEST": str(base),
        "UMBRA_F299_PERF_HEAD_MANIFEST": str(head / "manifest.json"),
        "UMBRA_F299_PERF_WORKLOAD": args.workload,
        "UMBRA_F299_PERF_SESSION": args.session,
        "UMBRA_F299_PERF_EVIDENCE": str(args.output / "samples"),
        "UMBRA_INTEGRATION_REQUIRED": "1",
    }
    if args.session == "sid1":
        environment["UMBRA_REQUIRE_SID1"] = "1"
    if registry:
        value = json.loads(registry.read_text())
        frozen = json.loads((head / "manifest.json").read_text())
        paths = {row["role"]: row["absolute_path"] for row in frozen["artifacts"]}
        for role, descriptor in value["providers"].items():
            descriptor["executable"] = list(os.fsencode(paths[role]))
        head_registry = args.output / "registry.json"
        head_registry.write_text(json.dumps(value, indent=2) + "\n")
        environment["UMBRA_F299_PERF_REGISTRY"] = str(registry)
        environment["UMBRA_F299_PERF_HEAD_REGISTRY"] = str(head_registry)
    (args.output / "cargo.toml").write_text("[env]\n" + "".join(
        f"{key} = {{ value = {json.dumps(value)}, force = true }}\n"
        for key, value in environment.items()
    ))


if __name__ == "__main__":
    main()
