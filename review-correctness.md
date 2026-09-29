# Review — correctness (graph `dg-0ved1w0e`, #135 multithreaded closure, slice 1)

Node `review_correctness`, **visit 6** (single-item confirmation). Date 2026-09-29.

**Verdict: CLEAN PASS. Merge-ready.** All five confirmations hold. The repaired
sentence is true and carries no cardinality. The gates reconcile. **I found nothing
false, and I opened no new sweep.**

N1 — the only finding outstanding from visit 5 — is closed by deletion. The entire
code change this round is one line.

---

## Pin

| | |
|---|---|
| Change | `qqxtnynk` |
| Reviewed commit | **`b4f2d684`** |
| Parent | `d8a42def` |

`jj diff --from b4f2d684 --to @ --name-only` was empty at the start of this review.
Re-verified at the end: **every source file and every `fix-r*.md` is byte-identical
to the pin**; the only paths that differ are this document and `review-scope.md`,
which are the two reviewers' own deliverables and differ because we are writing
them. **The current commit id is not stated** — saving this file advances it, and
`jj log -r qqxtnynk` is the authority.

---

## The five confirmations

**1 — The numeral is gone, with no cardinality replacing it.**
`grep -cEi 'carried (six|five|four|eight|three|two|one|[0-9]+)'` over the whole of
`fixtures.rs` returns **0**. The sentence now reads:

> *"Symbols rather than line numbers: this table first carried line numbers, and
> they had rotted by the time the window it describes was closed."*

**2 — Only `fixtures.rs` moved.** `jj diff --from d95b3361 --to b4f2d684 --stat`
names `fixtures.rs` (2 lines) plus `fix-r5.md` and the two reviewers' documents.
The complete code change is:

```
-// numbers: this table first carried six of them and every one had rotted by the
+// numbers: this table first carried line numbers, and they had rotted by the
```

**3 — `native.rs` sha256 begins `81e1cd50c79f3af6`** — reproduced here, and a
`diff` against `7811be20` confirms it is byte-identical to the tree I verified end
to end at visit 2.

**4 — `native.rs:571` appears exactly once** in `fixtures.rs`, still inside the
fenced ` ```text ` block under *"Measured against master (`e44d0db8`), three runs
out of three"*. The master transcript is untouched.

**5 — Non-comment changed lines versus master:** `fixtures.rs` **exactly six, all
removals** (the two `#[ignore]` attributes); `umbra-test-child.c` **zero**.

---

## The repaired sentence is true

Both limbs, each already measured at visit 5 and unchanged since:

- *"this table first carried line numbers"* — the master version of the table rows
  carried `:1913`, `:1931`, `:1970`, `:2002` and `:2008`.
- *"and they had rotted by the time the window it describes was closed"* — I
  checked every retired citation at visit 5; each now points at unrelated code
  (`_M` scratch allocation, `prepare_rewrite`, a bare `)?;`, a path length,
  `operation,`, `attach_child`'s assert, `} else {`, `install_breakpoint`). Not one
  still names its subject.

The claim that was false is gone and nothing was substituted for it. **N1 closed.**

---

## Gates at `b4f2d684`, serialised

| Gate | Reported | **Measured** |
|---|---|---|
| `cargo fmt --check` | clean | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 | exit 0, no issues |
| `cargo test --workspace --all-targets -- --test-threads=1` | exit 0 / 52 / 0 failed / 3 ignored | **exit 0, 52 ok, 0 failed, 3 ignored** |
| direct tracer | 13 `CAPTURED`, 0 `SKIP` (14 with provider IPC) | **14 `CAPTURED`, 0 `SKIP`**; 13 + 1 + 4 passed |
| `--lib` | 36 | **36 passed** |

The C fixture was rebuilt before the suite ran; the binary hashes to
`a2c278ac91a7df34`, the same value as the previous two rounds, as expected from a
`.c` with no changes this round.

### The reaper collided with `review_scope` again, and it is not a finding

My first workspace run exited **101** — 20 suites ok, 1 failed — with two
`fatal signal/exception: T09` failures, this time in `open_svc` and
`posix_spawn_write`. Last round it was `fork_write` and `grandchild_write`. That
difference is itself the documented behaviour: the reaper kills *"whichever case
happened to be live"*. The process table was clear when I started; `review_scope`'s
suite began during mine.

**Re-run with nothing else running: exit 0, 52 suites, 0 failed, 3 ignored, zero
`T09`, zero `REAPED`.** Reported figures confirmed. Recorded per your instruction
as the known mechanism rather than as a new finding — and it is now the second
independent in-the-wild reproduction of it, on different cases, which is further
corroboration that the shipped harness note describes the behaviour accurately.

*(One self-correction: my first attempt at the re-run silently did nothing, because
I chained it behind a `grep -c` that exits 1 when it counts zero. My own shell, not
the tree. Re-run properly, with the result above.)*

---

## What I did not do

Per your instruction, and reporting it rather than acting on it:

- **No new sweep was opened.** Item (7)'s class was closed at visit 5 by
  enumerating all 1244 unmoved prose lines across `fixtures.rs`, the crate README
  and `umbra-test-child.c`; that enumeration came back clean and nothing in this
  round's one-line change could reopen it.
- **Items (1)–(6), (8), (9) and (11)** were discharged in full at visit 2 against a
  `native.rs` whose digest I reproduced above. Not re-run.
- **The improvable-not-false items stay left**: the two-layer doc comments carrying
  both a measurement and its correction, the orphan paragraph's depth, the
  README/`fixtures.rs` near-duplication of the `WaitPlan::Native` note, #134's
  documents, and `native.rs`'s own comments. I re-read none of them looking for a
  reason to reopen; each was checked for *truth* in earlier visits and each passed.

**Nothing false was found. A clean pass is the report.**

---

## Closing note: the arc's central finding, in its strongest form

Worth carrying to `merge_gate`, and it belongs to the worker rather than to either
reviewer. The sentence repaired this round is the one place in the arc where *"do
not state the count"* was **the sentence's own stated subject** — it existed to
explain that line numbers had been replaced with symbols because numbers rot — and
a count was stated in it anyway. As the worker put it, that *"removes the last
available explanation, since it can't be attributed to not knowing the rule."*

Every weaker explanation had already been eliminated over five rounds: it is not
carelessness (it caught the author, both reviewers and the driver), not
inexperience (it recurred inside the documents analysing it), and not a failure to
adopt the remedy (the remedy was adopted, written down, and then violated in the
sentence adopting it). What remains is the mechanism §J named: **restating an
established fact in new words is the act that manufactures a new false one**, and
the only defence that worked in this arc was a different reader than the writer,
plus — where it was possible — making the code assert what the prose claims.

I add one datum of my own from visit 5, in the same spirit: my draft of that
review miscounted the fenced transcript blocks as four by reading fence markers
rather than pairs, in the same document reporting N1. The rule was known. It did
not help.

---

## Recommendation

**Merge-ready. Clean pass.** The five confirmations hold independently, the
repaired sentence is true and cardinality-free, the master transcript is intact,
the standing invariants are unchanged, and the gates reconcile serialised.

No open correctness items. Nothing to carry to `merge_gate` from this node except
the closing note above, which is a process finding rather than a defect.
