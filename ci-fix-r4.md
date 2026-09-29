# CI + CR fix round 4 — Multithreaded tracee, slice 0 (graph `dg-nsw71bqq`)

Node `fix_from_ci_cr`, visit 4, continuing the `implement` session. Date 2026-09-28.
Change: **`wppwptxswssr`**, bookmark `feat/mt-fork`, parent `master` `e44d0db8`.
PR: https://github.com/invakid404/umbra/pull/134. Input:
`/tmp/graph-dg-nsw71bqq/ci-round4.md`, anchored on head `da49c395`.

One real bug, two documentation corrections, and a structural decision about the
partition. **No code change is owed for the red CI job** — the driver's diagnosis holds
on all four grounds (`umbra-overlay` is not in this diff, the same test passed in both
earlier ubuntu runs, the failure is a `mount unresponsive` timeout rather than an
assertion, and macOS passed the same job in the same run), and re-run `109104162060`
was dispatched. Nothing in this round touches that crate.

**Constraints held:** no production source, guardrails byte-identical, `single_thread()`
and the `debug_assert!` untouched, slice 1 not started, both waivers unconsumed.

---

## CR4-1 — the pid scan was dividing a count by 4 · **FIXED**

Correct, and it is the round's priority: this is a defect in code this PR added, and it
was silently defeating the hardening two rounds were spent on.

**Measured before fixing, because a units claim should not rest on reading a header.**
A five-line C probe against `libproc` on this host:

```
null-call return            : 1094
filled-call return          : 1075
actual positive pids in buf : 1074
if return were BYTES  -> 268 pids
if return were PIDS   -> 1075 pids
ps -Ao pid | wc -l          : 1077
```

So the filled call returns a **pid count** — libproc's wrapper divides the kernel's byte
count by `sizeof(int)` before returning it — and the old
`truncate(written / size_of::<i32>())` divided a second time, keeping 268 of 1075.
About a quarter, exactly as the finding said. Both snapshots were truncated
independently, so they could disagree arbitrarily: the reaper could miss a stray, or
diff two inconsistent views of the process table.

**Fixed as asked**, `truncate(written.min(pids.len()))`, so every returned pid is
retained and the buffer can never be overrun.

### The sibling assumptions, checked as requested

Each argument and result in that scan is counted differently, so all five were checked
rather than just the flagged line:

| site | unit | verdict |
|---|---|---|
| `proc_listallpids(NULL, 0)` return | **pid count** | used as a pid count for the buffer length — correct, and the comment now says "count" rather than "size" |
| `proc_listallpids` `buffersize` argument | **bytes** | `size_of::<i32>() * len` — correct |
| `proc_listallpids(buf, size)` return | **pid count** | **the bug**, fixed |
| `proc_pidpath` `buffersize` argument | **bytes** | `PROC_PIDPATHINFO_MAXSIZE`, a byte constant — correct |
| `proc_pidpath` return | **bytes**, excluding the NUL | `truncate(bytes)` — correct, and the same convention `native.rs`'s own `image_path` uses |

Two of the five are pid counts and three are byte counts, on adjacent lines, which is
why the units are now stated explicitly in the comment along with the measured numbers
that settle them.

### One thing the finding did not ask for, and why I added it anyway

Fixing the arithmetic exposed a worse latent hazard in the same function. A reply that
exactly fills the buffer may have been **cut off by it**, and a truncated snapshot is
dangerous asymmetrically: a process missing from *before* but present in *after* is
diffed as a stray **and killed**. That is the concurrent-kill hazard from round 2,
reached through buffer truncation instead of timing, and the `FIXTURE_LAUNCH` lock does
not protect against it.

So the scan now grows and retries while the reply fills the buffer, and — more
importantly — **signals uncertainty instead of guessing**. It returns
`Option<BTreeMap<..>>`, and `None` means "could not enumerate completely". An empty map
was the wrong failure value: an empty *before* makes every matching process look new.
`StrayFixtureChildren::drop` now refuses to reap unless **both** snapshots are known
complete, printing `REAP SKIPPED <case>: …` so the refusal is visible rather than
silent. Uncertainty kills nothing.

