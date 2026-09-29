# CI + CR fix round 5 — Multithreaded tracee, slice 0 (graph `dg-nsw71bqq`)

Node `fix_from_ci_cr`, visit 5, continuing the `implement` session. Date 2026-09-28.
Change: **`wppwptxswssr`**, bookmark `feat/mt-fork`, parent `master` `e44d0db8`.
PR: https://github.com/invakid404/umbra/pull/134. Input:
`/tmp/graph-dg-nsw71bqq/ci-round5.md`, anchored on head `06482364`.

**CI fully green**, and ubuntu passing at 2m41s retroactively confirms the round-4 red
was the `umbra-overlay` flake. Nothing owed there, and nothing in this round touches
compiled code at all.

One finding, documentation only — but it sits next to the PR's central severity claim, so
the substance of this round is the verification, not the edit.

**Constraints held:** no production source, guardrails byte-identical, `single_thread()`
and the `debug_assert!` untouched, slice 1 not started, both waivers unconsumed.

---

## The verification: **F2 survives, and it never depended on the UUID claim**

Checked against source rather than accepted. The driver's reading is confirmed, and the
check turned up one thing beyond it: **my text was not merely over-claiming, it was
factually wrong about one of the two run shapes** — in the direction that *understates*
how strong F2 is.

### What F2's core argument actually needs

The reclassification — availability and correctness, not a namespace escape — needs
exactly one property: **the target of an escaped write is outside the profile's sole
`file-write*` allowance.** An escaped write is by definition *unrewritten*, so its target
is the tracee's own workspace path. So the question is only: can the workspace path lie
inside the allowance?

### There are two allowances, not one, and my text described only one of them

`crates/umbra-supervisor/src/run.rs:1200-1203`:

```rust
let root_path = match &host_state {
    Some(state) => BytePath::new(state.host.as_os_str().as_bytes().to_vec()).ok(),
    None => binding.root.physical_path.clone(),
};
let interpose = host_state.is_some();
```

with `routed = binding.root.physical_path.is_none()` at `:952`. So:

| run shape | the profile's one write allowance | why the escaped write is outside it |
|---|---|---|
| **routed** (`interpose` true) | `<state_root>/<run-id>/host` — an **empty per-run host directory** that exists only so the one write rule names a real path; the run's data never goes there, since every routed operation reaches the store through the userspace client | `HostState::host`'s own documentation (`run.rs:548-562`): it is "strictly less than the grant a kernel-path run receives, and **nothing in the tracee's logical namespace resolves to it**" |
| **kernel-path** (`interpose` false) | canonicalized `binding.root.physical_path` (`run.rs:1218-1234`) — this run's own root inside the store | the workspace is not inside this run's root: the root is **created during preparation**, after the workspace has been inventoried, so a path that already existed cannot lie inside a directory that does not yet exist |

`impl.md` stated the kernel-path allowance unconditionally (`<store>/<run-id>/root`),
which is wrong for a routed run. The error is in the safe direction: the routed
allowance is *narrower*, and its disjointness from the tracee's namespace is asserted in
source, so F2 is **stronger** in the case my text mis-described than in the case it
described.

### The UUID claim was separable, unnecessary, and wrong as phrased

The struck sentence was: *"the run root is a fresh UUID directory created during
preparation, so no argv operand can point at it."* CodeRabbit is right that a UUID does
not provide that property — unguessability is not unreachability, and nothing stops a
tracee naming a path it has been told.

But the property F2 needs was never unguessability. It is **disjointness**, and each run
shape has its own reason for it: *stated in source* for the routed case, and **creation
order** for the kernel-path case. "Fresh UUID" was a loose shorthand for freshness that
reads as an entropy argument; removing it loses nothing, because the freshness that
matters is "created during preparation", which the corrected text now says in those
words.

**So: the correction narrows an over-claim sitting beside F2 and leaves F2 untouched.
The PR's severity framing does not change, and there is nothing here for `merge_gate`
beyond this record.** Stated plainly because the driver asked for either answer and this
is the one the source supports — had it gone the other way it would be at the top of this
document in bold, not in a sentence.

### One scope fact the check surfaced, now in `impl.md`

The 20 enforced runs behind §2.5 are **kernel-path** runs: `umbra-storage-local` exposes
a kernel path for the run root, so `routed` is false and `interpose` is false, which is
also why the shadow paths in those runs sit under `<store>/<run-id>/root/…`. The routed
case is not measured and does not need to be — it is the narrower allowance. §2.5 now
says so rather than leaving a reader to infer that the measurements covered both.

