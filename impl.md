# impl — Memoria 0.4.0 → 0.7.0 upgrade + reusable GitHub Actions adoption

- **Graph:** `dg-t43fjdpv` · **Node:** `implement` · **Date:** 2026-09-29
- **Workspace:** `/Users/inva/Coding/umbra-worktrees/memoria-upgrade` (jj change `zyunyyvo`, bookmark `feat/memoria-0.7`, parent `dd245580` = `master`)
- **Authorities:** `knowledge/umbra/graph-audits/dg-t43fjdpv-{memoria-upgrade,design-gate,prep-workspace}.md`
- **Binary:** `/tmp/graph-dg-t43fjdpv/m070/bin/memoria` → `memoria 0.7.0` (pre-built at tag `v0.7.0`, commit `1eaff8b6`). Invoked by absolute path throughout, because the `PATH` binary is `/opt/homebrew/bin/memoria` → `memoria 0.6.0` and would have shadowed any install.

Every command below was run for real; its output is quoted as produced. Deviations are in the last section.

**Superseded in part by `ci-fix-r1.md`.** CodeRabbit found the runner label `ubuntu-latest` unsafe for this Action, which supports Ubuntu 24.04 only; the CI-fix round regenerated the managed workflow with `--runner ubuntu-24.04`, pinned the same exact label in `memoria-auto-ack.yml`, and pinned the release-archive digest there. The transcripts below are left exactly as they were taken, so they still show `ubuntu-latest` — read them as the record of this round, not as the shipped configuration.

---

## 0. The blocker, confirmed before anything was changed

`prep_workspace` measured that memoria cannot run in a jj workspace. Re-confirmed here on the first call:

```
$ cd /Users/inva/Coding/umbra-worktrees/memoria-upgrade
$ /tmp/graph-dg-t43fjdpv/m070/bin/memoria --root . integrations skill status --target claude
error [git_unavailable]
  git failed for .: not inside a Git worktree: fatal: not a git repository (or
  any of the parent directories): .git
  {}

memoria agent status failed with exit status 4
exit=4
```

So the publish node's *"this arc IS the upgrade, so run in place"* is wrong, exactly as `prep_workspace` recorded. The throwaway-git-worktree pattern was used instead:

1. hand edits in the jj workspace,
2. `jj describe` → the bookmark materialises as a git ref,
3. `git worktree add --detach <tmp> <commit>` from `~/Coding/umbra`,
4. every memoria **write** (skill upgrade, github install, review, ack) run in that worktree,
5. generated files copied back into the jj workspace,
6. worktree re-pointed at the amended commit and the gate re-run there, so the verified tree is the committed tree and not an intermediate one.

Throwaway worktree: `<scratchpad>/gwt`.

---

## 1. `memoria.toml` version 2 → 3

```
$ sed -i '' '1s/^version = 2$/version = 3/' memoria.toml
$ head -1 memoria.toml
version = 3
```

The `[documentation] guidance` array was **deliberately not touched**, and the reason is scope, not cost: ratified step 1 is `memoria.toml:1` and nothing else, so editing the array would have been an unratified change. It still carries the 0.6 phrase *"every nested README also declares its ownership boundary"*, in two places.

An earlier revision of this record claimed editing the array "would change the guidance digest and invalidate all 23 documents, turning a 1-ack upgrade into a 23-ack one." That is measurably false and is corrected here. `review_scope` probed it twice at this change, with restore-and-reverify after each: rewriting the stale vocabulary in place, and appending an entirely new guidance rule. Both left `Reviews 23 current, 0 pending`, `memoria check` exit 0, and `memoria review --format json` with `tasks 0`. Editing the guidance block invalidates nothing; the immediate ack cost is **zero**.

The one real consideration, and it is a follow-up rather than a blocker: a changed digest raises `guidance_changed` on tasks that are *already* pending, and `scripts/memoria-auto-ack.sh:125` skips any task carrying it. So a guidance edit is best paired with a re-ack sweep that refreshes `reviewed_digest`, or the next Renovate bump goes un-acked. That pairing is out of scope here and is filed as follow-up work.