**Verified after the fix**: both `#[ignore]`d cases under default parallelism, twice —
exactly one `REAPED` each, **0** `REAP SKIPPED`, 0 strays remaining. The reaper still
reaps the real stray, and the growth path did not trigger spuriously. Round 3's
preexisting-directory probe was re-run and still holds: the planted sentinel survives.

## CR4-2 and CR4-3 · **FIXED, and superseded by the decision below**

Both were stale-subtotal corrections. Rather than substitute `808` — which CR4-1's own
fix would have invalidated in the same round, since it changed `fixtures.rs` again —
they are corrected by pointing at the withdrawal below, with the round documents marked
as historical records rather than rewritten to claim something new. `ci-fix-r3.md`'s
sentence is scoped to path totals, process-document counts and the per-arc inventory,
consistent with the CR3-2 narrowing.

## The partition boundary — I removed the content figures rather than re-deriving them

**The diagnosis is right and it is the most useful thing in this round.** Fix round 4's
partition assumed the four content files and their figures were stable, so the subtotal
could be asserted while path totals could not. That premise held against the threat it
was built for — *added process documents* — and failed against one nobody modelled:
**review rounds that edit content files.** The figure is size-independent with respect
to process documents and size-*dependent* with respect to review iteration.

Of the two options offered I took **removal**, not deferred derivation. Three reasons,
in order of weight:

1. **"The final commit, when no further round can move it" is not knowable from inside a
   round.** Every round of this arc has believed it was the last. Round 3's S3-1 was
   exactly this: a substitution correct at the tree it was taken on and stale one commit
   later, argued at the time to be safe *because its inputs looked stable*. Deferring the
   derivation preserves the failure mode and only narrows the window in which it fires —
   and this round is the proof, because CR4-1 moved `fixtures.rs` again while CR4-2 was
   being fixed.
2. **It is one generator seen from a third distance, and the remedy is already known.**
   Round 1: writing a grand total moved it inside the same edit. Fix round 4: the round's
   own `fix-rN.md` invalidated the path total. Now: the round's own *code fix* invalidates
   the content subtotal. In every case *the act of performing the round changes the number
   the round states*. Removal is what worked twice; re-deriving is what failed twice.
3. **Nothing is lost, because the counts were never the claim.** What round 1's F3
   protects is a reader who counts the paths and cannot tell which extras are not
   production code. §8 now makes three claims, all properties rather than counts, all
   checkable in one command:

   - exactly four content files, **named**, with their edit character;
   - **no production source in the diff** — anything else the tool lists is a root `.md`
     of this arc or `memoria.lock`;
   - the three code files are **insertion-only**, so no pre-existing test, helper or doc
     comment can have changed — which is strictly stronger than any subtotal, and is what
     a reviewer actually wants to know.

   `jj diff --stat` shows all three: four paths with zero deletions but the README's one,
   and no `crates/**/src/**` path at all.

