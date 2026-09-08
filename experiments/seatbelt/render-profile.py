#!/usr/bin/env python3
"""Render umbra.sb for one run root, mirroring the supervisor's own renderer.

The template holds exactly one token, which stands for a complete quoted Seatbelt
string literal. This script performs that one substitution and nothing else, so
the experiments run against the same policy shape the supervisor installs. It
never invents a default root: a missing or unusable root is an error.

    ./render-profile.py /absolute/run/root /tmp/umbra-rendered.sb
"""
import sys
from pathlib import Path

TOKEN = "{{UMBRA_RUN_ROOT}}"
TEMPLATE = Path(__file__).resolve().parent / "umbra.sb"


def quote(root: str) -> str:
    """Produce a complete Seatbelt string literal, including its quotes."""
    if not root.startswith("/"):
        raise SystemExit(f"write root must be absolute: {root}")
    if root == "/":
        raise SystemExit("write root must not be the filesystem root")
    if root.endswith("/"):
        raise SystemExit(f"write root must not have a trailing separator: {root}")
    if any(ord(c) < 0x20 or 0x7F <= ord(c) <= 0x9F for c in root):
        raise SystemExit("write root contains control characters")
    escaped = root.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        raise SystemExit(f"usage: {argv[0]} <absolute-run-root> <output-profile>")
    source = TEMPLATE.read_text()
    if source.count(TOKEN) != 1:
        raise SystemExit(f"{TEMPLATE} must contain exactly one {TOKEN}")
    remaining = source.replace(TOKEN, "")
    if "{{" in remaining or "}}" in remaining:
        raise SystemExit("template contains an unknown unresolved token")
    rendered = source.replace(TOKEN, quote(argv[1]))
    Path(argv[2]).write_text(rendered)
    print(f"rendered {TEMPLATE} for {argv[1]} into {argv[2]}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