---

## 2. Skill packages → the four-file 0.7 structure

Status before (both targets `outdated`, and note the flag the node text asked for does not exist — `--replace-existing` is not on this command, and there is no `--apply` gate):

```
$ memoria --root . integrations skill status --target claude
planned: status target=claude destination=<gwt>/.claude/skills/memoria
  scope      local
  state      outdated
  version    installed 0.4.0 / embedded 0.7.0
  existing   managed package version 0.4.0 for claude (record schema 2)
  retained   <gwt>/.claude/skills/memoria.install.lock (synchronization_lock, removable_by_uninstall=false)
exit=0
```
(identical for `--target codex` → `<gwt>/.agents/skills/memoria`.)

```
$ memoria --root . integrations skill upgrade --target claude
memoria: agent upgrade: target claude scope local destination <gwt>/.claude/skills/memoria
  writes [SKILL.md, review-details.md, saved-exports.md, integrations.md, .memoria-install.json]
  removals [] replaced [SKILL.md, .memoria-install.json] backup none
exit=0

$ memoria --root . integrations skill upgrade --target codex
memoria: agent upgrade: target codex scope local destination <gwt>/.agents/skills/memoria
  writes [SKILL.md, review-details.md, saved-exports.md, integrations.md, .memoria-install.json]
  removals [] replaced [SKILL.md, .memoria-install.json] backup none
exit=0
```

`backup none` on both is the proof the audit predicted: the installed 0.4.0 files matched the managed package hash exactly, so there were no local customisations to preserve. The two `memoria.install.lock` files were `retained`, not rewritten.

Status after, on the committed tree:

```
  state      current
  version    installed 0.7.0 / embedded 0.7.0
  existing   managed package version 0.7.0 for claude (record schema 2)
  state      current
  version    installed 0.7.0 / embedded 0.7.0
  existing   managed package version 0.7.0 for codex (record schema 2)
```

---

## 3. GitHub Actions — `install`, not `upgrade`

The node text's command was run first, to record the failure rather than assert it:

```
$ memoria --root . integrations github upgrade --version 0.7.0 --action-ref v0.7.0
error [github_not_installed]
  no managed Memoria workflow at .github/workflows/memoria.yml; run `memoria integrations github install` first
  {}

memoria integrations github upgrade failed with exit status 1
exit=1
```

Status beforehand confirmed why — memoria had never written a workflow here, and it warned about the two hand-rolled siblings:

```
hint [github_sibling_workflows]
  2 other workflow files already exist beside this destination. Memoria reads
  none of them and can duplicate an existing documentation check.
  workflows: ci.yml, memoria-auto-ack.yml

no change: integrations github status .github/workflows/memoria.yml
  state      absent
  record     .github/memoria-workflows/memoria.yml.json
  memoria    installed none / desired 0.7.0
```

The ratified command:

```
$ memoria --root . integrations github install --version 0.7.0 \
    --action-ref 1eaff8b6688e8345a5cbb6c8dbac6a2e367fe32c \
    --runner ubuntu-latest --apply
memoria: integrations github install: .github/workflows/memoria.yml
  writes [.github/workflows/memoria.yml, .github/memoria-workflows/memoria.yml.json] removals []
applied: integrations github install .github/workflows/memoria.yml
  state      current
  record     .github/memoria-workflows/memoria.yml.json
  memoria    installed 0.7.0 / desired 0.7.0
  action ref installed 1eaff8b6688e8345a5cbb6c8dbac6a2e367fe32c / desired 1eaff8b6688e8345a5cbb6c8dbac6a2e367fe32c
  runner     installed ubuntu-latest / desired ubuntu-latest
  write      .github/workflows/memoria.yml
  write      .github/memoria-workflows/memoria.yml.json
exit=0
```

The generated file was **not edited by a single byte** — the record's `expected_workflow` field holds the complete text, so any drift forfeits memoria's management. The ratified D3 deviations ship as emitted: `on: [push, pull_request]`, no `concurrency` group, and a `uses:` SHA with no trailing `# v0.7.0` comment.

