# Fix — round 4 (graph `dg-0ved1w0e`, #135 multithreaded closure, slice 1)

Node `fix`, round 4. Date 2026-09-29.
Change **`qqxtnynk`**, bookmark `feat/mt-closure`, parent `d8a42def`.
Input: `/tmp/graph-dg-0ved1w0e/review-synthesis-r4.md`, with the current
`review-correctness.md` and `review-scope.md` in the tree as its evidence.

> The commit id is not stated; writing this file moves it. `jj log -r qqxtnynk` is
> the authority. The worktree-root `review-synthesis-r*.md` files belong to other
> arcs.

**Both reviews pass. No correctness defect, no scope breach, no code problem.**
`native.rs` is unchanged since visit 2 by blob identity and is **untouched again this
round** — verified byte-identical to the round-3 tree. This round is one exhaustive
documentation pass over three files, and nothing else.

---

## 0. What the pass covered, and why it was a pass rather than a list

The synthesis's table was explicitly a **floor**: two independent sweeps found
non-overlapping sets, and scope had certified the README clean on the sentences it
checked while correctness quoted two false ones from that same README. Working the
table item by item would have sampled the class a third time.

So this was an exhaustive pass over **all of**:

- `crates/umbra-platform-macos/tests/fixtures.rs`
- `crates/umbra-platform-macos/README.md`
- `experiments/fixtures/umbra-test-child.c`

for: present-tense claims about tracer behaviour; claims about what `mt_write` /
`mt_spawn` do (fail, are ignored, panic, leave strays, poison a mutex, produce
orphans); claims about `continue_run` resuming every thread or siblings walking
through the window; predictions phrased as current expectations; and citations into
`native.rs`.

**Method**: every `native.rs` citation enumerated by grep; a trigger-phrase scan; then
a re-sweep by regex against the repaired files to confirm the class terminated; then a
third pass reading every remaining mention of the two cases in prose. The third pass
is what found the largest site. Residual matches after repair are listed in §4 with
the reason each is correct as written.

**Repair style, per the synthesis:** past tense and named symbols, not deletion. These
comments carry the arc's reasoning and are worth keeping as history — they were wrong
only where they read as present tense about the shipped tree. Where a sentence was
*replaced*, the superseded wording is quoted inside the replacement so the record of
what was believed survives beside the correction.

---

## 1. The one thing that must not be touched — and was not

**`native.rs:571` inside the fenced `text` block headed "Measured against master
(`e44d0db8`), three runs out of three" is a verbatim transcript of a master panic.**
Renumbering it would falsify the evidence. Reproducing the number master printed is
the correct behaviour.

It is intact. The repair script that touched `mt_spawn`'s doc asserted the
transcript's occurrence count before and after its edits and refused to write
otherwise; the final tree carries the string exactly once, and both *"Measured against
master"* blocks are present and unmodified. **The framing around them was moved to
unmistakable past tense; the transcripts themselves were not edited at all.**

The same rule was applied to `mt_write`'s `MISSED …` transcript, to the *"5 of 5
enforced release runs"* and *"10 enforced `umbra run` executions"* figures, and to the
*"0 of 10 runs"* supervisor-race figure — all master measurements, all left as
measured, with only the sentences introducing them made past-tense.

---

## 2. Every site repaired

### Named by the reviewers

| site | what it claimed | repair |
|---|---|---|
| `fixtures.rs` — `StrayFixtureChildren` doc | the unwinding path is *"the only path that matters: on `mt-spawn` the tracer's `debug_assert!` fires inside `next_event`"* | past tense; states the assertion **no longer fires** and that `Drop` is what keeps cleanup on the ordinary failure path |
| `README.md` | *"Neither reaches it today."* — exactly backwards | **"Both reach it on every run"**, with the superseded claim named as the thing that changed |
| `fixtures.rs` — `fixture_named_processes` | the orphan *"Measured on every `mt-spawn` run"* | scoped to *while that case still failed inside the spawn's error window*; says it is **not** measured on every run now, and why the reaper stays |
| `fixtures.rs` — `mt_fixture` body | the unwinding panic *"is the case that actually happens here"* | past tense; now the ordinary failure path |
| `fixtures.rs` — `Second` doc | *"both unwind past the end of this function… on the runs that matter"* | past tense, plus why panic-safe teardown still matters when the expected path is success |
| `fixtures.rs` **and** `README.md` — `WaitPlan::Native` | *"expected to collide the way `mt-spawn` does"* | both repaired separately (near-duplicates, not one shared sentence): the prediction is **overtaken rather than confirmed**, and the reason it is overtaken is the reason `mt_spawn` passes |
| `fixtures.rs` — `mt_write` doc | present-tense superseded mechanism (`continue_run` *"resumes every thread"*, a sibling *"walks straight through it"*) | explicit heading that the paragraphs describe the mechanism **as it stood at that measurement**; the mechanism kept verbatim as history; a **"What closed it"** paragraph added |
| `fixtures.rs` — three citations | `native.rs:498` + its two call-site numbers, `:2387`, `:539` | **symbols named**: `Session::single_thread`, `Session::resume`, `Session::return_stop` |
| `umbra-test-child.c` | *"the window **the single-slot model** has to survive"* | past framing, superseded wording quoted, rationale for the barrier restated in terms that do not depend on the slot model |
| the harness note | *"the reaper takes its reap-nothing path"* | **mechanism corrected**: with the env unset, `mt_fixture` returns **before the guard is constructed**, so `Drop` never runs at all |
| the harness note | *"CI is unaffected"* | **scoped to what was measured**: within one workflow run. Two workflow runs can coexist — the concurrency group is per ref — and that path is named as neither measured nor excluded |
| `fix-r3.md` §6 | *"a section about wrong counts that contained four of them"* | count removed, no replacement; the imprecision (one of the items was a misattribution, not a count) fixed in the same sentence |

