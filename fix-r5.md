# Fix — round 5 (graph `dg-0ved1w0e`, #135 multithreaded closure, slice 1)

Node `fix`, round 5. Date 2026-09-29.
Change **`qqxtnynk`**, bookmark `feat/mt-closure`, parent `d8a42def`.
Input: `/tmp/graph-dg-0ved1w0e/review-synthesis-r5.md`, with the current
`review-correctness.md` and `review-scope.md` in the tree as its evidence.

> The commit id is not stated; writing this file moves it. `jj log -r qqxtnynk` is
> the authority.

**Both reviews pass.** The documentation class is verified closed: correctness re-ran
its sweep over every unmoved prose line and found no known-false shape surviving. Both
reviewers confirmed the master transcripts intact independently. One finding remained,
found by both, and it is one word.

---

## 1. The deletion

A comment in `fixtures.rs` read:

> *"Symbols rather than line numbers: this table first carried **six of them** and
> every one had rotted by the time the window it describes was closed."*

The second limb is true. The count is not — nothing in that block is six. It now reads:

> *"Symbols rather than line numbers: this table first carried line numbers, and they
> had rotted by the time the window it describes was closed."*

**The numeral is deleted, not corrected.** No cardinality replaces it, in any form.
That was the prescription from both reviewers and from the synthesis, and the reason is
the only reason a round was worth spending on one word: **a deletion cannot introduce a
new false count.** Every prior round of this class failed to terminate because it
restated. This one removes rather than restates, so there is nothing left to be wrong.

Scope's observation is the part worth carrying, and it is why this sentence and not
another: **this is the one place in the arc where "don't state the count" was the
stated intent of the sentence, and a count was stated anyway.** The remedy was being
written down and violated in the same breath. That is a sharper demonstration than any
of the instances catalogued in `fix-r2.md` §6, because it removes the last available
explanation — it cannot be attributed to not knowing the rule.

---

## 2. Nothing else changed

`jj op diff` between this round's snapshot and the previous one names exactly one
file: `crates/umbra-platform-macos/tests/fixtures.rs`. That is the whole round.

| standing constraint | verdict |
|---|---|
| `native.rs` untouched | **byte-identical to the visit-2 verified state**, confirmed independently: its sha256 begins `81e1cd50c79f3af6`, the value the synthesis cites, reproduced here rather than taken on trust |
| `z0`/`Z0` byte-identical | filter over the whole diff returns **nothing** |
| `fixtures.rs` non-comment invariant | **six changed lines vs master, all removals** — the two `#[ignore]` attributes |
| `umbra-test-child.c` | **zero** non-comment changes, and unchanged this round |
| reaper components vs master | `StrayFixtureChildren`, its `Drop`, `fixture_named_processes`, `fixture_processes_named` — all identical, compared by **body extraction**, not a fixed window |
| master transcripts | `native.rs:571` appears exactly once, inside its fence, unmodified |
| `single_thread()` with both call sites | unchanged, still pinned by test |
| deferred items 3 / 4b / 5 / 6 | absent; the reaper documented, never touched |
| no line numbers in new text | none; the replacement names no line and no number |
| attribution trailers | both intact |

**Left as improvable, not false** — unchanged from round 4, and deliberately not
reopened: the two-layer `mt_write` / `mt_spawn` doc comments; the orphan paragraph's
depth in `fixture_named_processes`; the README / `fixtures.rs` near-duplication, which
is a real improvement and a real risk and belongs to whoever owns that consolidation;
`#134`'s process documents, routed to #132; and `native.rs`'s own comments, which are
under a do-not-touch and were written against the current tree.

**Nothing new and false was found this round.** The one thing this round's own
verification turned up is in §4, and it is a defect in my *method*, not in the tree.

---

## 3. Closing data for the merge-gate presentation

### The `101` has a candidate mechanism, and still no asserted cause

Correctness produced one from its own run: its first workspace invocation exited `101`
with `T09` because `review_scope` was running concurrently and the reaper `SIGKILL`ed
its tracees; the serialised re-run was clean. `cargo`'s fail-fast after one failing
suite produces exactly that truncated-output / exit-`101` signature — which is why the
original instance had no log — and **the anomaly first appeared in the round that first
activated the reaper.**

