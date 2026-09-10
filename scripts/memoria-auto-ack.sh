#!/usr/bin/env bash
#
# Auto-ack the root README's memoria review when a Renovate branch's
# only changes are provably-safe dependency bumps (Cargo.lock and/or
# Cargo.toml with version literals in [dependencies] tables).
#
# Contract: on entry, HEAD is the Renovate branch tip. On success, this
# script has either (a) done nothing (change wasn't safe or nothing was
# pending) or (b) staged an updated memoria.lock and committed it with a
# `github-actions[bot]` author. The caller (workflow) handles the push.
#
# Exits non-zero only on internal errors. "Not safe to auto-ack" is an
# expected outcome that leaves the branch untouched.
set -euo pipefail

BASE_REF="${BASE_REF:-origin/master}"
# When the workflow invokes this script from master's checkout with the
# PR head worktree as cwd (the `pull_request_target` topology), the
# workflow sets SCRIPTS_ROOT to master's `scripts/` so this script never
# resolves its helper via cwd — which would be untrusted PR-head content.
# For local dev / manual use, fall back to `scripts/` relative to cwd
# (the auto-ack worktree in that context is fully trusted).
SCRIPTS_ROOT="${SCRIPTS_ROOT:-scripts}"

# ---- 1. What actually changed vs base? -------------------------------

# actions/checkout already fetched full history (fetch-depth: 0). Only
# refresh the base ref itself; do NOT pass --depth here (a shallow-depth
# fetch on a full clone creates .git/shallow and can break merge-base
# for `git diff BASE...HEAD` when the base is older than the depth).
# Strip the `origin/` prefix from BASE_REF so `git fetch` receives a
# remote-side ref name.
git fetch origin "${BASE_REF#origin/}"
changed=$(git diff --name-only "$BASE_REF"...HEAD)
if [ -z "$changed" ]; then
    echo "auto-ack: no changes vs $BASE_REF, nothing to do"
    exit 0
fi

# ---- 2. Restrict changed paths to the allowlist ----------------------

# Anything outside Cargo.toml / Cargo.lock (root or per-crate) means a
# structural change we refuse to auto-ack. Note this list is
# intentionally NARROWER than the workflow's commit-allowlist (which
# only permits memoria.lock outbound); on the inbound side we want to
# reject any file we don't understand. Per-crate manifests are included
# because members like crates/umbra-platform-macos and crates/umbra-storage-nfs
# pin dependencies (e.g. libc) directly rather than through
# [workspace.dependencies], so a Renovate bump to those deps changes the
# member manifest.
non_safe=$(printf '%s\n' "$changed" \
    | grep -vE '^(Cargo\.lock|Cargo\.toml|crates/[^/]+/Cargo\.toml)$' || true)
if [ -n "$non_safe" ]; then
    printf 'auto-ack: SKIP — Renovate touched paths outside the safe allowlist:\n%s\n' "$non_safe"
    exit 0
fi

# ---- 3. For every Cargo.toml touched, verify diff is version-only ---

changed_manifests=$(printf '%s\n' "$changed" | grep -E '(^|/)Cargo\.toml$' || true)
if [ -n "$changed_manifests" ]; then
    while IFS= read -r manifest; do
        if ! python3 "$SCRIPTS_ROOT/check-cargo-toml-diff.py" "$BASE_REF" "$manifest"; then
            echo "auto-ack: SKIP — $manifest diff isn't pure version-literal bumps; human ack required"
            exit 0
        fi
    done <<<"$changed_manifests"
fi

# ---- 4. Ack every pending README ------------------------------------

# memoria's structured output lists exactly which READMEs are pending
# and why. We only ack ones whose sole cause is `input_changed` — if
# a README shows any other cause (guidance_changed, policy_changed,
# imports_changed with structural effects, missing_evidence, etc.) we
# bail so a human can adjudicate. This is defense-in-depth: even inside
# the safe-diff branch above, we still refuse to auto-ack a README that
# claims something we don't recognize.
review_json=$(memoria --root . review --format json 2>/dev/null || true)
if [ -z "$review_json" ]; then
    echo "auto-ack: memoria review returned no JSON; nothing to ack"
    exit 0
fi

pending_paths=$(printf '%s' "$review_json" \
    | python3 -c '
import json, sys
top = json.load(sys.stdin)
# The review command emits {command, data, diagnostics, ok, schema_version}.
# `data.tasks[]` lists every README under review, each with `document`,
# `status`, `guidance_changed`, and a `causes` list whose entries carry
# a `code`. Auto-ack ONLY when every gate agrees:
#   - status == "pending" (there is actually something to ack)
#   - guidance_changed is false (the guidance itself did not shift; if it
#     did, an authorized review is required per SKILL.md §Rules)
#   - every cause code is `input_changed` (no other freshness signal)
tasks = top.get("data", {}).get("tasks", [])
for t in tasks:
    if t.get("status") != "pending":
        continue
    if t.get("guidance_changed"):
        continue
    causes = t.get("causes") or []
    codes = {c.get("code") for c in causes if isinstance(c, dict)}
    if codes and codes.issubset({"input_changed"}):
        print(t.get("document"))
')

if [ -z "$pending_paths" ]; then
    echo "auto-ack: no READMEs are pending purely from input_changed; nothing to do"
    exit 0
fi

commit_subject=$(git log -1 --format=%s HEAD)
note="auto-ack: safe dependency version bumps (${commit_subject})"

# memoria ack needs a fresh packet snapshot + its 21-byte token per README.
# `memoria review <path> --format json` emits data.token and the packet
# body; feed the whole JSON to ack via --packet=- to record the ack.
while IFS= read -r readme; do
    echo "auto-ack: memoria ack $readme"
    packet=$(memoria --root . review "$readme" --format json)
    token=$(printf '%s' "$packet" | python3 -c 'import json, sys; print(json.load(sys.stdin)["data"]["token"])')
    printf '%s' "$packet" | memoria --root . ack \
        --packet - \
        --token "$token" \
        --result no-update \
        --reviewer github-actions \
        --note "$note" \
        "$readme"
done <<<"$pending_paths"

# ---- 5. Confirm the ack made memoria happy --------------------------

memoria --root . check

# ---- 6. Stage the resulting memoria.lock (only) ---------------------

# The caller (workflow) still runs a stricter allowlist check on the
# entire tree before committing, but stage here so `git diff --cached`
# tells the workflow whether there is anything to commit.
if ! git diff --quiet -- memoria.lock; then
    git add memoria.lock
fi
echo "auto-ack: done"
