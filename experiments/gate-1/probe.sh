#!/usr/bin/env bash
# M0 Gate 1 probe: resign a target binary ad-hoc with get-task-allow, then
# heuristically confirm task control by launching under debugserver.
#
# Usage: probe.sh <target-binary>
#
# Exit 0 = task control appears to work (debugserver stayed running past the
#          heuristic timeout, i.e. it bound its listen port and is waiting for
#          a gdb-remote client).
# Exit 1 = resign failed or debugserver exited immediately (denied).

set -euo pipefail

TARGET="${1:?usage: probe.sh <binary>}"
WORK="${UMBRA_M0_WORK:-/tmp/umbra-m0}"
DBGSVR="/Library/Developer/CommandLineTools/Library/PrivateFrameworks/LLDB.framework/Resources/debugserver"
ENT="$(cd "$(dirname "$0")" && pwd)/ent.plist"
PORT="${UMBRA_M0_PORT:-31337}"

mkdir -p "$WORK/twins"

if [[ ! -x "$TARGET" ]]; then
    echo "target not executable: $TARGET" >&2
    exit 1
fi
if [[ ! -x "$DBGSVR" ]]; then
    echo "debugserver missing at $DBGSVR — install Command Line Tools" >&2
    exit 1
fi
if [[ ! -f "$ENT" ]]; then
    echo "entitlements plist missing: $ENT" >&2
    exit 1
fi

NAME="$(basename "$TARGET")"
TWIN="$WORK/twins/$NAME"

echo ">>> original signature:"
codesign -dvv "$TARGET" 2>&1 | grep -E '(Identifier|Format|flags|Authority|Runtime)' | head -5 || true

echo ">>> resigning $TARGET -> $TWIN"
cp -f "$TARGET" "$TWIN"
if ! codesign -f -s - \
        --entitlements "$ENT" \
        --preserve-metadata=identifier,flags,runtime \
        "$TWIN" 2>&1; then
    echo "!!! resign failed" >&2
    exit 1
fi

echo ">>> attach test via debugserver"
"$DBGSVR" "127.0.0.1:$PORT" "$TWIN" >/dev/null 2>&1 &
DPID=$!
sleep 1.2

if kill -0 "$DPID" 2>/dev/null; then
    echo "    RESULT: debugserver still running after 1.2s → task control OK"
    kill "$DPID" 2>/dev/null || true
    wait "$DPID" 2>/dev/null || true
    exit 0
else
    echo "    RESULT: debugserver exited → task control DENIED"
    exit 1
fi
