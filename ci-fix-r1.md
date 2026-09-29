# CI + CR fix round 1 — Multithreaded tracee, slice 0 (graph `dg-nsw71bqq`)

Node `fix_from_ci_cr`, visit 1, continuing the `implement` session. Date 2026-09-28.
Change: **`wppwptxswssr`**, bookmark `feat/mt-fork`, parent `master` `e44d0db8`.
PR: https://github.com/invakid404/umbra/pull/134. Input:
`/tmp/graph-dg-nsw71bqq/ci-round1.md`, anchored on head `75ba048c`.

**CI was green and stays out of scope.** Verified by log-read, not badge: 11 distinct
`CAPTURED` names over 12 occurrences, 0 `MISSED`, 20 `PASS`, only the two declared
`nfs_*_matrix` skips, 6 `PASS userspace` on the live NFSv4 job, and every figure
matching `fix-r4.md`. Both new tests appear correctly `#[ignore]`d **with their verdicts
in the ignore reasons**, so the defects are legible from CI output. Nothing to fix there.

**Three CodeRabbit findings, all valid, all fixed. None rebutted** — I looked at each on
its own terms and agree with all three; CR-2 in particular is the kind of finding a
measurement PR should be grateful for.

**Constraints held:** no production source (both code fixes are test-harness and
fixture files), guardrails byte-identical, `single_thread()` and the `debug_assert!`
untouched, slice 1 not started, both waivers unconsumed.

---

## CR-2 — `umbra-test-child.c`, the silent-failure path inside the instrument · **FIXED**

Taken first because it is the most important. `mt_rendezvous` returned `void`: on
reaching its spin bound it fell through and the caller performed the measured operation
anyway. A rendezvous timeout therefore produced a run in which both destinations were
written **without the threads ever overlapping** — indistinguishable, from the outside,
from a clean measurement. A silent-failure path in the instrument, in a PR whose entire
subject is silent-failure paths.

`mt_rendezvous` now returns `int`, and the change is three parts rather than one:

- **It reports.** `0` only when `MT_PARTICIPANTS` participants actually arrived;
  otherwise it reports through `error_line` with `ETIMEDOUT`, so the timeout is loud on
  stderr.
- **The failure is sticky and shared** — a new `mt_rendezvous_failed` atomic. Without
  it, one participant could give up while the other arrived late and proceeded alone,
  which is the same defect one thread over. Whoever gives up first sets it; everyone
  else sees it and gives up too.
- **Every caller skips its measured call.** `mt_write_thread` returns before
  `write_case`, leaving `mt_job::result` at its non-zero initial value; `mt_spawn` skips
  the `posix_spawn` entirely and sets a non-zero result. A timed-out rendezvous now
  exits non-zero, which `fixture_argv`'s `ExitStatus::Code(0)` assertion turns into a
  test failure.

**No blocking wait was added to the tracee**, as CodeRabbit explicitly warned against —
the dispatch path under measurement is untouched, and the rendezvous still issues no
syscall of its own between the barrier and the measured call.

**Verified by forcing the path** rather than by reading the code. A scratch copy with
`MT_PARTICIPANTS 3` (unreachable) and a shortened bound:

```
umbra-test-child: rendezvous bound reached before both threads arrived: errno=60 (Operation timed out)
umbra-test-child: rendezvous abandoned by another thread: errno=60 (Operation timed out)
forced-timeout exit=1
```

and the destination directory was **empty** — neither file written. So a skipped
measurement now reads as a failure, which was the ask. The normal path is unaffected:
`smoke.sh` 12 of 12 `PASS`, both `mt` arms included.

## CR-1 — `fixtures.rs`, run-scoped child ownership with panic-safe teardown · **FIXED**

`mt_spawn` leaked one suspended, unattached child on every run. Nothing in the backend
owns it — the tripwire fires before `finish_return` attaches it, so there is no session,
no watchdog entry, and the harness never receives its pid — and it survives the panic
holding the inherited descriptors 0/1/2. The panic also unwound past `fixture_argv`'s
own cleanup, so files were left behind too. Corroborated by this arc's own history:
reviewers reaped such children by hand after every round.

Two guards, both dropped on the unwinding path, which is the only path that matters
here:

- **`StrayFixtureChildren`** — run-scoped ownership by observation, since ownership by
  handle is not available. `proc_listallpids` + `proc_pidpath` snapshots the processes
  whose executable file name matches the fixture's *before* the traced run; its `Drop`
  snapshots again and `SIGKILL`s the difference, printing a `REAPED` line naming the pid
  and path.

  **Correction, from CI round 2's CR2-1: the snapshot diff does not by itself make
  the kill attributable, and this paragraph originally claimed it did.** The diff
  excludes processes that existed at the before-snapshot; a matching child started
  *afterwards* by a concurrent test appears only in the "after" set and is
  indistinguishable from the stray. What supplies the attribution is the
  `FIXTURE_LAUNCH` lock added in CI round 2 — see `ci-fix-r2.md`. `SIGKILL` rather
  than `SIGTERM` because the child is suspended before its first instruction and will
  never run a handler. Matching is on file name because the tracer launches a resigned
  twin from a cache path the harness does not know.