```yaml
# This workflow is managed by Memoria.
# Change it with `memoria integrations github upgrade --apply`.
# A manual edit makes the file modified, and Memoria then preserves it.
name: Memoria documentation
on: [push, pull_request]
permissions:
  contents: read
jobs:
  memoria:
    runs-on: ubuntu-latest
    timeout-minutes: 10
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - name: Set up Memoria
        id: memoria
        uses: viktordanov/rs-memoria@1eaff8b6688e8345a5cbb6c8dbac6a2e367fe32c
        with:
          version: '0.7.0'
      - run: memoria --version
      - run: memoria check
```

Then the hand-rolled gate was deleted. Lines 71-77 of `ci.yml` (the six ratified lines plus their trailing blank, so no double blank is left behind):

```diff
@@ -68,13 +68,6 @@ jobs:
       - name: Test contract documentation
         run: cargo test --workspace --doc
 
-      - name: Install memoria CLI
-        run: cargo install --locked --git https://github.com/viktordanov/rs-memoria --tag v0.4.0 rs-memoria
-        # Pinned to rs-memoria's v0.4.0 tag; bump on new releases.
-
-      - name: Memoria documentation gate
-        run: memoria --root . check
-
   raw-transport:
```

`grep -ni 'memoria' .github/workflows/ci.yml` now returns nothing. The `rust` matrix job itself is untouched: `macos-14` + `ubuntu-latest`, `cargo fmt --check`, `cargo clippy -D warnings`, workspace tests and doctests all still run on both legs — only the documentation gate moved.

---

## 4. `memoria-auto-ack.yml` — the step that preserves the auto-ack mechanism

This is the mandatory one. Once the lock is format 3 a 0.4.0 binary cannot read it, so leaving line 119 pinned to `v0.4.0` would have broken the Renovate auto-ack on the first bump after merge.

Lines 97-119 — `dtolnay/rust-toolchain`, the whole `Swatinem/rust-cache` block including `shared-key: memoria-cli-v0.4.0` at 110, and the second `cargo install … --tag v0.4.0` at 114-119 — were replaced by the setup Action. The job is already `runs-on: ubuntu-latest`, which is what makes the Linux-only Action legal there.

```diff
-      - name: Install Rust toolchain
-        uses: dtolnay/rust-toolchain@6bed0761d98439e5a578e2877258200ad565ba87 # stable
-
-      # Cache the cargo registry/git/bin so the memoria CLI is not
-      # rebuilt from source on every Renovate PR. …
-      - name: Cache Cargo registry, git deps, and installed binaries
-        uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2
-        with:
-          shared-key: memoria-cli-v0.4.0
-          cache-directories: |
-            ~/.cargo/bin
-
-      - name: Install memoria CLI
-        run: cargo install --locked --git https://github.com/viktordanov/rs-memoria --tag v0.4.0 rs-memoria
+      # The memoria CLI comes from the first-party setup Action, which
+      # downloads a prebuilt binary instead of building from source. …
+      - name: Set up Memoria
+        uses: viktordanov/rs-memoria@1eaff8b6688e8345a5cbb6c8dbac6a2e367fe32c # v0.7.0
+        with:
+          version: '0.7.0'
```

Unlike the memoria-managed workflow, this file is umbra's own, so the SHA carries the house-style `# v0.7.0` comment.

One stale comment inside the same edited mechanism was corrected: line 142 read *"while the toolchain + memoria CLI installed"* and now reads *"while the memoria CLI installed"*, because the toolchain step no longer exists.

`scripts/memoria-auto-ack.sh` was **not changed**, as the audit's F-3 requires. The 0.7 contract keeps `data.tasks[].{document,status,guidance_changed,causes[].code}`, keeps `data.token` (now `mrv3.…`, still 21 bytes so the script's length comment holds), still accepts `ack --packet -` fed the default manifest artifact, and still yields exactly `['input_changed']` for a dependency bump, which is the script's allowlist.

