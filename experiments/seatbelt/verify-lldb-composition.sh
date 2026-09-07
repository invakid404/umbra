#!/bin/bash
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
# The existing Command Line Tools Python is also LLDB's scripting dependency.
# No installation, system configuration, or files in the umbra repo are changed.
export PYTHONDONTWRITEBYTECODE=1
exec /usr/bin/python3 "$ROOT/lldb_composition.py" "$@"