- **`Second::drop`** — the second destination's directory and shadow file, plus
  `fixture_argv`'s own host directory **when it is empty**. `remove_dir` refuses a
  non-empty directory, so the emptiness check *is* the call: pure residue goes, a
  directory holding an escaped host file stays as evidence. The escaped file itself is
  removed and its bytes now travel in the assertion message instead —
  `holding Some("two\\n")` — because a leaked file in `TMPDIR` accumulates while the
  failure output is what anybody actually reads.

**Verified with no external `pkill` in the loop**, so the guard was the only reaper:

```
thread 'mt_spawn' panicked at crates/umbra-platform-macos/src/native.rs:571:9:
REAPED mt-spawn: stray fixture child pid 4534 (…/twins/a2c278ac…/umbra-test-child)
strays remaining: 0        TMPDIR residue: 0        shadow residue: 0
```

Both cases run clean now: zero stray processes, zero `TMPDIR` residue, zero shadow
residue, and both verdicts unchanged.

### The distinction the fix preserves, deliberately

**The underlying leak is master's, and nothing here closes it.** It is a property of the
error window — any error between the spawn's `svc` and `finish_return` reaches it,
measured 10/10 under enforced `umbra run` **including the 5 release runs** where the
assertion is compiled out and the run fails on an unrelated `Io during path` refusal —
and it is filed on that basis. CR's ask is narrower and also right: the **test harness**
should not leak regardless of the master defect.

So the fix is scoped to the harness, and both documents say so in as many words.
`fixtures.rs`'s `mt_spawn` doc comment now reads *"That fixes the harness and not the
defect… Reaping here does not close that, and a `REAPED` line is evidence of it rather
than of its absence."* `impl.md` §2.4 keeps the 10/10 measurement verbatim and adds that
the enforced runs do not go through this harness at all. **The issue's description is
unweakened** and the harness fix must not be read as resolving it.

## CR-3 — `fix-r4.md:46`, broken table cell · **FIXED**

The cell contained a code span with a bare pipe, which GFM reads as a column break.
Replaced with "three process-document rows without figures", as suggested.

Swept the other four round documents for the same defect while there: six further
matches, all `` `^(warning\|error)` `` where the pipe is **backslash-escaped** and
therefore renders correctly. Left alone; CR-3's was the only genuine break.

---

## Suggested replies to the CR threads

Posted by the CI/CR node once the fixes are verified, so they describe what was done.

**To CR-1 (`fixtures.rs`):**

> Fixed, and scoped to the harness as you framed it. The test now runs under two
> `Drop` guards so teardown survives the panic that bypassed it: `StrayFixtureChildren`
> snapshots the fixture's processes via `proc_listallpids`/`proc_pidpath` before the
> traced run and `SIGKILL`s whatever appeared once it unwinds, printing a `REAPED` line
> with the pid; `Second::drop` clears the second destination, its shadow, and
> `fixture_argv`'s host directory when empty, with the escaped file's bytes moved into
> the assertion message. Verified with no external `pkill`: zero strays, zero `TMPDIR`
> residue. No blocking wait was added — the dispatch path under measurement is
> unchanged. One thing worth flagging: the leak itself is master's, a property of the
> error window between the spawn's `svc` and `finish_return`, measured on 10 of 10
> enforced runs including 5 release runs where the assertion is compiled out. That is
> tracked separately; this change stops the *harness* accumulating processes and does
> not close the defect, and the doc comment says so.

**To CR-2 (`umbra-test-child.c`):**

> Fixed, and thank you — this was the most valuable of the three. `mt_rendezvous` now
> returns a result: `0` only when both participants arrived, otherwise it reports via
> `error_line` with `ETIMEDOUT`. `mt_write_thread` returns before `write_case` and
> `mt_spawn` skips the `posix_spawn` entirely, both leaving a non-zero result, so a
> timed-out rendezvous fails the run instead of producing an unsynchronised one that
> reads as clean. Added beyond the suggestion: the failure is sticky and shared via an
> atomic flag, because otherwise one participant could give up while the other arrived
> late and proceeded alone — the same defect one thread over. Verified by forcing the
> path with an unreachable participant count: both threads report the timeout, neither
> destination is written, exit status 1. No blocking wait was introduced, per your
> warning; the measured dispatch path is untouched.

**To CR-3 (`fix-r4.md`):**

> Fixed — the cell now reads "three process-document rows without figures". I also swept
> the other four round documents for the same defect; the six remaining matches are
> `^(warning\|error)` where the pipe is backslash-escaped and renders correctly, so this
> was the only genuine break.

---

## The count check, run again for this round

