#!/bin/bash
# Run from an ordinary terminal, outside an already-applied Seatbelt sandbox.
set -eu
export LC_ALL=C
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PROFILE="$ROOT/umbra.sb"
mkdir -p "$ROOT/results"
LOG="$ROOT/results/verify-deny.log"
DENIED=/tmp/umbra-should-be-denied
ALLOWED=/mnt/umbra-nfs/umbra-should-be-allowed
failed=0
report() { printf '%s\n' "$*"; printf '%s\n' "$*" >> "$LOG"; }
: > "$LOG"

# A failed sandbox launch must never be mistaken for a denied filesystem call.
if output=$(/usr/bin/sandbox-exec -f "$PROFILE" /bin/sh -c \
    'test -r /bin/ls && /bin/echo UMBRA_SANDBOX_READY' 2>&1) && \
    [[ "$output" == UMBRA_SANDBOX_READY ]]; then
    report 'PASS sandbox starts and reads /bin/ls'
else
    report "FAIL sandbox starts and reads /bin/ls: $output"
    report 'FAIL /tmp write denied: sandbox did not start'
    report 'FAIL NFS write allowed: sandbox did not start'
    exit 1
fi

# Prove the exact path is writable outside this policy. Never touch a preexisting
# file or symlink; only remove a file created by this invocation.
if [[ -e "$DENIED" || -L "$DENIED" ]]; then
    report "FAIL /tmp write denied: preexisting fixture $DENIED; refusing to touch"
    failed=1
elif (set -o noclobber; : > "$DENIED") 2>> "$LOG"; then
    rm -- "$DENIED"
    if output=$(/usr/bin/sandbox-exec -f "$PROFILE" /bin/sh -c \
        '/bin/echo UMBRA_TOUCH_STARTED; /usr/bin/touch "$1"' sh "$DENIED" 2>&1); then
        report 'FAIL /tmp write denied: touch succeeded'
        rm -f -- "$DENIED"
        failed=1
    elif [[ "$output" == UMBRA_TOUCH_STARTED$'\n'* ]] && \
        [[ "$output" == *'Operation not permitted'* || "$output" == *'Permission denied'* ]] && \
        [[ ! -e "$DENIED" && ! -L "$DENIED" ]]; then
        report "PASS /tmp write denied: ${output#*$'\n'}"
    else
        report "FAIL /tmp write denied: unexpected failure: $output"
        failed=1
    fi
else
    report 'FAIL /tmp write denied: unsandboxed write control failed'
    failed=1
fi

if [[ ! -d /mnt/umbra-nfs ]]; then
    report 'SKIP NFS write allowed: skipped, mount not up (/mnt/umbra-nfs absent)'
elif [[ -e "$ALLOWED" || -L "$ALLOWED" ]]; then
    report "FAIL NFS write allowed: preexisting fixture $ALLOWED; refusing to touch"
    failed=1
elif output=$(/usr/bin/sandbox-exec -f "$PROFILE" /bin/sh -c \
    '/usr/bin/touch "$1" && test -f "$1" && /bin/rm "$1"' sh "$ALLOWED" 2>&1); then
    report 'PASS NFS write allowed: touch, existence check, and cleanup succeeded'
else
    report "FAIL NFS write allowed: $output"
    failed=1
fi
exit "$failed"
