#!/bin/bash
# Run from an ordinary terminal, outside an already-applied Seatbelt sandbox.
#
# This checks a *rendered* profile, the same artefact the supervisor installs.
# There is no built-in mount path and no fallback: supply the rendered profile
# and the write root it grants. A missing input is an error, never a skip and
# never a prompt.
#
#   ./render-profile.py /absolute/run/root /tmp/umbra-rendered.sb
#   UMBRA_PROFILE=/tmp/umbra-rendered.sb UMBRA_WRITE_ROOT=/absolute/run/root \
#       ./verify-deny.sh
set -eu
export LC_ALL=C
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
mkdir -p "$ROOT/results"
LOG="$ROOT/results/verify-deny.log"
DENIED=/tmp/umbra-should-be-denied
failed=0
report() { printf '%s\n' "$*"; printf '%s\n' "$*" >> "$LOG"; }
: > "$LOG"

if [[ -z "${UMBRA_PROFILE:-}" || -z "${UMBRA_WRITE_ROOT:-}" ]]; then
    report 'FAIL inputs: set UMBRA_PROFILE to a rendered profile and UMBRA_WRITE_ROOT to the root it grants'
    exit 1
fi
PROFILE=$UMBRA_PROFILE
ALLOWED="$UMBRA_WRITE_ROOT/umbra-should-be-allowed"
if [[ ! -f "$PROFILE" ]]; then
    report "FAIL inputs: rendered profile $PROFILE does not exist"
    exit 1
fi
if grep -q '{{' "$PROFILE"; then
    report "FAIL inputs: $PROFILE still contains an unrendered template token"
    exit 1
fi
if [[ "$UMBRA_WRITE_ROOT" != /* || "$UMBRA_WRITE_ROOT" == */ ]]; then
    report "FAIL inputs: UMBRA_WRITE_ROOT must be absolute without a trailing separator"
    exit 1
fi
# The profile under test must be the one that grants exactly this root.
if ! grep -qF "(subpath \"$UMBRA_WRITE_ROOT\")" "$PROFILE"; then
    report "FAIL inputs: $PROFILE does not grant writes under $UMBRA_WRITE_ROOT"
    exit 1
fi

# A failed sandbox launch must never be mistaken for a denied filesystem call.
if output=$(/usr/bin/sandbox-exec -f "$PROFILE" /bin/sh -c \
    'test -r /bin/ls && /bin/echo UMBRA_SANDBOX_READY' 2>&1) && \
    [[ "$output" == UMBRA_SANDBOX_READY ]]; then
    report 'PASS sandbox starts and reads /bin/ls'
else
    report "FAIL sandbox starts and reads /bin/ls: $output"
    report 'FAIL /tmp write denied: sandbox did not start'
    report 'FAIL run-root write allowed: sandbox did not start'
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

# The granted root is required input, so its absence is a failure, not a skip.
if [[ ! -d "$UMBRA_WRITE_ROOT" ]]; then
    report "FAIL run-root write allowed: $UMBRA_WRITE_ROOT is not an existing directory"
    failed=1
elif [[ -e "$ALLOWED" || -L "$ALLOWED" ]]; then
    report "FAIL run-root write allowed: preexisting fixture $ALLOWED; refusing to touch"
    failed=1
elif output=$(/usr/bin/sandbox-exec -f "$PROFILE" /bin/sh -c \
    '/usr/bin/touch "$1" && test -f "$1" && /bin/rm "$1"' sh "$ALLOWED" 2>&1); then
    report "PASS run-root write allowed under $UMBRA_WRITE_ROOT"
else
    report "FAIL run-root write allowed: $output"
    failed=1
fi

# A sibling run root must be denied by the same profile: granting one run's root
# must not grant the store that contains it.
SIBLING=$(dirname -- "$UMBRA_WRITE_ROOT")/umbra-sibling-should-be-denied
if [[ -e "$SIBLING" || -L "$SIBLING" ]]; then
    report "FAIL sibling write denied: preexisting fixture $SIBLING; refusing to touch"
    failed=1
elif ! output=$({ /usr/bin/touch "$SIBLING" && /bin/rm "$SIBLING"; } 2>&1); then
    report "FAIL sibling write control: configuration error: $output"
    failed=1
elif output=$(/usr/bin/sandbox-exec -f "$PROFILE" /bin/sh -c \
    '/usr/bin/touch "$1"' sh "$SIBLING" 2>&1); then
    report 'FAIL sibling write denied: touch succeeded'
    rm -f -- "$SIBLING"
    failed=1
elif [[ ! -e "$SIBLING" && ! -L "$SIBLING" ]]; then
    report "PASS sibling write denied: $output"
else
    report "FAIL sibling write denied: unexpected state: $output"
    failed=1
fi
exit "$failed"