Both workflows parse:

```
$ python3 -c "import yaml; [yaml.safe_load(open(f)) for f in [...]]"
ok .github/workflows/memoria-auto-ack.yml
ok .github/workflows/ci.yml
```

---

## 5. Root `README.md` — the crate and experiment index

A `## Crate and experiment index` section was appended, one line of lead-in plus 22 links. All 22 nested READMEs are listed: the 17 new handoffs and the 5 already linked in prose at lines 17/23/25/26/66, whose prose was left exactly as it was.

```
$ ls crates/*/README.md experiments/*/README.md | wc -l
      22
$ # every link target resolves
link check done      (no MISSING lines)
```

Under 0.7 this is not cosmetic. An unlinked subfolder stays inside root's scope, so without the index a bump to any of the 12 unlinked crates pends the consumer-facing root README *as well as* the crate's own. Measured on this tree after the change, matching the audit exactly:

```
Documents       23: 23 READMEs, 0 opted-in documents; 22 handoffs; 0 sources covered by more than one document
Navigation      0 document(s) not reachable from the root
Coverage        0 selected file(s) that no document covers
```

`handoff_absent` 17 → 0, `overlapping_sources` 63 → 0, `navigation_disconnected` 14 → 0. The predicted cost also materialised: `missing_import_hint` 9 → 26, severity `hint`, which does not fail `check`.

---

## 6. `docs/memoria.md` — the seven false claims

All seven corrected, plus one extra instance of the same stale vocabulary (see Deviations):

| Was | Now |
|---|---|
| 5-7 "The nearest `README.md` above a file owns it; a nested README starts a new ownership boundary." | a document's scope is its folder and everything below it; linking or importing a tracked document in a subfolder hands that subfolder off |
| 13 "any code change in a README's **ownership boundary**" | "in a README's **scope**" |
| 40-41 "The existing `rust` job … on both `macos-14` and `ubuntu-latest`" | the dedicated managed workflow, `setup-memoria`, `ubuntu-latest` only, with the reason the macOS leg was dropped |
| 47 "The job pins the CLI to a release tag." | the Action is pinned to a commit SHA and asked for an exact version; memoria owns the file via `.github/memoria-workflows/memoria.yml.json`; the auto-ack workflow uses the same ref |
| 51-52 "Existing `review` and `explain` JSON contracts and exit codes remain compatible across both releases" | the contracts are versioned and did change — 0.6.0 envelope `schema_version: 3`, `mrv3.` token, manifest by default with `--full` for the complete export; 0.7.0 replaced the ownership model and requires `version = 3`; older artifacts are refused |
| 59 "the packet's canonical **JSON v2** envelope" | "canonical JSON envelope (`schema_version: 3` since 0.6.0)" |
| 85 "under ignored experiment paths, **with no owned files**" | "under ignored experiment paths; their **scopes contain no review inputs**" |

That first pass was verified with a grep whose alternation was narrower than the thing being swept for: `'ownership\|owns it\|owned files\|JSON v2\|release tag\|remain compatible'` cannot match `owned source` or `owned inputs`, and both survived at `:25` and `:101`. Round r1 widened the pattern to `-iE 'own(s|ed|ership)'`, which surfaces them immediately, and corrected both — `:25` to "the source in its scope", `:101` to "review inputs". The counts for the two sweeps are re-measured in `fix-r1.md`, not restated here.

---

## 7. One review, one ack

The audit's central correction held exactly. `memoria status`, taken once every edit was in place and before the first ack:

```
Reviews         22 current, 1 pending, 0 never reviewed, 0 waiting
Documents:
  pending          README.md  [input_changed, document_changed]
  current          crates/umbra-agent-claude/README.md
  … 21 more, all current …
```

`document_changed` is the index; `input_changed` is the six files that moved inside root's scope. The other 22 documents kept their 0.6 records untouched. No batch script, no ordering.