## The edit

`impl.md`'s fact 4 is replaced by the two-case table above with its source citations, the
UUID sentence is struck with a note recording that it was there and why it went, and the
corroborating-runs section is scoped to kernel-path runs. No other section changed; F2's
reclassification paragraph is untouched, which is the point.

---

## Suggested reply to the CR thread

**To the `impl.md:454-459` finding:**

> Fixed, and the check behind it is worth reporting because this text sits beside the
> PR's severity reclassification. You are right on both counts. The two run shapes are
> now distinguished from source (`run.rs:1200-1203`, on `routed =
> binding.root.physical_path.is_none()` at `:952`): a routed run's one allowance is
> `<state_root>/<run-id>/host`, an empty per-run host directory that the run's data never
> touches, and `HostState::host`'s own documentation supplies the property that matters —
> "nothing in the tracee's logical namespace resolves to it"; a kernel-path run's is the
> canonicalized `binding.root.physical_path`. The UUID claim is gone. You are right that
> a UUID gives no unreachability property, and it was never needed: what makes an escaped
> write land outside the allowance is disjointness — asserted in source for the routed
> case, and creation order for the kernel-path case, since the run root is created during
> preparation after the workspace has been inventoried. I verified explicitly that the
> reclassification does not depend on the struck claim, because if it had, that would
> have changed the PR's framing rather than one sentence. It does not. One thing beyond
> the finding: the old text gave the kernel-path allowance unconditionally, so it was
> wrong about routed runs in the direction that understated the guarantee, and the 20
> enforced measurements are now labelled as kernel-path runs.

---

## Gates, re-run against the final tree (lesson 24), verdicts read by name (lesson 23)

C fixture recompiled and the crate's tests rebuilt, though this round changed no compiled
file — only `impl.md` and this document.

| Gate | Result |
|---|---|
| `memoria --root . check`, throwaway worktree at the **pushed** head | **0 errors**, `23 README(s) current`, 14 `navigation_disconnected` warnings = master's baseline |
| `cargo fmt --all -- --check` | exit 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 |
| `cargo test --workspace --all-targets` | exit 0; **52 suites, 832 passed, 0 failed, 5 ignored** |
| `--test fixtures` (integration env, `--test-threads=1`) | **11 passed, 0 failed, 2 ignored**; **0** `SKIP`, **0** `MISSED`, **0** `REAPED`, **0** `REAP SKIPPED` |
| `--test provider_ipc` | 1 passed — `CAPTURED open-libc provider IPC` |
| `--test sandbox_launch` | 4 passed |
| `-p umbra-cli --test run_fixtures` | 10 passed, **20** distinct `PASS`, 2 declared `SKIP nfs_*_matrix` |
| `-p umbra-cli --test resume_cli` | 3 passed |
| `-p umbra-supervisor --test reopen` | 7 passed |
| `smoke.sh` untraced | **12 of 12 PASS** |
| markdown table integrity, all nine round documents | 0 broken — swept after an edit of mine introduced a blank line between a header and its separator, caught and fixed before push |

**The 832 figure keeps its standing qualification**: no `--nocapture` and no fixture
environment, so every integration case in it takes `fixture_argv`'s skip branch and
reports `ok` with its `SKIP` invisible.

**The eleven `CAPTURED` verdicts, read by name from this round's own output:**
`argv0-check`, `dirfd-rename`, `dup-inherit-write`, `exec-write`, `fork-write`,
`grandchild-write`, `open-libc`, `open-svc`, `posix-spawn-write`, `symlink-cycle`,
`wnohang-wait`.

**Both `#[ignore]`d cases re-measured**, same two verdicts as every round. **No new
measurement of the defects was taken**; every enforced-run figure in `impl.md` is rounds
0–2's, restated, now with its run shape named.

## The fixed-point check

Writing this document adds a path. §0 and §8 state no path total, no process-document
count and **no line counts at all** — the remedy adopted in CI round 4 — so nothing goes
stale on arrival. What §8 asserts is the four content files by name with their edit
character, that the three code files are insertion-only, and that no production source is
in the diff; a further round document falsifies none of it, and neither would a further
code fix.

Change **`wppwptxswssr`** on `feat/mt-fork`, parent `master` `e44d0db8`.
