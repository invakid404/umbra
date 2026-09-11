#!/usr/bin/env python3
"""Exit 0 iff a GitHub Actions workflow YAML diff is safe to auto-ack.

Safe means: every difference between BASE and HEAD is a step's
`uses:` value change to another `<owner>/<repo>@<ref>` reference
(i.e. a Renovate-style action-pin bump). Anything else — an added
or removed step, a permission or trigger change, a `run:` block
edit, an env or `with:` change, a workflow rename, or any structural
edit — exits non-zero, and the caller MUST fall back to a human ack.

The check parses both revisions of the YAML with `PyYAML` (available
in the CI runner's default Python) and compares the parsed dicts
deeply, so YAML formatting churn and comment edits cannot sneak past.
(PyYAML discards comments, which is what we want: Renovate's usual
"bump the SHA, update the trailing ``# vX.Y.Z`` tag comment" edit is
fully captured by the `uses:` string change.)

Usage: check-workflow-yaml-diff.py <base-ref> <yml-path>
"""

from __future__ import annotations

import re
import subprocess
import sys
from collections.abc import Iterator
from typing import Any

try:
    import yaml  # PyYAML
except ImportError:  # pragma: no cover — CI runners ship PyYAML by default
    print(
        "check-workflow-yaml-diff.py: PyYAML is required (pip install PyYAML)",
        file=sys.stderr,
    )
    sys.exit(3)


class WorkflowLoader(yaml.SafeLoader):
    """SafeLoader with the YAML 1.1 legacy boolean shorthand stripped.

    PyYAML defaults to YAML 1.1, which treats `on`, `off`, `yes`, `no`,
    `y`, `n` (case-insensitive) as booleans. GitHub Actions workflows
    universally use `on:` as the trigger key; under the default
    resolver, both `on: push` and `true: push` would collapse to
    `{True: 'push'}`, letting a top-level trigger-key swap slip past a
    structural differ. Under this loader `on`/`off`/`yes`/`no` parse
    as strings and `true`/`false` remain proper YAML 1.2 booleans, so
    an `on:` → `true:` rewrite surfaces as an actual key change.
    """


# Copy PyYAML's resolver table then strip the bool resolvers from it,
# and re-register only the YAML 1.2-conforming pattern (True/False in
# any case). This is the recommended pattern documented in PyYAML's
# resolver.py for consumers who need YAML 1.2 semantics.
_BOOL_TAG = "tag:yaml.org,2002:bool"
_YAML_1_2_BOOL_RE = re.compile(r"^(?:true|True|TRUE|false|False|FALSE)$")
WorkflowLoader.yaml_implicit_resolvers = {
    ch: [(tag, regexp) for tag, regexp in resolvers if tag != _BOOL_TAG]
    for ch, resolvers in yaml.SafeLoader.yaml_implicit_resolvers.items()
}
for first_char in "tTfF":
    WorkflowLoader.yaml_implicit_resolvers.setdefault(first_char, []).append(
        (_BOOL_TAG, _YAML_1_2_BOOL_RE)
    )


# `<owner>/<repo>[/<subpath>...]@<40-hex-sha>`. Immutable pins only:
# tags and floating refs are rejected on purpose so a hypothetical
# mutable-ref bump (e.g. `actions/checkout@main` → `@master`) can
# never slip through auto-ack. Group 1 captures the "action locator"
# (everything before `@`) so a locator swap
# (`actions/checkout@<sha>` → `attacker/checkout@<sha>`) can be
# detected by comparing base and head captures. Subpath depth is
# unbounded to accommodate reusable-workflow references of the form
# `<owner>/<repo>/.github/workflows/<file>@<sha>` (five segments)
# and nested composite-action locators like `<owner>/<repo>/a/b/c@<sha>`.
_USES_RE = re.compile(r"^([\w.-]+/[\w.-]+(?:/[\w.-]+)*)@([a-f0-9]{40})$")


def load_yaml_at(ref: str, path: str) -> Any:
    result = subprocess.run(
        ["git", "show", f"{ref}:{path}"],
        check=True,
        capture_output=True,
    )
    return yaml.load(result.stdout.decode("utf-8"), Loader=WorkflowLoader)


