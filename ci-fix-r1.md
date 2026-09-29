# ci-fix r1 — Memoria 0.4.0 → 0.7.0 upgrade

- **Graph:** `dg-t43fjdpv` · **Node:** `fix_from_ci_cr` (round 1) · **Date:** 2026-09-29
- **Input:** `knowledge/umbra/graph-audits/dg-t43fjdpv-ci-round1.md`
- **PR:** https://github.com/invakid404/umbra/pull/140 · **Reviewed head:** `4f02818f9f14ed18143d0b65f017efb68ed188f1`
- **Binary:** `/tmp/graph-dg-t43fjdpv/m070/bin/memoria` → `memoria 0.7.0`, invoked by absolute path. The `PATH` binary is 0.6.0 and was never used.

## CI was green; the review was not

Every check passed, and the logs confirm the upgrade did what it was supposed to: the Action resolved at the pinned SHA, installed the **prebuilt release archive** (`memoria: release asset …/memoria-0.7.0-x86_64-unknown-linux-gnu.tar.gz`), reported `memoria 0.7.0`, and the gate printed `OK: 23 document(s) current` in 6 seconds. Zero cargo builds of memoria anywhere in the run.

The `CodeRabbit: SUCCESS` check reported only that the bot **ran**. Its review *state* was `CHANGES_REQUESTED`, anchored to the exact head SHA, with two Major findings. **Both are valid and both are accepted** — no rebuttals this round. I re-verified each independently rather than taking the review text or the node brief on trust; the verification commands are quoted below.

---

## CR-2 — `ubuntu-latest` → `ubuntu-24.04` · accepted

The more serious of the two, and it is a latent failure rather than a present one.

`setup-memoria` refuses every platform and release but Ubuntu 24.04. `ubuntu-latest` resolves to 24.04 **today**, which is exactly why CI is green — the run log shows `Image: ubuntu-24.04`. The day GitHub moves that alias, **both** memoria jobs fail at setup, before either check runs. CI being green is therefore not evidence against this finding; it is the thing that hides it.

Ownership note: the audit flagged this in section D-2 and the ratified command still carried `--runner ubuntu-latest`, taken from the audit's own probe and never re-examined at the design gate. `ubuntu-24.04` is also memoria's default. CodeRabbit is right.

### The managed half — regenerated, not hand-edited

```
$ memoria --root . integrations github upgrade --version 0.7.0 \
    --action-ref 1eaff8b6688e8345a5cbb6c8dbac6a2e367fe32c --runner ubuntu-24.04
preview: integrations github upgrade .github/workflows/memoria.yml
  state      outdated
  runner     installed ubuntu-latest / desired ubuntu-24.04
  write      .github/workflows/memoria.yml
  write      .github/memoria-workflows/memoria.yml.json
```

Applied with `--apply`:

```
applied: integrations github upgrade .github/workflows/memoria.yml
  state      current
  memoria    installed 0.7.0 / desired 0.7.0
  action ref installed 1eaff8b6688e8345a5cbb6c8dbac6a2e367fe32c / desired 1eaff8b6688e8345a5cbb6c8dbac6a2e367fe32c
  runner     installed ubuntu-24.04 / desired ubuntu-24.04
  write      .github/workflows/memoria.yml
  write      .github/memoria-workflows/memoria.yml.json
```

It rewrites the workflow **and** its ownership record together, which is the whole point — neither file was touched by hand, so management is intact:

```
$ python3 -c "import json; r=json.load(open('.github/memoria-workflows/memoria.yml.json')); \
              print(r['expected_workflow'] == open('.github/workflows/memoria.yml').read())"
True
$ grep -n 'runs-on' .github/workflows/memoria.yml
10:    runs-on: ubuntu-24.04
```

**This is the first time the managed upgrade path has been exercised, and it is incidental proof that ratified Decision 1 (Shape 1) bought something real.** `integrations github upgrade` is the command that refused with `error [github_not_installed]` at the start of this arc, because umbra's gate was hand-rolled and memoria never adopts a workflow it did not write. After the `install`, the same command now works, and a runner-label change that would otherwise have been a hand edit became a one-command regeneration with the ownership record kept in sync. Shape 3 (tag bump only) would have left this a manual edit; Shape 2 (Action inside `ci.yml`) would have too.

### The hand-rolled half

`.github/workflows/memoria-auto-ack.yml` is umbra's own file, so it is edited directly:

```diff
 jobs:
   auto-ack:
-    runs-on: ubuntu-latest
+    # Exact label, not `ubuntu-latest`: the setup-memoria Action below supports
+    # Ubuntu 24.04 only and refuses any other release, so the alias moving to a
+    # newer image would fail this job at setup. Change it in step with the Action.
+    runs-on: ubuntu-24.04
```

The comment above the Action step, which previously justified the label by saying the Action "supports Linux runners only; this job is already `runs-on: ubuntu-latest`", now states the real constraint and points at the pin.

### The documentation half

CR-2 also asked for `docs/memoria.md` to stop describing the Action as merely Linux-only. Accepted:

```diff
-`memoria check` on `ubuntu-latest`. A non-zero exit fails the job. The gate does
-not run on `macos-14`: that Action supports Linux runners only, and
+`memoria check` on `ubuntu-24.04`. A non-zero exit fails the job. The runner
+label is exact on purpose: the Action supports **Ubuntu 24.04 only** and refuses
+every other platform and release, so `ubuntu-latest` would break the job the day
+GitHub moves that alias to a newer image. Pin the label, and change it in step
+with the Action. The gate therefore does not run on `macos-14` — but
 `memoria check` hashes tracked files and reads the lock, so its result does not
 depend on the operating system.
```

---

## CR-1 — pin the release archive digest · accepted, with an asymmetry

The Action always fetches and compares the published checksum sidecar. CodeRabbit's argument is that an attacker able to replace the archive can replace the sidecar alongside it, so the sidecar alone proves only internal consistency. The Action's own `action.yml` says as much — I read it at the pinned SHA rather than relying on the review's paraphrase:

```yaml
  sha256:
    description: >-
      Optional SHA-256 digest of the release archive for the runner's architecture.
      The published checksum sidecar is always required and always compared; this
      input adds an independent expected value. A matrix over architectures must
      pin one digest for each architecture.
```

The premise holds here specifically, because the release is mutable, and I computed the digest myself rather than copying the suggestion:

```
$ gh api repos/viktordanov/rs-memoria/releases/tags/v0.7.0 --jq '.immutable'
false
$ curl -sL .../v0.7.0/memoria-0.7.0-x86_64-unknown-linux-gnu.tar.gz -o m070.tar.gz
$ shasum -a 256 m070.tar.gz
efdbf68399c8e40e4d4e6abcf2761f92c27cd461929d47520c6797b6d6916ae7
```

Matches the release metadata and CR's value. Applied:

```diff
       - name: Set up Memoria
         uses: viktordanov/rs-memoria@1eaff8b6688e8345a5cbb6c8dbac6a2e367fe32c # v0.7.0
         with:
           version: '0.7.0'
+          sha256: 'efdbf68399c8e40e4d4e6abcf2761f92c27cd461929d47520c6797b6d6916ae7'
```

One digest suffices because this runner is x86_64 only; a matrix over architectures would need one per architecture, and the comment now records that. A stale digest fails the job closed, which is the safe direction, so the pin is paired with `version` and must move with it.

### Why the digest is in only one of the two workflows

This is deliberate and worth stating plainly, because it otherwise reads as an oversight.

`.github/workflows/memoria.yml` is generated by `memoria integrations github`, and that command exposes no digest input. Verified directly rather than assumed:

```
$ memoria integrations github upgrade --help
Options:
      --path --root --format --version --action-ref --runner --apply --dry-run
$ memoria integrations github install --help | grep -iE 'sha|digest|checksum'
      --action-ref <REF>   Action code reference: a full 40-character commit SHA or an exact vX.Y.Z tag
```

No `--sha256` on either subcommand. Adding the line by hand would make the file *modified*, memoria would stop managing it, and ratified Decision 3 — plus the CR-2 fix immediately above, which depends on the managed path still working — would be forfeited for one line. CodeRabbit scoped its suggestion to the hand-rolled file, which is the correct call.

The residual exposure is bounded and asymmetric in the right direction. `memoria.yml` is a `contents: read` job with no secrets that runs `memoria --version` and `memoria check`. `memoria-auto-ack.yml` is the repo's highest-privilege job — `pull_request_target` with a write-scoped PAT — and that is the one now carrying the independent pin. If a digest input is added upstream, `integrations github upgrade` is the mechanism to adopt it, and that path is now known to work.