### Found by this pass, named by neither reviewer

| site | what it claimed | why it matters |
|---|---|---|
| `README.md` | *"A multithreaded tracee doing ordinary file I/O or a `posix_spawn` is therefore **not refused; it is unmediated**."* | **Simply false now** — that is precisely what the change closed. The largest single site in the pass, in the README both sweeps had been over. Repaired to past tense with a sentence naming what closed it. |
| `README.md` | *"`mt-write` is a second, distinct defect that per-thread slots do **not** close."* | present tense on a closed defect; the *slots-do-not-close-it* half is still true and measured, so it was kept and re-tensed rather than dropped |
| `README.md` | *"The two cases **fail** differently for one reason…"* | present tense; also now records that the resume closes both and the slots only the second |
| `README.md` | *"A release build compiles that assertion out, so the backend **takes** the overwrite silently"* | present tense; scoped to the tree the measurement was taken against, with *"not reachable now, in either profile"* |
| `fixtures.rs` — `mt_spawn` doc | *"which says that layer already models per-thread operations correctly **while this backend's slots do not**"* | **False now.** This backend's slots *are* keyed by thread — that is what this change did. The observation is kept because the asymmetry was the argument for closing the case. |
| `fixtures.rs` — `mt_spawn` doc | *"the spawn's `ReturnKind::Spawn` **is** in flight… **takes** the overwrite… what **ends** it is the path decode"* | a present-tense block describing a sequence no longer reachable; re-tensed, transcripts and figures untouched |
| `fixtures.rs` — `mt_spawn` doc | *"The tripwire **fires** after the `posix_spawn` syscall has already run"* | re-tensed, with the reason the reaper stays (any error reaching that window has the same effect) |
| `fixtures.rs` — the ungated-arms comment | *"a multithreaded tracee … **is** not refused: it **is** unmediated"* | the fixtures.rs twin of the README's largest site; repaired with an explicit *"that window is now closed"* |
| `fixtures.rs` — the per-arm slot table | further rotted `native.rs` line numbers neither review named — every line number the per-arm table carried, plus `single_thread`'s two call-site numbers | all replaced with symbols, and the table now records that every number it once carried had rotted |
| `umbra-test-child.c` — `mt_write`'s comment | *"the tracer **has** two in-flight namespace transactions to keep apart on one `pending` slot and one `entry` slot"* | present tense on the replaced model; re-tensed, and the case is noted as unchanged and still entering the same way |

**That is the expected outcome and not a failure**, as the synthesis said. The pattern
is worth noting: the sites neither reviewer found are concentrated in the *longest*
doc comments — `mt_spawn`'s and the README's fixture section — where a false sentence
sits many paragraphs from anything that looks like a claim about the current tree.

---

## 3. Deliberately left

Per the instruction not to let round 5's scope creep, these are **improvable but not
false**, and were left alone:

- **`mt_write`'s and `mt_spawn`'s doc comments are long and now carry two layers**:
  the measurement, and the correction. A reader has to hold both. Compressing them
  would lose the arc's reasoning, which the synthesis explicitly asked to keep.
- **`fixture_named_processes`' orphan paragraph** still explains a leak in more depth
  than a reader of the reaper strictly needs. Accurate as repaired; not shortened.
- **The README's fixture section and `fixtures.rs`' ungated-arms comment are
  near-duplicates.** They were repaired *separately and consistently*, as the
  synthesis flagged for `WaitPlan::Native`. Merging them into one source of truth is a
  real improvement and a real risk, and it is not this round's.
- **`#134`'s process documents** carry the same falsified sentence. **Not this arc's
  to fix** — routed to #132 with the root-document convention item.
