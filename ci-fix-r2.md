# CI + CR fix round 2 — Multithreaded tracee, slice 0 (graph `dg-nsw71bqq`)

Node `fix_from_ci_cr`, visit 2, continuing the `implement` session. Date 2026-09-28.
Change: **`wppwptxswssr`**, bookmark `feat/mt-fork`, parent `master` `e44d0db8`.
PR: https://github.com/invakid404/umbra/pull/134. Input:
`/tmp/graph-dg-nsw71bqq/ci-round2.md`, anchored on head `f7510681`.

Four items: the memoria CI blocker, CR2-3 (serialize fixture launches), CR2-2 (stale
comment), CR2-1 (documentation overclaim), plus the outstanding mechanism question —
answered first, because it is the one with a recurrence risk.

**Constraints held:** no production source, guardrails byte-identical,
`single_thread()` and the `debug_assert!` untouched, slice 1 not started, both waivers
unconsumed.

---

## The mechanism by which `memoria.lock` was reverted

**I caused it, and my round-1 explanation for it was wrong.** Both halves matter,
because the wrong explanation is the part that would recur.

What I wrote in `ci-fix-r1.md` was that the lock had been modified by my round-1
diagnostic `memoria --root . check`, which "evidently updates the lock before failing".
**That was a guess and it is false.** Tested directly this round — snapshot the lock,
run the same failing command, compare:

```
0896b522…   before
memoria check failed with exit status 4      (error [git_unavailable])
0896b522…   after        LOCK UNCHANGED
```

The real sequence, read out of `jj evolog` rather than reconstructed from memory. Each
row is a working-copy commit of change `wppwptxswssr` with its `memoria.lock` hash:

| commit | time | lock | what happened |
|---|---|---|---|
| `a390cc6a` | 20:50 | `0896b522` = master | end of fix round 4 |
| `de3f3efa` | 21:03:09 | `0bb39d5e` | **first divergence** — `publish` acked the macOS README |
| `75ba048c` | 21:03:58 | `e0482fbd` | second ack, root README; this is the head CI was **green** on |
| `96e93e19` | 21:35:09 | `0896b522` = master | **my `jj restore --from @- memoria.lock`** |
| `f7510681` | 21:35:49 | `0896b522` = master | pushed; CI red |

`jj evolog` labels the 21:35:09 operation "restore into commit d5e7028806cd…". So:
`publish` ran two `memoria ack`s, jj snapshotted the resulting lock into the change —
correctly, because the lock is a deliverable — and forty minutes later I reverted it.

**Two contributing causes, both mine, and both fixable rather than regrettable:**

1. **An unverified attribution acted on destructively.** I saw a binary file I had not
   knowingly edited, formed a hypothesis about where it came from, and ran `jj restore`
   without testing the hypothesis. The test takes ten seconds. `jj evolog` would have
   named the introducing operation outright. **The rule that follows: never `jj restore`
   a path on an attribution you have not tested — identify the operation that introduced
   it first.**
2. **My own §0/§8 partition had no bucket for it.** Four review rounds were spent making
   §0 assert "exactly four content files, everything else is this arc's process
   documents, zero production source". `memoria.lock` is none of those three things, so
   the partition classified it as residue by omission — and I then acted on that
   classification. A partition that is exhaustive by construction is safe; one that is
   exhaustive by accident teaches the reader to delete whatever falls outside it. §0 and
   §8 now name `memoria.lock` explicitly as a **deliverable** category, with the reason.

Worth #135 or its own issue as a process item: *in a jj workspace, a generated
deliverable that no document's file partition names is a file the next worker will
delete.* The graph's own history is the evidence.

**One correction I owe in return:** the driver's round-1 statement that CI does not gate
on memoria was wrong, and so my "this would have shipped" framing in `ci-fix-r1.md` was
too strong. `.github/workflows/ci.yml:75-76` runs a step named "Memoria documentation
gate" inside the *Rust workspace* job on both platforms, and it caught the regression on
both. The merge was never at risk from this; my path-list check was redundant, not
load-bearing. `ci-fix-r1.md`'s wording overstated its own importance and this paragraph
is the correction rather than a silent edit.

## Item 1 — memoria re-acked at the current head · **FIXED, CI blocker cleared**