§8 also now carries the withdrawal history as a table — which figure, invalidated by
what, withdrawn at which round — so the *shape* of the mistake is recorded rather than
just its corrections. I generalised the series in it (`fixtures.rs`'s additions "rose at
four separate rounds") rather than listing the values, because a history row ending in a
current value is itself a figure that goes stale.

**Verified against the intended threat:** `fixtures.rs` is at +545 additions this round,
having been 240, 247 and 494 at earlier ones, and §8 says nothing that changed.

---

## Suggested replies to the CR threads

**To CR4-1 (`fixtures.rs:480`):**

> Fixed, and thank you — this was defeating the two rounds of hardening that preceded
> it. I measured the units rather than trusting the header: on this host the null call
> answers 1094, the filled call answers 1075, the buffer holds 1074 positive pids and
> `ps` sees 1077, so the return is a pid count (libproc divides the kernel's byte count
> by `sizeof(int)`) and the old `/ size_of::<i32>()` kept 268 of them — your "roughly a
> quarter" exactly. Now `truncate(written.min(pids.len()))`. I checked the sibling units
> as you asked: five sites, two pid counts and three byte counts on adjacent lines
> (`buffersize` arguments are bytes, `proc_pidpath`'s return is bytes excluding the NUL,
> matching `native.rs`'s own `image_path`); all four others were correct, and the units
> are now documented at the call with the measured numbers. One addition beyond the
> finding: fixing the arithmetic exposed that a reply exactly filling the buffer may have
> been cut off by it, and a truncated *before* snapshot would diff a live process as a
> stray and kill it — the round 2 hazard via truncation instead of timing. The scan now
> grows and retries, returns `Option` so `None` means "could not enumerate completely",
> and the guard refuses to reap unless both snapshots are complete, printing
> `REAP SKIPPED`. Verified: one `REAPED` per run, zero `REAP SKIPPED`, zero strays, and
> the preexisting-directory probe still passes.

**To CR4-2 (`ci-fix-r2.md:270`):**

> Fixed, and the fix is not a substitution. 790 → 808 was right, but CR4-1's own change
> to `fixtures.rs` moved it again in this same round, which is the point: the content
> subtotal is stable against added process documents and unstable against review rounds
> that edit code. So §8 no longer states line counts at all — it states the four named
> content files, their edit character, that the three code files are insertion-only, and
> that no production source is in the diff, all checkable with `jj diff --stat`. The
> round documents keep their figures as dated records with a forward pointer rather than
> being rewritten.

**To CR4-3 (`ci-fix-r3.md:257-258`):**

> Fixed — scoped to what the check assesses (path totals, process-document counts, the
> per-arc inventory), consistent with the CR3-2 narrowing, and noting that §8 has since
> dropped its content figures entirely so the "other integers" it referred to no longer
> exist.

---

## Gates, re-run against the final tree (lesson 24), verdicts read by name (lesson 23)

C fixture recompiled and the crate's tests rebuilt after the last edit.

| Gate | Result |
|---|---|
| `memoria --root . check`, throwaway worktree at the **pushed** head | **0 errors**, `23 README(s) current`, 14 `navigation_disconnected` warnings = master's baseline |
| `cargo fmt --all -- --check` | exit 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0, **0** lines matching `^(warning\|error)` |
| `cargo test --workspace --all-targets` | exit 0; **52 suites, 832 passed, 0 failed, 5 ignored** |
| `--test fixtures` (integration env, `--test-threads=1`) | **11 passed, 0 failed, 2 ignored**; **0** `SKIP`, **0** `MISSED`, **0** `REAPED`, **0** `REAP SKIPPED` |
| both `#[ignore]`d cases, **default parallelism**, ×2 | exactly 1 `REAPED` each, 0 `REAP SKIPPED`, 0 strays remaining |
| preexisting-directory probe | sentinel survives, verdict unchanged |
| libproc units probe | pid-count semantics confirmed against `ps` |
| `--test provider_ipc` | 1 passed — `CAPTURED open-libc provider IPC` |
| `--test sandbox_launch` | 4 passed |
| `-p umbra-cli --test run_fixtures` | 10 passed, **20** distinct `PASS`, 2 declared `SKIP nfs_*_matrix` |
| `-p umbra-cli --test resume_cli` | 3 passed |
| `-p umbra-supervisor --test reopen` | 7 passed |
| `smoke.sh` untraced | **12 of 12 PASS** |

**The 832 figure keeps its standing qualification**: no `--nocapture` and no fixture
environment, so every integration case in it takes `fixture_argv`'s skip branch and
reports `ok` with its `SKIP` invisible.

**The eleven `CAPTURED` verdicts, read by name from this round's own output:**
`argv0-check`, `dirfd-rename`, `dup-inherit-write`, `exec-write`, `fork-write`,
`grandchild-write`, `open-libc`, `open-svc`, `posix-spawn-write`, `symlink-cycle`,
`wnohang-wait`.

**Both `#[ignore]`d cases re-measured**, same two verdicts as every round — the pid-scan
fix did not move the defects. **No new measurement of the defects was taken**; every
enforced-run figure in `impl.md` is rounds 0–2's, restated.

## The fixed-point check, run again

Writing this document adds a path, and CR4-1 changed a content file. Neither falsifies
anything in §0 or §8, because §8 now states **no line counts at all** and §0 states no
path total: what remains is the four content files named, their edit character, "no
production source", the per-round document rule, and `memoria.lock` as a conditional
deliverable. A hypothetical `ci-fix-r5.md` and a hypothetical further code fix both leave
all of it true — which is the first time in this arc that has been true of *both* threats
at once.

Change **`wppwptxswssr`** on `feat/mt-fork`, parent `master` `e44d0db8`.