**Neither reviewer asserts this as the cause, and neither do I.** Round 0's instance was
never identified and cannot now be. What has changed is that *"unexplained"* is no
longer the only available disposition: there is a mechanism on this machine that
produces that exact signature, demonstrated rather than hypothesised. That is a
materially better closing position on an item open since round 0, and it is as far as
the evidence reaches.

### Both reviewers experienced the documented hazard, from opposite sides

Correctness took the `101`/`T09` as the victim. Scope caught a contaminated run by its
`REAPED=4` signature and discarded it, as the aggressor. The harness note written in
round 3 describes something both reviewers then independently ran into — which is about
as strong a validation as a hazard note gets, and it arrived after the note rather than
before it.

It is also why every suite run in this round was **serialised**, with nothing else
running, and why each gate log was checked for `REAPED` lines: all zero.

### A comparison is only as good as its anchor

Scope reproduced the cause of the fixed-window comparison hazard I hit in round 4:
`struct StrayFixtureChildren` sits at a different line at master because the comment
above it grew, so a fixed-size window straddles different content on each side and
reports a difference that is not there. **Body extraction is the correct method**, and
it is what §2 above uses. Scope filed this beside its own transient bad reads this arc
— two digests, a `grep` that dropped a line plainly present, a `diff -q` false
difference — under that heading. §4 adds one of mine to the same pile.

### The completing move on falsified prose — both halves

The arc's most transferable output is these two together, and neither is sufficient
alone:

1. **Correctness's:** *a keyword sweep for "prose that stopped being true" finds most of
   it and not all of it; the completing move was **enumerating every unmoved line**
   rather than grepping for the shapes you expect.* Two independent keyword sweeps in
   round 4 did not overlap and neither was exhaustive; the enumeration is what closed
   the class.
2. **Mine:** *when a change closes a defect, sweep every file that **describes what it
   changed** — a set disjoint from the files the change touched.* The files this change
   edited were not the files that had gone false; byte-identity certification is
   precisely the instrument that cannot see the difference.

Both reviewers also corrected themselves in-document this round rather than quietly:
correctness's draft said "all four fenced blocks", read off fence *markers* which are
*pairs* — its own finding's exact class, recorded as such; scope logged its transient
bad reads. Every role in this graph has now both produced an instance of this class and
caught one.

---

## 4. This round's own verification defect

Recorded because the arc's standard is that a bad method is worth more in the open than
a clean-looking result.

`umbra-test-child.c` did not change this round, so its compiled fixture should have
been current. I checked that by rebuilding it and running `cmp` against the existing
binary. **They differed, and I initially read that as "stale".** They differ because
`clang` is not deterministic here: two builds of *identical* source differ in a few
hundred bytes — the embedded build identity — so `cmp` on a compiler's output cannot
answer "is this binary current".

The conclusion was harmless because I rebuilt, which is the safe direction, and the
suite then ran against a binary built from the tree under test. But the check itself
proved nothing, and a check that proves nothing while appearing to is exactly what this
arc keeps finding. The sound version is to compare the *source* — which the `jj op
diff` in §2 does — and rebuild unconditionally. Filed alongside scope's anchor
instances: **a comparison is only as good as its anchor, and a compiler's output is not
one.**

---

## 5. Gates

Every suite run **serialised**, nothing else running, each log checked for `REAPED`.

| Gate | Verdict |
|---|---|
| `cargo fmt --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 |
| `cargo test --workspace --all-targets -- --test-threads=1` | **exit 0**, 52 suites ok, 0 failed, 3 ignored (the pre-existing NFS fault-injection cases), **`REAPED` lines: 0** |
| direct tracer + `provider_ipc` + `sandbox_launch` + `--lib` | exit 0; **14 `CAPTURED`, 0 `SKIP`**, **`REAPED` lines: 0**; 13 / 1 / 4 / 36 passed |

**Qualified by `CAPTURED` (lesson 23)** from the `--nocapture` run; the workspace run
captures stderr, and its `ok` is exactly the signal these two cases can emit without
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

The tracer provider, same discipline and built immediately before reading:
`Session15continue_thread`, `Session17continue_absorbed`, and the `vCont;c:` literal.

The fixture binary was rebuilt from the tree under test before the suite ran, for the
reason in §4.