`memoria --root` must be a git worktree root and this jj workspace is not one
(#123/#131), so the acks were derived in a throwaway worktree cut from the anchor at
**this round's head**, not at `publish`'s:

```
git -C ~/Coding/umbra worktree add --detach /tmp/memoria-ack-XXXXXX 92aa0088
```

The head was reachable there without pushing first: the anchor and this jj workspace
share one git object store, so `git cat-file -t 92aa0088` resolves. Both acks were taken
with `review --full --format json`, the `.data.token` from that packet, and a
substantive note; then the resulting `memoria.lock` was copied back into the jj
workspace and the worktree removed.

**Lesson 17 ordering held exactly as predicted** — acking one README revealed the next:

| order | README | revision | result |
|---|---|---|---|
| 1 | `crates/umbra-platform-macos/README.md` | 19 → **20** | `updated` |
| 2 | `README.md` (root), surfaced by the first | 43 → **44** | `no-update` |

The notes describe **this** round's work rather than repeating `publish`'s: the macOS
note records that the invalidating `fixtures.rs` change is test-harness only — the
`FIXTURE_LAUNCH` mutex, the `StrayFixtureChildren` guard, `Second::drop` — so the
documented tracer behaviour and both recorded defects are unchanged, and that the
`mt-spawn` paragraph was updated to name the EFAULT path-decode refusal rather than the
supervisor guard. The root note records that the slice adds no capability, command or
configuration, so no root change is warranted.

**Acceptance, from a fresh throwaway worktree at the new head:**

```
OK: 23 README(s) current, imports rendered, no coverage or structure errors.
errors: 0
warning [navigation_disconnected] — 14 item(s)
```

0 errors and 14 `navigation_disconnected` warnings, which is master's baseline exactly.
`git status` in that worktree showed **`M memoria.lock` and nothing else** — the ack
edited no README, so no content file moved under the acks.

### The ordering this round had to learn: ack **last**

The first ack pass here was clean at the tree it was taken on, and the gate went red
again at the pushed head. Cause, read off the failure rather than guessed: **root
`README.md` takes the repository's root-level `.md` files as inputs.** `memoria.toml`
ignores `experiments/**`, `target/**`, `**/.dSYM/**`, `.jj/**` and `Cargo.lock` — and
nothing else, so `impl.md`, `fix-rN.md` and `ci-fix-rN.md` are all inputs. Editing this
document *after* acking therefore re-invalidated the root README with
`identity: ci-fix-r2.md`.

So the acks were redone with every document edit already final, and the only change
after them is `memoria.lock` itself, which is not an input to any README. **The rule for
the next round, and for the next arc: the memoria ack is the last action before the
amend, after the prose is finished.** Lesson 17 says acking one README reveals the next;
this adds that acking before the documents are final reveals the *same* one again.

## Item 2 — CR2-3, fixture launches serialized · **FIXED**

The finding is right and it is the important one: a snapshot-diff reaper can kill a
*legitimately running* child of a concurrent test, and `cargo test` runs a harness's
tests on several threads by default. Trading a leaked process for a non-deterministic
failure of an unrelated test is a bad trade.

A `static FIXTURE_LAUNCH: Mutex<()>` now covers the whole region from a fixture's
before-snapshot through its launch to its stray cleanup. Three points of design:

- **The deadlock warning is correct and is handled by splitting, not by trying to be
  clever with reentrancy.** `fixture_argv` takes the lock and delegates to
  `fixture_argv_locked`, which holds the old body. `mt_fixture` takes the lock itself
  and calls `fixture_argv_locked`, so the lock is never taken twice on one thread.
  `overlay_fixture` takes it too, because it launches a tracee and would otherwise
  create a child inside a concurrent `mt_fixture`'s window.
- **Drop order is the mechanism, so the declarations are ordered deliberately.** In
  `mt_fixture` the lock guard is declared *first* and therefore dropped *last*: strays
  are reaped, then destinations cleared, then the lock released. Declaring it later
  would release it before cleanup and reopen exactly the race being closed.
- **Poisoning is absorbed** — `lock().unwrap_or_else(|p| p.into_inner())`. `mt_write`
  and `mt_spawn` panic by design on every run, so a poisoned mutex is the *normal*
  state after them; propagating it would turn the other eleven cases into secondary
  failures with a misleading cause.

**No interprocess lock was added, and that is a deliberate answer rather than an
omission.** The hazard is threads inside one test binary. `cargo` runs each test
target's executable in sequence rather than concurrently, and this repository's CI
additionally passes `--test-threads=1`. Nothing here supports two fixture test
processes at once — a second would contend for the debugger connection and the resigned
twin cache independently of this guard — so an interprocess lock would add a failure
mode without removing one. The comment on `FIXTURE_LAUNCH` says so, and says that this
is the place to change if that ever stops being true.

**Verified in the configuration that would have flaked**, i.e. default parallelism with
no `--test-threads=1`:

| run | result |
|---|---|
| whole suite, default threads, ×3 | `11 passed; 0 failed; 2 ignored` each time; 11 `CAPTURED`, 0 `SKIP`, 0 `MISSED`, **0 `REAPED`** |
| both `#[ignore]`d cases together, default threads, ×2 | `2 failed` as designed; **exactly 1 `REAPED`** per run — `mt_spawn`'s real stray, never `mt_write` — and 0 strays remaining |

The second row is the one that matters: before this change, `mt_write` running
concurrently with `mt_spawn` could have had `mt_spawn`'s child attributed to it, or
vice versa. Exactly one reap per run, always the true one, and the first panic's
poisoning did not disturb the second test.

## Item 3 — CR2-2, the stale `mt_write` comment · **FIXED**

The comment still said the escaped host file and its `TMPDIR` directory survive on
failure. `Second::drop` removes both; what survives is the assertion text carrying
`holding Some("two\\n")`. Rewritten to say that, and to say that an earlier revision
described the opposite — this is lesson 28's **sixth** instance, a fix pass leaving a
stale claim in a comment adjacent to what it changed, so the correction records itself
rather than quietly replacing the text.

**Swept every comment this round and last round touched.** One more needed the same
treatment, not filed by anyone: `mt_spawn`'s doc comment described
`StrayFixtureChildren` as snapshotting and killing the difference, with no mention of
what makes the difference attributable — the same gap CR2-1 filed against `ci-fix-r1.md`,
one file over. It now names `FIXTURE_LAUNCH`. The remaining touched comments
(`Second`'s fields, `Second::drop`, the `harness_directory` rationale) were re-read and
were accurate as written at that round. *Round 3 note: `harness_directory` has since
been removed entirely — see `ci-fix-r3.md` — so that third rationale no longer exists.*

## Item 4 — CR2-1, the snapshot-diff overclaim · **FIXED**

`ci-fix-r1.md` claimed concurrent children "were already in the before set and are left
alone". They are not: the diff excludes processes that existed *at* the before-snapshot,
and a matching child created afterwards by a concurrent test appears only in the after
set, indistinguishable from the stray. The claim is removed and replaced with the real
limitation plus what now supplies the missing property — the `FIXTURE_LAUNCH` lock — and
the suggested CR-1 reply text in that document had the same phrase, now also corrected.

Fixed after item 2, as instructed, so the document describes what was built rather than
what was hoped. The same correction is in the code, on `fixture_named_processes`: *"The
exclusivity is the correctness argument; the diff is only the mechanism."*

---

## Suggested replies to the CR threads

Posted by the CI/CR node after verification.

**To CR2-3 (`fixtures.rs:456-480`):**

> Fixed, and you were right that this was worse than the leak it replaced. A
> `static FIXTURE_LAUNCH: Mutex<()>` now spans snapshot → launch → stray cleanup.
> Your deadlock warning was the part that shaped the design: `fixture_argv` takes the
> lock and delegates to a new `fixture_argv_locked` holding the old body, and
> `mt_fixture` takes the lock itself then calls the unlocked form, so the lock is never
> acquired twice on one thread; `overlay_fixture` takes it too, since it also launches a
> tracee. The lock guard is declared before the cleanup guards so it is dropped after
> them — releasing it before cleanup would reopen the race. Poisoning is absorbed via
> `unwrap_or_else(|p| p.into_inner())`, because `mt_write` and `mt_spawn` panic by design
> and would otherwise poison the mutex for the other eleven cases. No interprocess lock:
> the hazard is threads within one binary, `cargo` runs test targets sequentially, and
> nothing here supports concurrent fixture test processes — stated in the comment so the
> assumption is visible if it changes. Verified with default parallelism (no
> `--test-threads=1`): the full suite passes 3/3 with zero `REAPED`, and running both
> panicking cases together gives exactly one `REAPED` per run — always `mt_spawn`'s real
> stray, never `mt_write` — with zero strays remaining.

**To CR2-2 (`fixtures.rs:603-607`):**

> Fixed. The comment now says the escaped file and its directory are both removed by
> `Second::drop` and that the evidence travels in the assertion message as
> `holding Some("two\\n")`, with a note that an earlier revision described the opposite.
> I also swept the other comments touched in that pass and found one more instance
> nobody filed: `mt_spawn`'s doc comment described the stray reaper without mentioning
> what makes a reap attributable, which is the same gap as CR2-1 one file over. It now
> names the `FIXTURE_LAUNCH` lock.

**To CR2-1 (`ci-fix-r1.md:81-82`):**

> Fixed — the claim is removed. You are right that the diff only excludes processes
> present at the before-snapshot, so a concurrently created matching child would have
> been read as a stray; that was the written symptom of CR2-3. The document now states
> the real limitation and names what supplies the missing property, the `FIXTURE_LAUNCH`
> lock added for CR2-3, and the same correction is in the code comment on
> `fixture_named_processes`: the exclusivity is the correctness argument, the diff is
> only the mechanism.

---

## The fixed-point check, run again for this round

Writing this document adds a path, and this round also adds `memoria.lock` — a category
§0 and §8 did not have. **Would a hypothetical `ci-fix-r3.md` falsify anything left in
either section?**

| claim remaining | survives? |
|---|---|
| "Exactly four content files are touched, and they are these" + the four paths | **Yes** — a round document is not a content file |
| §8's four per-file `+/−` figures and their subtotal | **Yes** against added process documents — but **no** against CR rounds that edit content files, which is the gap CI round 4 found. §8 states no line counts at all now; see `ci-fix-r4.md` |
| "no production source file is in the diff" | **Yes** |
| this arc's process documents: "`impl.md`, one `fix-rN.md` per review round, one `ci-fix-rN.md` per CI round" | **Yes** — stated as a rule over rounds, not a list |
| "some of those paths already carry an earlier arc's document, so they appear as modifications; the later ones are new files … read off `--stat`" | **Yes** |
| **new:** "`memoria.lock`, when the documentation gate required a re-ack" | **Yes** — conditional on the gate, not on a count |
| §0's two prior-arc rows, headed "document families it left at root" | **Yes** — round 4's fix already removed the "never writes" predicate that this round would have falsified |
| §0's "**two** earlier arcs" | **Yes** |

**Nothing the check assesses goes stale.** Stated precisely, because the earlier
wording overreached: what this check covers is the claims that vary with the *number of
paths in the diff* — the path totals, the process-document counts and the per-arc
inventory — and none of those is stated as a literal any more. It is **not** a claim
that §0 and §8 contain no other integers. At the time of writing they did: §8 tabulated
the four content files' `+/−` figures and their subtotal, on the reasoning that a further
round document or lock re-ack does not touch a content file.

*CI round 4 note:* that reasoning was sound about process documents and wrong about
review iteration — a CR round editing `fixtures.rs` moves the same figure. §8 now states
no line counts at all; `ci-fix-r4.md` records why removing them beat re-deriving them.

Note the one claim that *would* have broken this round is the one round 4 already fixed:
"documents at root this arc never writes" listed `ci-fix-r1.md`, and a `ci-fix-r2.md`
would have compounded it.

---

## Gates, re-run against the final tree (lesson 24), verdicts read by name (lesson 23)

C fixture recompiled from this tree and the crate's tests rebuilt after the last edit.

| Gate | Result |
|---|---|
| `memoria --root . check` (throwaway worktree at the **pushed** head) | **0 errors**, `23 README(s) current`, 14 `navigation_disconnected` warnings = master's baseline |
| `cargo fmt --all -- --check` | exit 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0, **0** lines matching `^(warning\|error)` |
| `cargo test --workspace --all-targets` | exit 0; **52 suites, 832 passed, 0 failed, 5 ignored** |
| `--test fixtures` (integration env, `--test-threads=1`) | **11 passed, 0 failed, 2 ignored**; **0** `SKIP`, **0** `MISSED`, **0** `REAPED` |
| `--test fixtures` **default parallelism**, ×3 | `11 passed` each; 0 `REAPED`, 0 `MISSED` |
| both `#[ignore]`d cases, **default parallelism**, ×2 | exactly 1 `REAPED` each, 0 strays remaining |
| `--test provider_ipc` | 1 passed — `CAPTURED open-libc provider IPC` |
| `--test sandbox_launch` | 4 passed |
| `-p umbra-cli --test run_fixtures` | 10 passed, **20** distinct `PASS` verdicts, 2 declared `SKIP nfs_*_matrix` |
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

**Both `#[ignore]`d cases re-measured**, same two verdicts as every round — the
serialization did not move the defects. `mt_write`'s assertion is now at
`fixtures.rs:612`, measured rather than assumed. **No new measurement of the defects was
taken**; every enforced-run figure in `impl.md` is rounds 0–2's, restated.

Change **`wppwptxswssr`** on `feat/mt-fork`, parent `master` `e44d0db8`.