- **`native.rs`'s own comments** were not swept. The synthesis scoped this pass to
  three files and `native.rs` is under a do-not-touch. Its comments were rewritten
  wholesale by this change and are the one body of prose in the crate that was
  *written against the current tree*.

---

## 4. Residual matches after the re-sweep, each correct as written

The regex re-sweep leaves these, and none is a defect:

- `fixtures.rs`, twice — *"Reaping here does not close it"*. True: reaping does not
  close the orphan leak, which is the point of the sentence.
- `fixtures.rs`, once — the protected `native.rs:571` master transcript (§1).
- `umbra-test-child.c`, once — *"single-slot model has to survive"* appearing inside
  its **own replacement**, quoted as the superseded wording. Keeping it is the repair
  style, not a miss.

---

## 5. Standing constraints, re-verified after the pass

| constraint | verdict |
|---|---|
| `native.rs` untouched | **byte-identical** to the round-3 tree (`diff -q` clean) |
| `z0`/`Z0` byte-identical | the filter over the whole diff returns **nothing** |
| `fixtures.rs` six-non-comment-line invariant | **6 changed lines, all removals**, all the two `#[ignore]` attributes |
| `umbra-test-child.c` comment-only | **0** non-comment changed lines |
| reaper components byte-identical to master | `StrayFixtureChildren`, its `Drop`, `fixture_named_processes`, `fixture_processes_named` — **all four identical**, compared by extracted body rather than by a fixed-size window |
| `single_thread()` with both call sites | unchanged, still pinned by test |
| deferred items 3 / 4b / 5 / 6 | absent; item 5's reaper documented, not touched |
| no new line numbers in new text | none added; three rotted citations replaced by symbols, six more found and replaced |
| no counts of this arc's own corrections | none supplied anywhere, including in this document |
| attribution trailers | both intact |

---

## 6. The finding this round adds

The arc's transferable finding was already that false documentation is a first-class
defect here. This round sharpens it in a way worth carrying:

**Byte-identity certification is the instrument that cannot see this class.** Every
site repaired above sat in text that had not moved for rounds, and was therefore
certified safe by every previous round — mine, both reviewers', and the driver's.
Scope put it exactly: the *"no line numbers in new text"* remedy protects **new**
prose only, and byte-identity is *"what makes the first invisible."*

**And two independent sweeps were not enough.** Correctness and scope swept the same
README and did not overlap; scope reported it clean on what it checked while
correctness quoted two false sentences from it. This exhaustive pass then found more
than both — including the largest site, in that same README. The lesson is not that
the sweeps were careless. It is that **sampling a class of defect does not terminate
it**, and that the only sweep that ends it is one whose stopping condition is
"exhausted the file", not "found some".

The remedy that follows, and that belongs in the standing checklist beside the
byte-identity check: **when a change closes a defect, sweep every file that documents
that defect, exhaustively, in the same change.** Not the files the change touched —
the files that describe what it changed. Those are disjoint sets, and this arc spent
four rounds discovering it.

---

## 7. Gates

| Gate | Verdict |
|---|---|
| `cargo fmt --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 |
| `cargo test --workspace --all-targets -- --test-threads=1` | **exit 0**, 52 suites ok, 0 failed, 3 ignored (the pre-existing NFS fault-injection cases) |
| direct tracer + `provider_ipc` + `sandbox_launch` + `--lib` | exit 0; **14 `CAPTURED`, 0 `SKIP`**; 13 / 1 / 4 / 36 passed |

**The C fixture was rebuilt before the suite ran.** `umbra-test-child.c` changed this
round — comments only, but it is the tracee under test, and running the suite against
a stale binary would qualify something other than the tree. `clang -arch arm64` per
the CI recipe, then the suite.

**Qualified by `CAPTURED` (lesson 23)** from the `--nocapture` run; the workspace run
captures stderr and its `ok` is exactly the signal these two cases can emit without
running.

**Rebuild discipline (lesson 24) and the mtime hazard.** The `transport-raw` provider
was rebuilt **immediately before** its symbols were read, because a default-feature
`cargo build --workspace --bins` overwrites that path *and moves mtime backwards*:

```
$ cargo build -p umbra-storage-nfs-userspace --features transport-raw --bins
  warning: …: libnfs raw binding: 16 functions emitted
$ nm target/debug/umbra-storage-nfs-userspace | grep -E " _rpc_(connect_async|service|nfs4_compound_task)$"
00000001001244ec T _rpc_connect_async
0000000100128518 T _rpc_nfs4_compound_task
0000000100123464 T _rpc_service
```

The tracer provider, same discipline: `Session15continue_thread`,
`Session17continue_absorbed`, and the `vCont;c:` literal, read from a binary built
immediately before.
