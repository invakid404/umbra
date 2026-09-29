# Review — scope (graph `dg-0ved1w0e`, #135 multithreaded closure, slice 1)

Node `review_scope`, **visit 6** — single-item confirmation. Date 2026-09-29.
Remit: scope only. Mechanism correctness is `review_correctness`'s node.

Inputs read end to end across this arc: `/tmp/graph-dg-0ved1w0e/audit.md`,
`/tmp/graph-dg-0ved1w0e/design-gate.md` (the ratification — the yardstick),
`impl.md`, `fix-r1.md` … `fix-r5.md`.

**Verdict: PASS — clean. Nothing false found.** R-1 is repaired by deletion, the
repaired sentence is true on every clause, the envelope is unmoved, and all five
dispatched confirmations reproduce independently. No new sweep class opened; the
deliberately-left items stay left, and are listed as left.

---

## Pin

| | |
|---|---|
| Change | `qqxtnynk`, bookmark `feat/mt-closure` |
| Commit reviewed | **`b4f2d684`** |
| Parent | `d8a42def` |

No commit id is asserted as current; `jj log -r qqxtnynk` is the authority. Every
line number below is a **quotation** — of master's removed `#[ignore]` text, or of a
citation under discussion — never a location looked up and published.

---

## Part 1 — The five confirmations, reproduced independently

| # | Claim | Confirmed |
|---|---|---|
| 1 | No cardinality replaces the numeral | ✅ `grep -inE 'carried (six\|five\|four\|three\|two\|one\|eight\|nine\|ten\|[0-9]+)'` over the file: **zero matches** |
| 2 | Only `fixtures.rs` moved | ✅ `jj diff --from d95b3361 --to b4f2d684 --name-only`: `fixtures.rs` plus `fix-r5.md`, `review-correctness.md`, `review-scope.md` — the three process documents the workers write. **One source file.** And within it, the **only** content change is the single line |
| 3 | `native.rs` sha256 begins `81e1cd50c79f3af6` | ✅ reproduced: `81e1cd50c79f3af62cd6c07f…`, the visit-2 fully-verified value |
| 4 | `native.rs:571` exactly once, inside the master transcript | ✅ **one** occurrence, and it is the only `native.rs:NNN` citation left in the file; it sits inside the ` ```text ` fence under *"Measured against master (`e44d0db8`), three runs out of three"* |
| 5 | Non-comment changed lines vs master | ✅ `fixtures.rs` **exactly six** — 6 removals, **0 additions**, all the two `#[ignore]` attributes; `umbra-test-child.c` **zero** |

### The reaper, by body extraction

Symbol-anchored, per the hazard I reproduced at visit 5 — `struct
StrayFixtureChildren` sits at **line 560 at master and 621 here** because the comment
above it grew, so any fixed window straddles different content and lies.

| component | master vs `b4f2d684` |
|---|---|
| `struct StrayFixtureChildren` | **BYTE-IDENTICAL** (`f3ef65979aad81c4`) |
| `impl Drop for StrayFixtureChildren` | **BYTE-IDENTICAL** (`494ac7f222ba6f49`) |
| `fn fixture_named_processes` | **BYTE-IDENTICAL** (`b1c004fbebca1f50`) |
| `fn fixture_processes_named` | **BYTE-IDENTICAL** (`988b4952789e707b`) |

Third consecutive visit at these four digests. Deferred item 5's machinery remains
unopened.

---

## Part 2 — The repaired sentence is true

> `// claim in this crate's README on the first pass. Symbols rather than line`
> `// numbers: this table first carried line numbers, and they had rotted by the`
> `// time the window it describes was closed.`

Checked clause by clause against the master blob and this tree:

| clause | verdict |
|---|---|
| *"this table first carried line numbers"* | ✅ **true** — the table carried them at master |
| *"they had rotted"* | ✅ **true** — I read each cited line at master and here. All rotted: what was `s.entry = Some(pc)`, `return_stop(Fork)`, `return_stop(Wait)`, `return_stop(Spawn)` and `return_stop(Exec)` now holds unrelated code at every one of those lines |
| *"Symbols rather than line numbers"* | ✅ **true** — the repaired block carries **zero** line numbers |
| any cardinality | ✅ **none** — confirmation 1 |

**The repair introduced nothing false**, and it is the arc's one correction that
added no new claim at all — because it removed rather than restated. That is the
substantive point, and it is worth more than the word it removed.

---

## Part 3 — Envelope unmoved

| Item | Verdict |
|---|---|
| Waiver (a) | ✅ **5 of 6** — `regs`, `set_regs`, `return_stop`, `finish_return`, `intercept` each exist at master and gained a TID; `continue_thread` is absent at master, so a new fn, not a bound consumer |
| Waiver (b) | ✅ **one packet** — a single production `vCont;c:` send; the rest is doc text |
| Waiver (c) | ✅ disclosure **present and exact**, wrapped across two `///` lines: *"a **watchdog kill naming a timeout, not a hang**"* |
| New `#[test]` fn this round | ✅ **none** — `native.rs` 3 → 7, unchanged since visit 3; `fixtures.rs` 15 → 15 |
| `--lib` | ✅ **36** |
| Existing test body edited | ✅ none — the six non-comment changed lines are all attribute removals |
| Attribute-form `#[ignore]` | ✅ **zero** |
| Deferred item 3 | ✅ production-only counts identical to master: `fn single_thread` 1, call sites **2** |
| Deferred 4b / 5 / 6 | ✅ absent |
| `Cargo.toml` / `Cargo.lock` | ✅ absent from the diff |
| `events.rs`, `abi.rs`, interposer, storage | ✅ absent from the diff |
| Confinement | ✅ default workspace has nothing outside `knowledge/`; no untracked files |