---

## Observed behaviour, recorded not fixed — the gate runs twice

The memoria gate ran **twice** on this PR, from runs `36591088918` and `36591029561`. Cause: the managed workflow's `on: [push, pull_request]`, which fires on both events for a branch in this repo, where `ci.yml` limits push to `master`. Both runs passed; the cost is roughly 6 seconds of duplicate work per push.

This is **ratified Decision 3 showing up in practice**, not a regression. The generated template's trigger, its missing `concurrency` group, and its `uses:` SHA without a `# vX.Y.Z` comment were all accepted as-is precisely because hand-editing any of them forfeits the management that CR-2 has now made load-bearing. Recorded here so `merge_gate` can weigh the real cost against the alternative, which is Shape 2 and no managed upgrade path.

---

## Verification

`memoria` still cannot run in the jj workspace — unchanged, re-confirmed:

```
error [git_unavailable]
  git failed for .: not inside a Git worktree          exit status 4
```

Same throwaway-git-worktree pattern: edits in the jj workspace, `jj describe`/amend, `git worktree add --detach` at the amended commit, memoria run there by absolute path, regenerated files copied back, worktree re-pointed after each amend.

All three workflows parse, and the new input parses as an input rather than as text:

```
ok .github/workflows/memoria-auto-ack.yml
ok .github/workflows/ci.yml
ok .github/workflows/memoria.yml
auto-ack runs-on: ubuntu-24.04
step with: {'version': '0.7.0', 'sha256': 'efdbf68399c8e40e4d4e6abcf2761f92c27cd461929d47520c6797b6d6916ae7'}
```

No `ubuntu-latest` remains in either memoria workflow; the only occurrences of that string are inside the two comments that explain why it is not used. `ci.yml`'s own `rust` and `raw-transport` jobs still use `ubuntu-latest` and are untouched — they build umbra with the pinned toolchain and have nothing to do with the Action.

### Guardrails, re-checked

- `.github/workflows/memoria.yml` and its ownership record — **regenerated by memoria, never hand-edited**; byte-identity against `expected_workflow` verified above and again at the sealed commit.
- `memoria.toml` — unchanged, guidance block included.
- `scripts/memoria-auto-ack.sh` — unchanged.
- Zero Rust, zero new `#[test]`, nothing outside the already-ratified file set.

### A note on the closing figures

`ci-fix-r1.md` is a new root-level scope source, like `impl.md` and `fix-r1.md` before it, so writing it pends root `README.md` and the node closes with one `memoria review README.md` → `memoria ack`. That ack rewrites `memoria.lock` after these bytes are fixed.

So, as in `fix-r1.md`: **this file quotes no lock byte count and no `state inspect` block**, because any such figure would be one cycle stale by the time the tree is sealed. What is asserted, and verifiable at the published commit, is the durable part — `memoria --root . check` exits 0 with `OK: 23 document(s) current, imports rendered, no coverage or structure errors`, and the lock stays format 3. That closing check runs from the git worktree against the final amended commit; its output and the pushed head SHA are reported in the node response.

---

## Deviations

1. **One supersession note added to `impl.md`.** Its round-0 transcripts legitimately show `--runner ubuntu-latest` and `runner installed ubuntu-latest`, because that is what round 0 ran. Per the M2 lesson from review round r1, measured output is not rewritten after the fact — so the transcripts are left exactly as taken and a single note near the top flags that the shipped configuration is now `ubuntu-24.04` and points here. The alternative, editing the quoted output to match today's tree, is the precise defect r1 corrected.
2. **Comments added beyond the one-line changes CR suggested.** The runner pin and the digest are each a line, but a bare `sha256:` hash and a bare exact runner label both look like arbitrary constants to the next maintainer, and both must be updated in step with `version`. The reasons are recorded at the point of use.

Nothing else changed. Not touched: `memoria.toml`, `scripts/memoria-auto-ack.sh`, any Rust source, any test, `README.md`, `ci.yml`, and the skill packages.

## Still out of scope

The two follow-ups from `fix-r1.md` stand, unactioned, for `merge_gate`: `memoria.toml`'s two remaining `ownership boundary` phrases (cost measured at 0 acks), and the `knowledge/` archive gap for `dg-0ved1w0e-impl.md` and `fix-r1.md`…`fix-r5.md`. The duplicate gate run recorded above is new material for the same conversation.
