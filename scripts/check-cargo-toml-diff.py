#!/usr/bin/env python3
"""Exit 0 iff a Cargo.toml diff is safe to auto-ack.

Safe means: every difference between BASE and HEAD is a version literal
inside one of the dependency tables (`[dependencies]`,
`[dev-dependencies]`, `[build-dependencies]`, `[workspace.dependencies]`).
Anything else — a dep added or removed, a `path`/`git`/`branch`/`rev` field
flipped, a feature set changed, or any content outside those tables —
exits non-zero and the caller MUST fall back to a human ack.

The check parses both revisions of Cargo.toml with `tomllib` (Python 3.11+,
in the stdlib) rather than diffing text, so TOML formatting churn cannot
sneak past.

Usage: check-cargo-toml-diff.py <base-ref> [cargo-toml-path]
"""

from __future__ import annotations

import subprocess
import sys
import tomllib
from typing import Any, Iterator

DEP_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")


def load_toml_at(ref: str, path: str) -> dict[str, Any]:
    result = subprocess.run(
        ["git", "show", f"{ref}:{path}"],
        check=True,
        capture_output=True,
    )
    return tomllib.loads(result.stdout.decode("utf-8"))


def dep_tables(data: dict[str, Any]) -> Iterator[tuple[tuple[str, ...], dict[str, Any]]]:
    """Yield ((path,), table) for every dep table present in the TOML."""
    for name in DEP_TABLES:
        v = data.get(name)
        if isinstance(v, dict):
            yield (name,), v
    ws = data.get("workspace")
    if isinstance(ws, dict):
        for name in DEP_TABLES:
            v = ws.get(name)
            if isinstance(v, dict):
                yield ("workspace", name), v


def diff_dep_table(base: dict[str, Any], head: dict[str, Any]) -> str | None:
    """Return None if the two dep tables differ only in version literals; else a reason."""
    if base.keys() != head.keys():
        added = sorted(set(head) - set(base))
        removed = sorted(set(base) - set(head))
        return f"dep set changed (added={added}, removed={removed})"
    for key in base:
        b = base[key]
        h = head[key]
        if type(b) is not type(h):
            return f"dep {key!r}: kind changed ({type(b).__name__} → {type(h).__name__})"
        if isinstance(b, str):
            # Bare-string dep. Any diff is a version bump — accept.
            continue
        if isinstance(b, dict):
            # Table dep. Only the `version` field may differ; every other
            # field (features, default-features, optional, path, git, rev,
            # branch, tag, package, …) must be byte-equal.
            b_rest = {k: v for k, v in b.items() if k != "version"}
            h_rest = {k: v for k, v in h.items() if k != "version"}
            if b_rest != h_rest:
                return f"dep {key!r}: non-version fields changed"
            continue
        return f"dep {key!r}: unexpected value type {type(b).__name__}"
    return None


def strip_dep_tables(data: dict[str, Any]) -> dict[str, Any]:
    """Replace dep tables with a sentinel so equality checks skip them."""
    out: dict[str, Any] = {}
    for k, v in data.items():
        if k in DEP_TABLES:
            out[k] = "<stripped>"
        elif k == "workspace" and isinstance(v, dict):
            out[k] = {
                ik: ("<stripped>" if ik in DEP_TABLES else iv)
                for ik, iv in v.items()
            }
        else:
            out[k] = v
    return out


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print("usage: check-cargo-toml-diff.py <base-ref> [cargo-toml-path]", file=sys.stderr)
        return 2
    base_ref = argv[1]
    path = argv[2] if len(argv) > 2 else "Cargo.toml"

    try:
        base = load_toml_at(base_ref, path)
        head = load_toml_at("HEAD", path)
    except subprocess.CalledProcessError as e:
        print(f"error: failed to read {path} at {base_ref} or HEAD: {e}", file=sys.stderr)
        return 3
    except tomllib.TOMLDecodeError as e:
        print(f"error: {path} is not valid TOML: {e}", file=sys.stderr)
        return 3

    if strip_dep_tables(base) != strip_dep_tables(head):
        print(f"UNSAFE: {path} has non-dependency-table changes", file=sys.stderr)
        return 1

    base_deps = dict(dep_tables(base))
    head_deps = dict(dep_tables(head))
    for path_tuple in sorted(set(base_deps) | set(head_deps)):
        b = base_deps.get(path_tuple, {})
        h = head_deps.get(path_tuple, {})
        reason = diff_dep_table(b, h)
        if reason is not None:
            print(f"UNSAFE: [{'.'.join(path_tuple)}]: {reason}", file=sys.stderr)
            return 1

    print(f"SAFE: {path} diff is pure version-literal bumps in dep tables")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
