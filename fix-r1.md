# Fix round 1 — Multithreaded tracee, slice 0 (graph `dg-nsw71bqq`)

Node `fix`, visit 1, continuing the `implement` session. Date 2026-09-28.
Change: **`wppwptxswssr`**, bookmark `feat/mt-fork`, parent `master` `e44d0db8`.
Input: `/tmp/graph-dg-nsw71bqq/review-synthesis-r1.md` — twelve findings, five P0.
Sources read end-to-end: `review-correctness.md` (sha256 `f551cbca…`) and
`dg-nsw71bqq-review-scope.md`, both at the anchor's
`knowledge/umbra/graph-audits/`.

**Tree at the start of this round:** commit `6343991cfaa3994c964e7929a974c104ddaf3b20`
— not `3efced79`, the commit both reviews pinned. The correctness reviewer wrote
`review-correctness.md` into the worktree root and jj snapshotted it into the change
under review, twice (`3efced79` → `220e6ff8` → `6343991c`). The change id never
moved. F4 is that snapshot.

**No production source was touched in this round either.** `native.rs`, `abi.rs`,
`rsp.rs`, `umbra_interpose.c`, `journal.rs` and `run_fixtures.rs` remain
byte-identical to master, and slice 1 is still not implemented — both reviews
cleared the slice-0-only stop as *mandated*, so there was nothing to reopen.

---

## Disposition of all twelve findings

| id | P | Disposition |
|---|---|---|
| F1 | P0 | **Fixed** — and the surrounding argument rewritten, not patched |
| F2 | P0 | **Fixed by measurement** — 20 enforced runs; severity restated downward |
| F3 | P0 | **Fixed** — recomputed against the final tree, six paths at `f1b57a67`, self-consistent |
| F4 | P0 | **Fixed by decision** — review documents do not ship; policy stated in `impl.md` §0 |
| F5 | P0 | **Deferred deliberately, with the note F5 offers** — flagged for `merge_gate` and #132 |
| F6 | P1 | **Fixed** — per-arm table restored in all three places |
| F7 | P1 | **Fixed by stating it plainly**, in all three places; the case itself deferred to the item-6 issue |
| F8 | P1 | **Fixed** — third shape added and costed; it removes the `vCont` framing |
| F9 | P2 | **Fixed** — upgraded from assertion to measurement, 10/10 |
| F10 | P2 | **Fixed** — recorded, and narrowed to debug-only by measurement |
| F11 | P2 | **Deferred to `publish`** (blocked here), with item 6 marked a ship-gate |
| F12 | P2 | **Recorded** in `impl.md` §7.9; the title is `publish`'s to set |

---

## F1 — "silent overwrite in release" is false for the shipped path · **FIXED**

**Verified independently before changing anything.** `umbra-supervisor/src/events.rs:307`,
in `syscall_entry`:

```rust
if self.operations.contains_key(&thread) {
    return Err(error(
        ErrorKind::InvalidState,
        "supervisor.syscall_entry",
        "entry received while an operation is still awaiting its exit",
    ));
}
```

A real `Err`, not a `debug_assert!` — live in release. And `operations` is declared
`BTreeMap<ThreadId, OperationId>` at `umbra-supervisor/src/lib.rs:332`, so it is
keyed **by thread**. Both halves of the reviewer's claim hold.

**What was rewritten, and why patching one sentence would not have been honest.**
"Silent in release" was load-bearing: it was the reason `mt-spawn` looked severe, and
it rested on `native.rs:571` being the only tripwire. So `impl.md` §2.3 now says what
is true at each layer — the *backend* does take the overwrite in release, because the
assertion is compiled out; the *run* does not proceed silently, because a second
tripwire ships one layer up — and then draws the consequence the old argument
concealed:

**The supervisor already models in-flight operations per `ThreadId`. Only the
platform backend's slots are single-slot.** (The correctness review's C2.) That
asymmetry localises the defect exactly and is independent evidence that slice 1's
shape is the architecturally consistent fix rather than a guess. Neither the audit
nor the design gate mentions the guard; `impl.md` §2.3 now routes it to the item-6
issue and to the audit.

**One thing stated rather than borrowed.** I verified the guard by source read. I did
**not** reproduce it firing: in my own 20 enforced runs it never did, because the
Seatbelt denial kills the child before a second overlapping entry occurs. The
correctness reviewer measured it firing 5/5 in release. Both are recorded, attributed,
and shown to be compatible — reaching the guard needs the escaped thread to issue a
*second* traced call while the sibling's operation is still open, and that is timing.
`impl.md` says so in those words rather than claiming a measurement I do not have.

