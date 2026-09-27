# publish — `dg-29vwer0f` / #121

**PR: https://github.com/invakid404/umbra/pull/128**

| | |
|---|---|
| Number | **128** |
| State | `OPEN`, `MERGEABLE` |
| Base → head | `master` ← `feat/ls-userspace` |
| Head SHA | **`e6618397a94e465a8f37a4bf42ba275d23eb2992`** |
| Parent | `a85a8471` (#120) — **one change above master** |
| Title | `feat(tracer): serve directory reads on a virtual descriptor so ls works over the userspace client (#121)` |

## 1. The #123 preflight fired, on this PR's own publish

The ratified guard ran first, and it refused:

```
publish: /Users/inva/Coding/umbra-worktrees/ls-userspace has no .git (jj workspace?);
         memoria and gh cannot run here
```

That is #123 exactly, in the path it was written for. Both `memoria` and `gh pr
create` were therefore run from a **throwaway git worktree** of the anchor repo
(lesson 5+9) rather than from the jj workspace. The guard is workflow-only, as
ratified: **nothing #123-shaped is in the repo diff** — verified by five
independent sweeps across four review rounds.

## 2. memoria

`memoria check` initially failed `input_changed`: seven READMEs owned files this
slice touched. Each was reviewed against its packet and acked in dependency
order, re-cutting the packet after every ack (lesson 17).

| README | result | what changed |
|---|---|---|
| `README.md` | updated | The userspace-routing paragraph was stale four ways: `ls` joins `touch`/`cat`/`mkdir` (four utilities, not three), six mutation probes not five, and the new probe asserts on **entry names** rather than an exit code. |
| `crates/umbra-platform/README.md` | updated | `SyscallAbi::directory_request` added to the trait block, plus why the seam exists: `getattrlistbulk` is the one call with two caller-supplied blocks, and its contents are judged **after** the descriptor fence. |
| `crates/umbra-platform-macos/README.md` | updated | Four rows added to the installed-stub table (461, 13, 6, 399) and why they are a **set**: routing three of the four leaves `ls` failing on the fourth. |
| `crates/umbra-supervisor/README.md` | updated | The owned-module list omitted the new `src/directory.rs`; added with what it holds and where it is injected. |
| `crates/umbra-overlay/README.md` | updated | The false "directory reads are not routed" refusal (corrected during review) plus the snapshot **eviction on close** the `ReadDir` bullet did not describe. |
| `crates/umbra-storage-nfs-userspace/README.md` | updated | The `ls` rows rewritten during implementation. |
| `crates/umbra-cli/README.md` | **no-update** | Its only diff is one forwarded cargo feature; the README documents the command surface and dependency policy and enumerates no probe features, so nothing is falsified. |

Final state on the pushed commit: **`memoria check` exit 0 — `OK: 23 README(s)
current, imports rendered, no coverage or structure errors`**, worktree clean.

## 3. Verification carried into the PR

All re-run by the driver, not taken from a worker report.

**These figures are at head `e6618397`**, which is the head this document
published and which CI round 1 tested. That head has since been superseded twice
— `6a18b98f` (CI/CR fix round 1) and `52fba1e1` (memoria re-ack) — and **832
still holds at the current head**, because neither pass added or removed a test.
`impl.md` §7.1 is the canonical per-commit table for every figure in this slice.

One condition worth carrying, discovered after this document was written: the
`832` below was measured **without** `UMBRA_TEST_FIXTURE_PATH`, so the eleven
`umbra-platform-macos` fixture cases reported `ok` without executing. The count
is the same either way; the run behind it was not. From `6a18b98f` onward the
env is set. `ci-fix-r1.md` §2 has the measurement.

| gate | result, at `e6618397` |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace --all-targets` | **832 passed**, 0 failed, 3 ignored (master `a85a8471`, measured: **813**) |
| `userspace_run` vs live Ganesha | 22 passed, 0 failed |
| `run_fixtures` rewrite-backed matrix | 10 passed, incl. `/bin/ls -l` and `-t` |
| side-by-side vs `a85a8471` | 0 divergences |
| `attrlist` header shapes, routed | 0 run-enders |
| capacity matrix, routed | 0 run-enders; answers match the kernel |
| unbound-descriptor shapes | all answer `EBADF` |
| `readdir` probe / negative control | passes / fails on the names |

## 4. Attribution

- Commit: `Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>` and
  `Claude-Session: …session_01WVMBHbmtYQj4zTrKzUhddF` — **one each, byte-exact**,
  verified after every amend.
- PR body ends with the Claude Code line and the session link.

## 5. State

Nothing merged. Nothing else pushed. The bookmark points at the head SHA above.
`PAUSE BEFORE MERGING` stands — `merge_gate` is where the human decides.

Next: `wait_and_verify_ci`. Note for it — **read the log, not the status**
(the #120 lesson): the routing job's proofs are what matter, and CI has never
before run `Proof 7` or the two new `/bin/ls` flag rows.