---

## Part 4 — Gate figures

**Reconciled fresh at this commit:**

| Gate | Measured | |
|---|---|---|
| `cargo fmt --check` | exit 0, clean | ✅ |
| `clippy --workspace --all-targets -D warnings` | clean | ✅ |
| `cargo test --workspace --all-targets` | `EXIT=0`; **52** suites, 838 passed, **0** failed, **3** ignored | ✅ **EXACT** |
| `--lib` | **36** | ✅ **EXACT** |
| C fixture | rebuilt from source before any run | ✅ |

**The direct-tracer figure is carried from visit 5, and I am stating that plainly
rather than presenting a run I did not get.**

I made three attempts. The first I ran without gating on my own process check —
three sibling processes were live, and it returned `12 passed / 1 failed` with
**`REAPED=2`**. Discarded, and attributed: the reaped pids were running the twin
binary at `~/Library/Caches/umbra/twins/<hash>/umbra-test-child`, i.e. the sibling
suite's live tracees, and the casualty was **`grandchild_write`** — not an MT case.
A third victim name alongside fix-r2's measured `posix_spawn_write`, `symlink_cycle`,
`mt_write` and `mt_spawn` is consistent with *"it lands on whichever tracee happens
to be live"*, not a new pattern.

Two further attempts gated on sustained idle (60s, then 18s) and **both aborted
without running** — `review_correctness` is running essentially continuously in its
own final round, and no idle window opened in twenty minutes. The gates failed
closed, which is the correct behaviour. **I did not brute-force past them**: firing
my suite repeatedly would SIGKILL their tracees and corrupt their final
measurements, which is too high a price for re-confirming a figure that cannot have
changed.

**Why it cannot have changed, measured rather than assumed:**

- `native.rs` is **byte-identical** between the visit-5 tree and this pin.
- `fixtures.rs` differs from the visit-5 tree by **zero non-comment lines** (2 total,
  the single comment sentence reflowed).
- So **no executable statement differs** from the tree I measured at visit 5.

At visit 5 that tree returned, in **two independent clean runs** (`REAPED=0` both):
**13 passed / 0 failed / 0 ignored**, **13 distinct `CAPTURED`**, `SKIP=0`,
`MISSED=0`, `T09=0`, and **14 `CAPTURED`** including `CAPTURED open-libc provider
IPC`, with `sandbox_launch` 4 passed. Those are the figures of record for this pin,
and their basis is byte-identity of everything that executes.

The transport-raw symbol check likewise carries from visit 5, where it reproduced
byte-for-byte from a feature build made immediately before `nm`; `Cargo.lock` and the
provider crate are untouched since.

---

## Part 5 — Left as improvable-not-false

Per instruction, and I agree with leaving each: the two-layer doc comments, the
orphan paragraph's depth, the README / `fixtures.rs` near-duplication, #134's
process documents (→ **#132** with my N5), and `native.rs`'s own comments.

One item this round surfaced and I am **leaving**: the repaired sentence reads
*"Symbols rather than line numbers: this table first carried line numbers…"*, which
repeats the phrase within a clause. It is **true**, it is clear, and rewording it
would be a restatement of exactly the kind this arc has spent five rounds learning
not to make for cosmetic reasons. Left, and reported as left.

**The falsified-prose class stays closed.** It was terminated at visit 5 by an
enumeration over all unmoved prose lines rather than by sampling, which is the right
way to close a class I could only sample. I opened nothing new.

---

## A recorded disagreement, now resolved

The driver overrode my visit-5 recommendation to publish with R-1 merely recorded,
and opened this round for one word. I still think a numeral does not merit a review
round **on its own merits** — that was my position and I would take it again on the
same facts.

But the override was right for a reason I had not weighed: **a deletion cannot
introduce a new false count**, so it is the one repair shape that *terminates* this
class instead of iterating it. Every earlier round in this arc failed to terminate
because it restated. The evidence is now on the record: five rounds of restatement
each produced a fresh false claim, and the single round of deletion produced none.
That asymmetry is a better argument for the override than anything available before
it was run, and it is worth more to the next arc than the word was worth to this one.

The sharpened framing belongs in the merge-gate record: this is the one place in the
arc where *"do not state the count"* was **the sentence's own stated intent** and a
count was stated anyway — which removes the last available explanation, since it
cannot be attributed to not knowing the rule.

---

## Verdict

**PASS — clean. Nothing false found.**

- **All five dispatched confirmations reproduce independently**: no cardinality
  (zero grep matches); one source file moved, one line within it; `native.rs` sha
  `81e1cd50c79f3af6`; `native.rs:571` exactly once inside the fenced master
  transcript; non-comment changed lines **six** in `fixtures.rs` and **zero** in
  `umbra-test-child.c`.
- **The four reaper components are byte-identical to master by body extraction**,
  third consecutive visit at the same digests. The anchor moved 61 lines, which is
  why a fixed window would have lied.
- **The repaired sentence is true on every clause**, asserts no cardinality, and
  introduced nothing new.
- **The envelope is unmoved**: waivers (a) 5 of 6, (b) one packet, (c) exact; no new
  `#[test]` fn; `--lib` 36; no existing test body edited; deferred items 3 / 4b / 5 /
  6 absent; no `Cargo` change; confinement clean.
- **Gates reconcile**: `fmt`, `clippy`, `--lib` **36** and the workspace **52 / 0
  failed / 3 ignored** freshly measured here; the direct-tracer figure carried from
  visit 5's two clean runs on the basis that **no executable statement differs**,
  with the one contaminated attempt disclosed and attributed to the known mechanism.

**Recommendation: publish.** Nothing from this node is outstanding, and there is
nothing I would ask a sixth fix round to change.

**Routing: `review_scope` → publish.**