## F2 — restate severity downward, and run the decisive experiment · **FIXED BY MEASUREMENT**

Round 0's §2.5 reasoned where it should have run. It offered two branches and named
the severe one without measuring which occurs. That is now replaced by measurement,
and the wording is what the runs support.

**20 enforced `umbra run` executions, 10 of them release.** Registry rebuilt to
mirror `run_fixtures.rs::matrix()` (real `umbra-platform-macos`, `umbra-journal-file`,
`umbra-storage-local`), `--experimental --local-dev`, Seatbelt in force before the
target's first instruction, both destinations inside the workspace so the worst
branch had its best chance:

| configuration | runs | exit | host `output2` | failure |
|---|---|---|---|---|
| debug `mt-write` | 5 | 1 ×5 | never | `open: errno=1 (Operation not permitted)` → child exit 1 → `ProcessFailed during run.child` |
| **release** `mt-write` | 5 | 1 ×5 | never | identical |
| debug `mt-spawn` | 5 | 1 ×5 | never | panic `native.rs:571` → `provider disconnected` → `LeaseLost` (F10) |
| **release** `mt-spawn` | 5 | 1 ×5 | never | `Io during path: null or overflowing pointer` (`errno 14`) |

Signature sweep over all 21 enforced scratch directories: 10 `Operation not
permitted`, 5 `native.rs:571`, 5 `LeaseLost`, 5 `Io during path`, **0 host
`output`/`output2` files in total**.

**The decisive experiment was run, and its answer came from the policy as well as
the runs.** The synthesis asked for the one run that could restore the severe
classification: an escaped path that is host-writable under the rendered profile.
`experiments/seatbelt/umbra.sb` is `(deny default)` + `(allow file-read*)` + exactly
one write rule, `(allow file-write* (subpath {{UMBRA_RUN_ROOT}}))`, and
`crates/umbra-supervisor/src/sandbox.rs:42-47` *fails the render* unless that token
appears exactly once. The
token becomes this run's own root, `<store>/<run-id>/root`
(`crates/umbra-supervisor/src/run.rs:1218-1234`). The
template's own closing line: *"No /tmp, /private/var/folders, or other persistent
write carve-outs."* So there is no host-writable path for an escaped write to land
in, and the run root is a fresh UUID directory created during preparation that a
tracee cannot name from argv. The severe branch is excluded by construction, not by
luck — which is a stronger result than one run, and it is why the runs came back
`EPERM` 10/10.

**Reclassification, now stated once and carried into `impl.md` §2.5, the README, and
§7's issue payload:** the interception escape is real and confirmed; on the shipped
path it is **availability and correctness**, **not** a namespace escape, and must not
be filed under confidentiality or integrity.

**Residual caveat, bounded rather than waved at:** reaching the severe branch needs a
rendered profile granting a host-writable path the tracee also names. That is not a
configuration knob — it takes an edit to `umbra.sb` adding a second `allow
file-write*` line, which the one-token check does not prevent. Nobody ran that, and
nothing in the tree claims it.

**No severity claim now reaches `impl.md`, the README or the PR body unmeasured.**
The README's defect section previously said only that the second destination "is
written on the host"; it now distinguishes the bare-tracer harness from the enforced
path and gives the enforced figure.

**The commit description was amended too, and this goes beyond what F1 asked for.**
The synthesis said "correct `impl.md`", but the same false claim was in the commit
body — *"A release build takes that overwrite silently"* — and the commit body is
nearer to the PR body than `impl.md` is, so F2's "do not let any severity claim reach
the PR body unmeasured" bites there first. Both reviews ruled the description correct
as written, but the scope review listed that sentence among things "verified above"
while the correctness review was simultaneously refuting it; the rulings predate the
finding rather than surviving it. The amended body now carries the supervisor guard,
the per-arm correction, the `WaitPlan::Native` gap, the 20-run enforced figure with
the explicit "NOT a namespace escape", the unverified-prediction marking, and the
Mach sibling-hold. Everything both reviews actually ratified is untouched: the
`test(tracer):` type, the "measure the ungated multithreaded paths (slice 0)" framing,
no gate-removal claim, no thread-safety claim.

## F3 — diff path count · **FIXED**

Recomputed against the **final** tree after every other edit landed, including this
file. `jj diff -r @ --name-only` returns **six** paths:

```
crates/umbra-platform-macos/README.md            documentation
crates/umbra-platform-macos/tests/fixtures.rs    test
experiments/fixtures/smoke.sh                    test harness
experiments/fixtures/umbra-test-child.c          test fixture
impl.md                                          process document
fix-r1.md                                        process document (this file)
```

`impl.md` §0 now states the path count, lists every path with what each is, and
separates the *count* from the *guarantee* it was standing in for — the parent commit
is `master`, so every file not in that list is byte-identical by construction, which
is the claim that actually matters and does not depend on arithmetic. F3's own reason
for existing is quoted there: a reader who counts a different number cannot tell which
extra path is not production code.

*As-of note, added in round 2:* the six paths listed above are this diff at commit
`f1b57a67`, the tree round 1 delivered. Round 2 adds `fix-r2.md`, making it seven —
see `fix-r2.md` and `impl.md` §0/§8 for the current figure. And §8 no longer works the
way this paragraph originally described: rather than carrying per-file line counts for
the process documents, it gives exact counts only for the four content files and leaves
the process documents named-but-uncounted, because a line count of the file stating the
line count cannot converge. The scope reviewer withdrew its own request for those
counts on seeing that.

Self-consistent with F4: because `review-correctness.md` was restored (below), it is
not among the six.

## F4 — `review-correctness.md` in the shipping diff · **FIXED BY DECISION**

**Decision: review documents do not ship. Implementation documents this node
authored do.** `review-correctness.md` has been restored to master's bytes
(`jj restore --from @-`), so it is out of the diff and the root path again holds what
master holds.

**The reason, which is the mechanism rather than a preference.** In jj a root-level
write *is* a mutation of the change under review. That is exactly why the scope
reviewer wrote only to its scratchpad and to the anchor's archive, and why the
previous arc's round 3 records a change being mutated under a live reviewer for ~68
seconds. `review-correctness.md` is in the tree by accident of that mechanism, not by
anyone's decision. Ratifying the accident would also have shipped **one** review and
not the other, which was the one outcome defensible on no grounds at all; and the
symmetric alternative — writing the scope review into the tree myself — would have
overridden a peer node's deliberate placement decision.

**Nothing is lost.** This round's correctness review is already archived at
`/Users/inva/Coding/umbra/knowledge/umbra/graph-audits/dg-nsw71bqq-review-correctness.md`,
and I verified it is byte-identical to the copy removed and to `/tmp`'s: all three
hash `f551cbca9b348d1e4d1ee46075a1af7dbea50059d41f55e9b6225afdd339f3ee`, which is the
hash the synthesis states for its own input. The scope review is archived beside it,
as are this graph's audit, design gate and synthesis.

The policy and its reason are recorded in `impl.md` §0 so the next arc inherits the
decision rather than the accident.

## F5 — a stale previous-arc `review-scope.md` at this commit · **DEFERRED, WITH THE NOTE**

Confirmed, and **wider than the finding states**: the root carries the previous arc's
`review-scope.md`, `review-correctness.md`, `review-synthesis-r1..r4.md`,
`fix-r1..r3.md`, `ci-round1..2.md`, `ci-fix-r1..r2.md` and `publish.md` — all
`dg-egt6apy1`'s, all inherited from master unchanged, none in this diff. So a reader
at this commit finds the wrong arc's review at eight-plus paths, not one.

**Not removed here, deliberately.** Deleting eight unrelated documents is not a
slice-0 measurement change: it touches files no ratification covers, and it would
unilaterally pre-empt **#132**, which is the convention question the scope review
itself flags. F5 explicitly offers "leave it and note it for merge_gate", and that is
what was done — but noted *precisely* rather than in passing. `impl.md` §0 now names
which root documents are this arc's (`impl.md`, `fix-r1.md`) and which are the
previous arc's, states that a reader opening `review-scope.md` gets the wrong arc,
and routes the pattern to `merge_gate` and #132. `impl.md` §7.10 carries the #132 item.

## F6 — the README's false invariant · **FIXED, AND SAID SO**

The claim was that all four ungated arms "write the same two single-element slots".
Verified per arm against `native.rs`, which refutes it for `Poll` and makes it
imprecise for the rest:

| arm | `Session::entry` | `Session::pending` |
|---|---|---|
| `Namespace` | set at `:1913` | one hop later, via `resume()` → `return_stop` at `:2387` |
| `Exec` | — | `return_stop` `:2002`/`:2008` |
| `Wait`+`Native` | — | `return_stop` `:1970` |
| `Wait`+`Poll` | — | — **neither**: rewrites `x0`/`CPSR`/`PC`, `set_regs`, `continue_run` |
| `Fork` (gated) | — | `return_stop` `:1931` |
| `Wait`+`Park` (gated) | — | — sets `s.waiting` only |

The audit's section A′ had this right per arm; the README generalised it and lost it.
The per-arm table is now in the README, in `tests/fixtures.rs`'s module header, and in
`impl.md` §1, each naming the line numbers.

**And it is labelled as what it was.** Lesson 28 says the pass that fixes a false
invariant is a likely place to introduce one, and that is exactly what happened: the
same README edit that correctly replaced a stale count ("All eleven direct tracer
cases are enabled") introduced a false mechanism claim beside it. The README and
`impl.md` §1 both now say the generalisation was wrong and stood for one revision,
rather than quietly presenting the corrected table.

A second imprecision was corrected in the same pass, from the correctness review:
**`Delivery::Namespace` does not call `return_stop` itself.** It records the entry PC
and emits the event; the gate is planted one hop later, in `resume()`
(`native.rs:2387`). Round 0's three write-ups all implied otherwise. The conclusion
is unaffected — the path is one hop longer.

## F7 — `WaitPlan::Native` is a third ungated caller · **FIXED BY STATING IT**

Confirmed: `WaitPlan::Native` is `s.return_stop(ReturnKind::Wait)?` at
`native.rs:1970`, ungated, writing `pending` exactly as `Exec` does. Slice 0 does not
measure it.

**Disposition: state it plainly, defer the case.** The synthesis offers "either
measure `Native` or state plainly that it is unmeasured", and stating it is the
sanctioned option. Adding a third measured case is new scope no ratification covers,
and its verdict would be timing-dependent (`Native` requires a *finished* child, so
the harness's `done` flag must already be set). Inventing a fixture whose expected
result I would then have to defend is worse than the honest sentence.

**All three narrowing copies reconciled.** Round 0 said "Namespace and Exec are the
ungated arms" in three places that then disagreed with the README's four:

- `crates/umbra-platform-macos/tests/fixtures.rs` module header — now carries the
  per-arm table and the explicit "`WaitPlan::Native` is a third ungated
  `return_stop` caller and slice 0 does not measure it".
- `experiments/fixtures/umbra-test-child.c` header comment — now names all four
  ungated arms, says which write what, and points at `fixtures.rs`'s table as the one
  place that must stay reconciled.
- `impl.md` §1 — the two-row table is replaced by the six-row per-arm table with a
  "measured by" column whose `WaitPlan::Native` entry reads **nothing**.

Measuring it is `impl.md` §7.3, for the item-6 issue.

**Not fixed, and named as such:** the related lesson-20 surface the correctness
review notes — cases are now enumerated in four places plus `matrix()`, with no test
asserting they agree, and the README carries a hand-maintained prose count. This
change added to that surface; it did not create it, and pinning it needs a test that
reads all five enumerations, which is its own piece of work. Recorded here rather
than silently left.

## F8 — the remedy space is wider than `impl.md` said · **FIXED**

Round 0 presented two shapes as exhaustive. A third exists on machinery this backend
**already owns**, and it was verified rather than asserted:

- `Task::acquire` takes a Mach task port via `task_for_pid` (`native.rs:45-51`), and
  `Session` holds it as `Arc<Task>` (`:225`) for the session's life.
- `mach2` is already this crate's dependency (`Cargo.toml:49`; `Cargo.lock` 0.7.0),
  and it exposes `mach2::task::task_threads` and
  `mach2::thread_act::thread_suspend`/`thread_resume`.
- The audit's own section F probe already used `task_threads`, so the technique is in
  this graph's toolkit.

So "hold the siblings" is implementable with **zero new RSP wire surface and zero new
dependency**. `impl.md` §3 now carries a three-row costed table (Mach sibling-hold /
per-thread step / per-thread resume) instead of two prose bullets, plus:

- the **blocked** fourth shape, recorded so the next arc does not re-derive it: an
  out-of-line `svc` trampoline would also close the window, but `Session::allocate`
  requests `_M<size>,rw` (`native.rs:435`) — read/write, not executable — so it needs
  new executable-scratch work under W^X and `MAP_JIT`;
- a caution for any shape that keeps the entry site armed across a return:
  debugserver's `Z0` registrations are **reference counted**, and a duplicate `Z0`
  makes the matching `z0` decrement without restoring the instruction while still
  answering `OK`. This crate's README documents being bitten by exactly that in the
  closed `exec-write` M2 gap.

**And the re-ratification ask is re-framed, which is the point of the finding.**
`impl.md` §3 and §7.2 now say the human should be asked to **pick a shape**, not to
grant the deferred `vCont` waiver — a materially cheaper decision than the one the
design gate deferred. Round 0's "`mt-write` needs slice 2, and the `vCont` waiver with
it" is gone.

## F9 — upgrade the orphan leak from assertion to measurement · **FIXED**

Round 0 asserted the leak generalises "from any error between the spawn's `svc` and
`finish_return`, not only from the tripwire". Measured in this round across the 10
enforced `mt-spawn` runs: **one suspended orphan leaked in 10 of 10**, including the
**5 release runs**, where the `debug_assert!` is compiled out and the run instead
fails on an unrelated `Io during path` refusal.

The release column is what makes it a measurement rather than a restatement: the
assertion is absent, the error is a different one, and the orphan still leaks. So the
leak is a property of the **error window**. `impl.md` §2.4 now carries the table; the
`mt_spawn` doc comment carries the figure so someone running it by hand sees it. All
ten were reaped by hand.

## F10 — new: an enforced `mt-spawn` loses the writer lease · **FIXED, AND NARROWED**

Reproduced verbatim, 5/5 in debug:

```
StorageUnavailable during provider.transport: provider disconnected
  (cleanup also failed: LeaseLost during overlay: run left recovery-required:
   supervised tree termination unproven)
```

and the store is left holding `<store>/<run-id>/writer.lock` — recovery-required with
the writer lease lost.

**Narrowed by measurement, which the finding did not do: it is debug-only.** In
release the same case fails through an ordinary `Err` return (`Io during path`), the
provider stays up, and the lease is released — **0 of 5** release runs left a
`writer.lock`. So the lost lease is a consequence of the *panic*, i.e. of the
`debug_assert!`, whereas F9's orphan leak is not. `impl.md` §2.5.1 records both, and
§7.4/§7.5 file them as **separate** issue items for that reason: remove the panic and
F10 goes away while F9 remains.

## F11 — ratified items 6 and 7 outstanding · **DEFERRED TO `publish`**

Unchanged and correct: both are blocked in this workspace by #123/#131 (`.jj`, no
`.git`; `git` and `gh` fail here), and both are `publish` obligations. What this round
changed is the handoff's strength:

- `impl.md` §7.6 now marks item 6 explicitly as a **ratified ship-gate** — FILE ON
  SHIP — with the words "`publish` must not mark the arc complete without it".
- The issue payload is enumerated rather than gestured at: §2's measurements verbatim,
  §2.5's reclassification, the `events.rs:307` guard and its per-`ThreadId` keying,
  and items 3 (measure `WaitPlan::Native`), 4 (orphan leak) and 5 (lost lease).
- Item 7's correction text is unchanged, now bounded by F2's reclassification.

## F12 — PR title · **RECORDED**

`impl.md` §7.9 carries the scope review's recommendation verbatim —
`test(tracer): measure the ungated multithreaded paths before the thread-safe slot
model (slice 0)` — and states why item 1's rename lands across two places: the arc is
renamed to the thread-safe slot model, while this PR ships only its slice 0, so a
title claiming the slot model would over-claim. The commit description stays as
written; both reviews ruled it correct. Setting the title is `publish`'s action, not
this node's.

---

## Gates, re-run against the final tree (lesson 24), verdicts read by name (lesson 23)

The C fixture was recompiled from this tree and `cargo build -p umbra-platform-macos
--tests` re-run **after** the last source edit, before any figure below was taken.

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | exit 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0, **0** lines matching `^(warning\|error)` |
| `cargo test --workspace --all-targets` | exit 0; **52 suites, 832 passed, 0 failed, 5 ignored** |
| `--test fixtures` (integration env) | **11 passed, 0 failed, 2 ignored**; **0** `SKIP`, **0** `MISSED` |
| `--test provider_ipc` | 1 passed — `CAPTURED open-libc provider IPC` |
| `--test sandbox_launch` | 4 passed |
| `-p umbra-cli --test run_fixtures` | 10 passed, **20** distinct `PASS` verdicts |
| `-p umbra-cli --test resume_cli` | 3 passed |
| `-p umbra-supervisor --test reopen` | 7 passed |
| `smoke.sh` untraced | **12 of 12 PASS**, both new arms included |

**The 832 figure carries the same qualification it did in round 0 and it is still the
weaker figure**: that invocation sets no fixture environment and passes no
`--nocapture`, so every integration case in it takes `fixture_argv`'s skip branch
(`tests/fixtures.rs:47`) and reports `ok` with its `SKIP` line invisible. It proves
the suite compiles and the unit tests pass. It proves nothing about interception. The
rows beneath it are the qualification.

**The eleven `CAPTURED` verdicts, read by name from this round's own captured
output** — not carried over: `argv0-check`, `dirfd-rename`, `dup-inherit-write`,
`exec-write`, `fork-write`, `grandchild-write`, `open-libc`, `open-svc`,
`posix-spawn-write`, `symlink-cycle`, `wnohang-wait`. Zero `SKIP`, zero `MISSED` in
that run.

**The two `SKIP`s in `run_fixtures` are skips, not passes:** `SKIP
nfs_fixture_matrix` and `SKIP nfs_utility_matrix`, both on `UMBRA_TEST_SKIP_NFS_MATRIX`
— the same opt-out CI sets for that job. Nothing about a live NFS mount is qualified
here.

**The five `#[ignore]`d tests, by name** (`-- --ignored --list`): three pre-existing
NFS-fault cases in `umbra-storage-nfs/tests/mounted.rs`
(`provider_drop_after_deadline_miss_returns_promptly`,
`scratch_server_killed_after_flush_preserves_export_bytes`,
`scratch_transport_outage_recovers_to_local_without_remote_claim`), plus `mt_write`
and `mt_spawn`.

**Both `#[ignore]`d cases re-measured with `--ignored` against this build**, and they
give the same two verdicts as round 0 — the round-1 edits changed what the documents
claim, not what the tracer does:

```
thread 'mt_write' panicked at crates/umbra-platform-macos/tests/fixtures.rs:398:5:
MISSED mt-write: the second thread's output reached the host at …

thread 'mt_spawn' panicked at crates/umbra-platform-macos/src/native.rs:571:9:
a second intercepted syscall entered while one was still in flight: …
```

The `mt_write` assertion line moved 375 → **398** as the doc comments grew; every
reference in `impl.md` was updated to the current line and the movement is recorded in
§5.5 rather than left to look like drift.

**Release build** (`cargo build --workspace --bins --release`, exit 0) — the build
§2.5's ten release runs were driven through: `target/release/umbra` 3 702 400 bytes,
`target/release/umbra-platform-macos` 1 761 104 bytes. `--bins` reported nothing to
rebuild after the round-1 edits, which is the freshness check: every round-1 edit was
to a test, a fixture, a README or a process document, none of which is compiled into a
binary target.

**`transport-raw`, re-verified by symbols** (not re-measured from round 0's numbers —
the crate was untouched, so the figures are restated as still-true rather than
re-derived): `libnfs_raw.rs` emits 16 functions, the 7 allowlisted raw-RPC entry
points are each present once, the 5 managed lifecycle symbols are each absent, and
`libnfs.a` is 488 944 bytes at pin `18c5c73e…`.

**Guardrails: unchanged and unchangeable this round.** No production file is in the
diff, so `impl.md` §4's eleven pinned sites hold by construction. `single_thread()`
stays (`native.rs:498`, both call sites `:1922`/`:1967`); the `debug_assert!` stays
(`:571`) per ratified item 8; waivers 3 and 4 remain unconsumed.

---

## Not done, and why

- **Slice 1.** Both reviews cleared the slice-0-only stop as mandated by ratified item
  2. Nothing in this round reopens it.
- **A `WaitPlan::Native` fixture** (F7). Stated as unmeasured instead; the case is
  §7.3 for the item-6 issue.
- **Removing the previous arc's root documents** (F5). Out of scope for a slice-0
  measurement change and it would pre-empt #132; noted for `merge_gate` instead.
- **Pinning the five case enumerations with a test** (F7's lesson-20 tail). Its own
  piece of work; recorded above.
- **Items 6 and 7** (F11). Blocked here by #123/#131; `publish`'s obligations, with
  item 6 marked a ship-gate.
- **The severe-branch run under an edited `umbra.sb`** (F2's residual). Would require
  adding a write carve-out to the shipped template. Not run, and nothing claims it.

Change **`wppwptxswssr`** on `feat/mt-fork`, parent `master` `e44d0db8`.