def walk_differences(
    base: Any, head: Any, path: tuple[Any, ...] = ()
) -> Iterator[tuple[tuple[Any, ...], Any, Any]]:
    """Yield (path, base_val, head_val) for every leaf where the two trees differ.

    Uses the pair's structural shape to descend. Any type mismatch, list
    length mismatch, or key-set mismatch is reported as-is (whole subtree).
    """
    if type(base) is not type(head):
        yield path, base, head
        return
    if isinstance(base, dict):
        base_keys = set(base.keys())
        head_keys = set(head.keys())
        if base_keys != head_keys:
            # Report a synthetic path so the caller can see which key set changed.
            yield path + ("<keys>",), sorted(map(str, base_keys)), sorted(
                map(str, head_keys)
            )
            return
        for k in base_keys:
            yield from walk_differences(base[k], head[k], path + (k,))
        return
    if isinstance(base, list):
        if len(base) != len(head):
            yield path + ("<len>",), len(base), len(head)
            return
        for i, (b, h) in enumerate(zip(base, head)):
            yield from walk_differences(b, h, path + (i,))
        return
    if base != head:
        yield path, base, head


def is_recognized_uses_path(path: tuple[Any, ...]) -> bool:
    """A `uses:` diff at this path is meaningful only in two shapes:

      * `('jobs', <job_id:str>, 'steps', <index:int>, 'uses')`
        — a step-level action reference.
      * `('jobs', <job_id:str>, 'uses')`
        — a job-level reusable-workflow reference.

    Anything else that happens to end in a key named `'uses'` (e.g. a
    hand-crafted YAML with a `uses` key inside an `env:` map) is not a
    workflow-recognized location and must NOT be auto-acked.
    """
    if len(path) == 3 and path[0] == "jobs" and isinstance(path[1], str) and path[2] == "uses":
        return True
    if (
        len(path) == 5
        and path[0] == "jobs"
        and isinstance(path[1], str)
        and path[2] == "steps"
        and isinstance(path[3], int)
        and path[4] == "uses"
    ):
        return True
    return False


def is_uses_pin_bump(
    path: tuple[Any, ...], base_val: Any, head_val: Any
) -> bool:
    """Return True iff the diff at `path` is a benign action-pin SHA change.

    Requires ALL of:
      * `path` is a recognized `uses:` location (see
        `is_recognized_uses_path` — job-level or step-level only).
      * Both values match `<owner>/<repo>[/<subpath>]@<40-hex-sha>`,
        i.e. immutable pins. Tags and floating refs are rejected on
        purpose so a mutable-ref bump (e.g. `@main` → `@master`) can
        never slip through.
      * Base and head refer to the SAME action locator (owner/repo
        path segments). A locator swap
        (`actions/checkout@<sha>` → `attacker/checkout@<sha>`) is a
        MEANING change, not a pin bump; it must go through a human ack.
    """
    if not is_recognized_uses_path(path):
        return False
    if not isinstance(base_val, str) or not isinstance(head_val, str):
        return False
    b = _USES_RE.match(base_val)
    h = _USES_RE.match(head_val)
    if not b or not h:
        return False
    # Group 1 is the action locator (`<owner>/<repo>[/<subpath>]`);
    # group 2 is the 40-hex-SHA ref. Only the SHA is allowed to change.
    return b.group(1) == h.group(1)


def format_path(path: tuple[Any, ...]) -> str:
    return " -> ".join(repr(p) for p in path) if path else "<root>"


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(
            "usage: check-workflow-yaml-diff.py <base-ref> <yml-path>",
            file=sys.stderr,
        )
        return 2
    base_ref = argv[1]
    path = argv[2]

    try:
        base = load_yaml_at(base_ref, path)
        head = load_yaml_at("HEAD", path)
    except subprocess.CalledProcessError as e:
        stderr = e.stderr.decode(errors="replace") if e.stderr else ""
        print(
            f"error: cannot read {path} at {base_ref} or HEAD: {stderr}",
            file=sys.stderr,
        )
        return 3
    except yaml.YAMLError as e:
        print(f"error: {path} is not valid YAML: {e}", file=sys.stderr)
        return 3

    for path_tuple, b_val, h_val in walk_differences(base, head):
        if not is_uses_pin_bump(path_tuple, b_val, h_val):
            summary_base = repr(b_val)
            summary_head = repr(h_val)
            # Truncate huge subtrees for readability
            for label, s in ("BASE", summary_base), ("HEAD", summary_head):
                if len(s) > 240:
                    # Keep the summary short; the caller only needs to know
                    # which key changed, not the full payload.
                    pass
            print(
                f"UNSAFE: non-uses change in {path} at {format_path(path_tuple)}:",
                file=sys.stderr,
            )
            print(f"  before: {summary_base[:240]}", file=sys.stderr)
            print(f"  after:  {summary_head[:240]}", file=sys.stderr)
            return 1

    print(f"SAFE: {path} diff is pure action-pin (uses:) bumps")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