```
$ memoria --root . review README.md --format json > packet-root.json
exit=0
  schema_version: 3
  data.kind: review_manifest      manifest_version: 2    review_revision: 49
  data.token: mrv3.d7ef4246ab3d14a7
  counts: {"handoffs": 22, "imports": 0, "raw_input_bytes": 498807, "scope_files": 46, "suggested_sources": 0}
  review.mode: full_baseline      review.whole_document_pass: true
```

The artifact demanded a whole-document read (`unmapped_change` for `ci.yml`, `memoria-auto-ack.yml` and `docs/memoria.md`; `coverage_unrecorded/revision_not_first` for the two new `.github` files, because the last review predates handed-off-folder recording). The document was read end to end and cross-checked: the CI paragraph at 52-57 lists fmt, clippy, workspace checks, tests and doctests on macOS and Linux, all of which still run on both matrix legs; the Memoria-guide sentence at 101-103 still describes a review flow and a documentation check that runs in CI, both still true with the gate in its own workflow; all 22 index links resolve.

```
$ memoria --root . ack README.md --packet packet-root.json \
    --token mrv3.d7ef4246ab3d14a7 --result updated --reviewer claude-opus-4.7 --note "…"
memoria: ack: README.md revision 49 -> 50 (updated)
hint [historical_coverage]
  Historical coverage partial: 45/47 content inputs match one inspected commit.
  Review validity is independent of Git history.
Recorded README.md revision 50 (updated) by claude-opus-4.7
exit=0
```

`--result updated` rather than `no-update`, because this change does edit the document. The `historical_coverage` hint is expected: the two new `.github` files have no commit history yet.

---

## 8. Verification, at the pre-closing-ack commit `3e41104c`

The worktree was re-pointed at the amended commit (`git checkout --detach --force`, then `git clean -fd`, leaving `git status --short` empty), so what follows was measured against committed bytes rather than a working state.

**Read the `state inspect` figures below as commit `3e41104c` only.** They predate the closing ack that Deviation 1 describes, which rewrites `memoria.lock` — so at the published commit `File bytes`, `Revision`, and README's revision and result all move on by one cycle. What does hold at every commit from here on, including the published one, is the part that matters: `check` exit 0, `OK: 23 document(s) current`, and lock format 3. The published figures are measured and reported in `fix-r1.md`.

```
$ memoria --root . check
OK: 23 document(s) current, imports rendered, no coverage or structure errors.
$ echo $?
0
```

```
$ memoria --root . state inspect
State file      memoria.lock
Format          version 3          ← was 2
Codec           zstd-v1
File bytes      7651
Revision        316
Reviews (23):
  README.md
    revision 50 by claude-opus-4.7 at 2026-09-29T14:51:53Z (updated)
    files 46, imports 0, guidance c3b666d76496f9fd
    coverage evidence: crates/umbra-agent/, … , experiments/tracer/   (22 subtrees)
```

Integrations, same tree:

```
no change: integrations github status .github/workflows/memoria.yml
  state      current
  memoria    installed 0.7.0 / desired 0.7.0
  action ref installed 1eaff8b6688e8345a5cbb6c8dbac6a2e367fe32c / desired 1eaff8b6688e8345a5cbb6c8dbac6a2e367fe32c
  runner     installed ubuntu-latest / desired ubuntu-latest
skill claude: state current, installed 0.7.0 / embedded 0.7.0
skill codex:  state current, installed 0.7.0 / embedded 0.7.0
```

Still open and all severity `hint`, none failing: `missing_import_hint` ×26 and `handoff_not_applied` ×4. Both are explicitly out of scope.

### Final diff