Writing this document takes over `ci-fix-r1.md`, which `master` carries from
`dg-29vwer0f`. That is the trap four review rounds were spent on, so the check was run
again: **would a hypothetical `ci-fix-r2.md` falsify anything in §0 or §8?**

**It found one survivor, introduced by round 4's own reformulation.** §0's per-arc table
was headed *"documents at root this arc never writes"* and listed `ci-fix-r1.md` among
`dg-29vwer0f`'s — a claim this very document falsifies. The partition was right and the
predicate was not: "never writes" is not size-independent, because each CI round takes
over one more path just as each review round does. The rows are now headed *"document
families it left at root"*, and the paragraph beneath says which of those paths this arc
has taken over is answered by `jj diff --name-only` and by nothing here. That formulation
survives `ci-fix-r2.md`, `fix-r5.md`, and `publish.md` when the publish node writes it.

Everything else holds: "exactly four content files, named" and "zero production source"
are unchanged in kind, and §8's four per-file figures were re-quoted from `jj diff` this
round because two of the four grew — `fixtures.rs` +403 and `umbra-test-child.c` +183,
subtotal **717**, `README.md` +90 −1 and `smoke.sh` +41 unchanged. Those are the only
numbers either section states, and a further round document touches no content file.

---

## One thing found in my own working copy, not in the review

`jj diff --name-only` showed **`memoria.lock`** in the diff — a binary artifact I never
edited. Cause: round 1's diagnostic `memoria --root . check`, which fails here with
`error [git_unavailable]` (this workspace has `.jj` and no `.git`, issues #123/#131),
evidently updates the lock before failing. It has been sitting in the change since then
and would have gone to the PR.

Restored to master's bytes with `jj restore --from @-`; both sides now hash
`0896b522…`. It mattered for two reasons beyond tidiness: it is a **documentation-gate
artifact whose correctness cannot be verified locally**, since `memoria` is exactly the
tool that will not run in this workspace, so shipping a silent update to it would be
shipping an unverifiable change to the gate that checks the documentation; and it is
neither a content file nor a process document, so its presence falsified §0 and §8's
partition — the one thing four review rounds were spent making true. Caught by the
path-list check rather than by any review.

## Gates, re-run against the final tree (lesson 24), verdicts read by name (lesson 23)

The C fixture was recompiled from this tree and the crate's tests rebuilt after the last
edit. Unlike the previous four rounds this one changed compiled files, so the rebuild is
load-bearing rather than ceremonial.

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | exit 0 (one reflow applied first, from the new `unsafe` block) |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0, **0** lines matching `^(warning\|error)` |
| `cargo test --workspace --all-targets` | exit 0; **52 suites, 832 passed, 0 failed, 5 ignored** |
| `--test fixtures` (integration env) | **11 passed, 0 failed, 2 ignored**; **0** `SKIP`, **0** `MISSED`, **0** `REAPED` |
| `--test provider_ipc` | 1 passed — `CAPTURED open-libc provider IPC` |
| `--test sandbox_launch` | 4 passed |
| `-p umbra-cli --test run_fixtures` | 10 passed, **20** distinct `PASS` verdicts, 2 declared `SKIP nfs_fixture_matrix` / `SKIP nfs_utility_matrix` |
| `-p umbra-cli --test resume_cli` | 3 passed |
| `-p umbra-supervisor --test reopen` | 7 passed |
| `smoke.sh` untraced | **12 of 12 PASS**, both `mt` arms included |
| forced rendezvous timeout | reports twice, writes nothing, exit 1 |

**The 832 figure keeps its standing qualification**: no `--nocapture` and no fixture
environment, so every integration case in it takes `fixture_argv`'s skip branch and
reports `ok` with its `SKIP` invisible. The rows beneath it are the qualification.

**The eleven `CAPTURED` verdicts, read by name from this round's own captured output:**
`argv0-check`, `dirfd-rename`, `dup-inherit-write`, `exec-write`, `fork-write`,
`grandchild-write`, `open-libc`, `open-svc`, `posix-spawn-write`, `symlink-cycle`,
`wnohang-wait`. **`REAPED` is 0 in that run**, which is the expected result: the eleven
passing cases leak nothing, and only the two `#[ignore]`d ones exercise the guard.

**Both `#[ignore]`d cases re-measured with `--ignored`** against this build, same two
verdicts as every previous round — the harness changes did not move the defects:

```
thread 'mt_write' panicked at crates/umbra-platform-macos/tests/fixtures.rs:547:5:
MISSED mt-write: the second thread's output reached the host at …, holding Some("two\\n")

thread 'mt_spawn' panicked at crates/umbra-platform-macos/src/native.rs:571:9:
a second intercepted syscall entered while one was still in flight: …
```

The `mt_write` assertion moved to `fixtures.rs:547` as the guards were added, and now
carries the escaped bytes. **No new measurement was taken**; every enforced-run figure
in `impl.md` is rounds 0–2's, restated.

Change **`wppwptxswssr`** on `feat/mt-fork`, parent `master` `e44d0db8`.