```
$ jj diff --stat
.agents/skills/memoria/.memoria-install.json |  12 +-
.agents/skills/memoria/SKILL.md              | 226 +++++++++--------------------
.agents/skills/memoria/integrations.md       |  22 ++
.agents/skills/memoria/review-details.md     |  69 ++++++++
.agents/skills/memoria/saved-exports.md      |  38 ++++
.claude/skills/memoria/.memoria-install.json |  12 +-
.claude/skills/memoria/SKILL.md              | 226 +++++++++--------------------
.claude/skills/memoria/integrations.md       |  22 ++
.claude/skills/memoria/review-details.md     |  69 ++++++++
.claude/skills/memoria/saved-exports.md      |  38 ++++
.github/memoria-workflows/memoria.yml.json   |  10 +
.github/workflows/ci.yml                     |   7 -
.github/workflows/memoria-auto-ack.yml       |  34 ++--
.github/workflows/memoria.yml                |  22 ++
README.md                                    |  27 +++
docs/memoria.md                              |  47 ++++--
memoria.lock                                 | (binary) +76 bytes
memoria.toml                                 |   2 +-
18 files changed, 516 insertions(+), 367 deletions(-)
```

That stat was taken before this file was written, so it is 18 paths and its `memoria.lock` delta is the first ack's. Adding `impl.md` and the closing ack's lock write makes 19 paths, and the published lock delta is **+21 bytes** (7575 → 7596), not the +76 shown above. Round r1 changes both figures again; `fix-r1.md` carries the current ones.

### Proof of zero Rust

```
$ jj diff --summary | grep -c '\.rs$'
0
$ jj diff --summary | grep -E 'crates/|src/|tests/'
none
```

Zero `.rs` files, therefore zero new `#[test]` functions. No new user setup, mounts, drivers or privileged steps: the two Action steps replace two `cargo install` steps on runners that already existed, and the managed workflow's only permission is `contents: read`.

---

## Deviations from the eight ratified steps

1. **A second ack cycle, forced by this file.** `impl.md` is a selected input of root `README.md` — measured, not assumed:
   ```
   $ printf '\nprobe\n' >> impl.md && memoria --root . status
   Reviews         22 current, 1 pending, 0 never reviewed, 0 waiting
     pending          README.md  [input_changed]
   $ git checkout -- impl.md && memoria --root . status
   Reviews         23 current, 0 pending, 0 never reviewed, 0 waiting
   ```
   Writing the node's own record therefore pends root `README.md` again, and the node cannot both write this file and leave the gate green without re-acking after it. The change consequently ends with a second `memoria review README.md` → `memoria ack --result no-update` run against the final bytes of this file. That ack is the last write to any tracked input, so the published tree is green. Its transcript is reported in the node response rather than here: pasting it into this file would change these bytes and reopen the cycle. Step 7's "one ack" correction still stands for the upgrade itself — the second ack is bookkeeping for the artifact, not upgrade work.
2. **`ci.yml`: seven lines deleted, not six.** The ratified text says lines 71-76. Line 77 is the blank separating the deleted block from `raw-transport:`; deleting 71-76 alone would have left two consecutive blank lines. This matches the audit's own proven end-state worktree byte for byte (`+0 -7`).
3. **One extra line in `docs/memoria.md`.** Line 19 read *"see ownership coverage and pending reviews"* — the same deleted vocabulary as line 13, one paragraph away, and not in the list of seven. Corrected to *"see scope coverage"* rather than leaving the guide self-contradictory.
4. **One extra comment in `memoria-auto-ack.yml`.** Line 142's *"while the toolchain + memoria CLI installed"* described the toolchain step that step 4 deletes. Corrected inside the mechanism being edited.
5. **The `README.md` index carries a one-line lead-in** before the 22 links, so the section is not a bare heading over a naked list. Handoff detection is link-based and unaffected; the measured counters are identical to the audit's.
6. **Reviewer string.** No convention existed to copy (`scripts/memoria-auto-ack.sh:152` uses `github-actions`, which is the bot's identity). Used `claude-opus-4.7`, matching the commit trailer.

Nothing else departed from the eight steps. Not touched, as required: any Rust source, any test, `scripts/memoria-auto-ack.sh`, the `[documentation] guidance` block in `memoria.toml`, the generated workflow's bytes, the five existing prose links in `README.md`, the 4 `handoff_not_applied` hints, the 26 `missing_import_hint` findings, and the historical root artifacts (`publish.md`, `ci-fix-r*.md`, `fix-r*.md`, `review-*.md`).

Not pushed, no PR opened — that is the publish node's work.
