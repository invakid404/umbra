# review-correctness — `dg-29vwer0f` / #121, round 4

**Clean on the question the round exists for. No fourth instance of the errno-less
class.** I walked the axis myself, across all five files on the `getattrlistbulk` path
and in all three crates, rather than accepting the table: every tracee-supplied value
now terminates in an errno the supervisor can bind, and every remaining errno-less exit
judges umbra's *own* output. Two LOW notes follow, and **neither is an instance of the
class** — I am stating that explicitly because of what a fourth instance would trigger.

**The deliberate divergence is acceptable**, and I can give three measured grounds
rather than an opinion — including that it is **unreachable by `/bin/ls` and every
fts-based utility**, which no round has established before.

**Round-3's two findings are closed.** I did not re-run your matrices; I ran a different
and harder set — 15 *conjunctions* of two bad arguments each — and got **zero
run-enders**.

**Baseline:** `psvqmvlm` / `3480713b`, rebuilt here (`--workspace --bins` plus
`umbra-storage-nfs-userspace --features transport-raw --bins`), against the built
`a85a8471`. Live Ganesha on `127.0.0.1:12105`.

---

## 0. Summary

| # | severity | status | finding |
|---|---|---|---|
| **N4‑1** | low | CONFIRMED (table completeness) | The table enumerates the **request**'s six values, and six is right and complete. A **seventh tracee-*influenced* input** reaches the same encoder and is not on it: the directory's own entries, which a tracee creates through routed writes, terminating in `encoder_fault` — errno-less, run-ending. **Measured unreachable**, so not an instance of the class; but the table's rule is scoped to "a value in this table", so it is outside the rule's coverage. |
| **N4‑2** | low | CONFIRMED (consistency) — **recommend NOT fixing** | Under conjunctions of two bad arguments, umbra's errno *precedence* differs from the kernel's on 7 of 15 shapes (`filefd-zerocap`: kernel `ENOTDIR`, umbra `EINVAL`). Same shape N7 was fixed under, one level down — descriptor *kind* rather than *existence*. Every shape answers a real errno and the run survives. My recommendation is on the record below. |

**No MEDIUM or HIGH. Nothing that ends a run. Nothing silent.**

---

## Priority 4 first, because it is the one that decides the round — **the class is closed**

I did not take the table's word for it. I enumerated every error construction on the
`getattrlistbulk` path in every file it touches, and for each asked whether a
tracee-supplied value can reach it.

| file | errno-carrying (tracee-answerable) | errno-less (fatal) | errno-less reachable by a tracee? |
|---|---|---|---|
| `abi.rs` — `decode_entry`(461), `directory_bytes`, `io_buffer`, `directory_request` | `EINVAL`(22) zero buffer; `EFAULT`(14) null/overflow buffer; `EFAULT`(14) null/overflow attrlist; `ENOTSUP`(45) options | none | — |
| `events.rs` — `io_binding`, the `ReadDir` arm, `unserved_directory_request` | every errno unwrapped to a value | 4 of 7 exits: no block, broken register read, wrong block width, faulting read | **No.** First three are provider/wiring faults; the fourth is [#126], pre-existing and tracked |
| `engine.rs` — `resolve_directory` | `Deny(EBADF)`, `Deny(ENOENT)`, `Deny(ENOTDIR)`, `Deny(EINVAL)`, `Deny(<encoder errno>)` | missing-encoder `unsupported`; `lookup`; `merged`; the `memory_writes` overflow fold; the `consumed`/`total` validation | **No.** All five judge umbra's own wiring or the encoder's own output |
| `directory.rs` — `AbiDirectoryEncoder` | propagates the encoder's | 4: poisoned mutex; non-`ReadDir` operation; `buffer.length != max_bytes`; `counted != consumed`; plus `take()`'s no-binding | **No.** All internal. The no-binding one is the fix report's *inferred* item — see below, now confirmed |
| `dirents.rs` | `ENOTSUP`(45) attrlist contents; **`ERANGE`(34) capacity** | 4 `encoder_fault` sites: `record_bytes`'s one-component check, 3 in `count_records` | **No** — the three `count_records` sites walk the encoder's own bytes; `record_bytes` is N4‑1 and is measured unreachable |

**Three things I verified rather than reasoned:**

1. **`resolve_directory`'s exits, in order.** `routed_binding` → `lookup` + kind check
   (`Deny(ENOTDIR)`) → bound check (`Deny(EINVAL)`) → cache/`merged` → encode → errno
   ⇒ `Deny`, errno-less ⇒ fatal → overflow fold → the `consumed`/`total` validation.
   Every tracee value exits as a `Deny`; every errno-less exit is downstream of the
   encoder and judges it. The `consumed > entries.len()` guard still precedes the
   `entries[consumed..]` slice, so the slice cannot panic.
2. **The rule "inside `umbra-overlay` the errno must become `Deny` before leaving
   `resolve`" is true of every path, not aspirational.** The one seam that could have
   broken it — an errno-carrying error arriving from `dirents` — is converted at
   `engine.rs` line 103 before `self.planned` is set, so no `Plan` exists and nothing is
   journalled. The only errno that can arrive there is `too_small`'s 34; `record_bytes`
   and `count_records` carry none, so the `None => Err(e)` arm keeps encoder self-
   contradiction fatal.
3. **The fix report's one *inferred* item is now confirmed by inspection.** It argued the
   encoder's "reached the encoder with no output buffer bound" check is unreachable
   because the N7 guard's condition equals `routed_binding`'s denial condition.
   `routed_binding` denies `EBADF` when `!fds.contains_key(fd)` **and** when the state
   carries no `logical_path`. The guard fires only on the first, so the guard's condition
   is a strict *subset* of the denial condition — the inference holds, and it holds for
   the stronger reason than the one given. In the second case the fd *is* bound, the
   guard does not fire, the buffer is bound, `routed_binding` still denies, and the
   binding is cleared or overwritten by the next operation. No path reaches the encoder
   unbound.

**Independent empirical check — 15 conjunctions of two bad arguments, one run each,
routed. Zero run-enders.** This is the shape no previous round tested: every earlier
matrix varied one input at a time, and "one edit elsewhere exposes a check nobody could
reach" (round 3's N3‑2) is a *combination* defect.

```
filefd  filefd-zerocap  filefd-smallcap  filefd-wide  filefd-nullal
unbound-zerocap  unbound-wide-zerocap  smallcap-wide  zerocap-wide  zerocap-nullal
smallcap-nullal  nullbuf-smallcap  nullbuf-zerocap  opts0-zerocap  opts0-smallcap
-> 15/15 answered the tracee a real errno; 15/15 runs finished
```

Plus round 2's 20-shape request matrix re-run: **0 run-enders**, every shape answered.

---

## N4‑1 — the table's six are the right six, and one tracee-influenced input is off it

**Is any tracee-controlled value missing from the six?** Of the **request**, no. I
checked which register slots the path actually reads: `decode_entry`(461) reads x0 and
x3; `io_buffer` x2 and x3; `directory_request` x1 and x4; `directory_bytes` x3. Nothing
reads x5 or beyond, so the five syscall arguments plus the attrlist's memory contents are
exactly six values and the enumeration is complete for the request. The split of
`attrlist` into pointer and contents is the right split, because they terminate in
different places (`EFAULT`/#126 versus `ENOTSUP`).

**But the table's rule is scoped narrower than the encoder it protects.** The rule reads
"a value in *this table* may only be refused with an errno attached". A tracee also
controls, indirectly but completely, the **entries** — their names, kinds, link counts
and number — by creating files through routed writes. Those reach the same
`dirents::encode`, and `record_bytes` refuses three of them with `encoder_fault`:

```rust
if name.contains(&b'/') || name == b"." || name == b".." {
    return Err(encoder_fault("directory entry name is not one path component"));
}
```

errno-less, run-ending, and not on the table.

**Measured unreachable, which is why this is a note and not the class's fourth
instance:**

- The filesystem refuses all three names outright — I tried: `'.'` → *File exists*,
  `'..'` → *File exists*, `'a/b'` → *No such file or directory*.
- `Overlay::merged` applies `validate_name` to **both** halves (base at `engine.rs:2298`,
  shadow at `:2330`), and `validate_name` rejects exactly that set, so even a
  misbehaving storage backend cannot get one through.
- The only umbra-internal names that could look like entries — whiteout markers — live
  under `StorageAnchor::Control` with a `whiteouts/` prefix and hex-encoded components,
  a different anchor from the run root, so no marker can appear in a root-anchored
  listing.

So there is no live defect. What there is: the guard keeping this inert lives in
`umbra-overlay`, the check it protects lives in `umbra-platform`, and the table that
now encodes the rule mentions neither. That is the same crate-boundary blindness the
table was written to fix — one row ("the entries, from `Overlay::merged`, filtered by
`validate_name` in both halves; `encoder_fault` if that filter ever changes") would
close it, and the row is worth more than its size because the table is now the artefact
the next person trusts.

---

## N4‑2 — errno precedence under conjunctions, and why I recommend leaving it

Measured, host beside routed, 15 shapes. 8 agree. The 7 that differ:

| shape | kernel | umbra |
|---|---|---|
| `filefd-zerocap` | `ENOTDIR`(20) | `EINVAL`(22) |
| `filefd-wide` | `ENOTDIR`(20) | `ENOTSUP`(45) |
| `filefd-nullal` | `ENOTDIR`(20) | `EFAULT`(14) |
| `smallcap-wide` | `ERANGE`(34) | `ENOTSUP`(45) |
| `zerocap-nullal` | `EFAULT`(14) | `EINVAL`(22) |
| `nullbuf-smallcap` | `ERANGE`(34) | `EFAULT`(14) |
| `opts0-smallcap` | `ERANGE`(34) | `ENOTSUP`(45) |

The mechanism is layering: the supervisor's argument checks (`io_buffer`,
`directory_request`, the attrlist read) all run at the syscall entry, while the
descriptor-*kind* check is inside `resolve`. So when both an argument and the
descriptor's kind are wrong, umbra answers about the argument. The kernel answers about
the descriptor first.

This is recognisably the standard N7 was fixed under — "umbra diverging from the kernel
on a descriptor whose validity it has not checked" — one level further down: N7
established descriptor *existence* precedence, not descriptor *kind*. So I am naming it
rather than staying silent.

**I recommend not fixing it, and I want the reasoning on the record so the next round
does not reopen it:**

- Every one of the 15 answers a real errno and the run survives. Nothing is silent and
  nothing is fatal.
- POSIX specifies no precedence among simultaneous error conditions, and
  `getattrlistbulk(2)` lists its errors unordered. A program cannot depend on which of
  two it receives; it must handle the one it gets.
- The **single-fault** case — `filefd` alone, the only one a real program reaches —
  matches the kernel exactly (`ENOTDIR` both sides). Every divergence needs two
  simultaneous program bugs.
- Closing it means moving the supervisor's argument checks behind the engine's kind
  check, i.e. reordering the exact sequence rounds 3 and 4 have just stabilised, and
  round 3's N3‑2 is the evidence that reordering this sequence is how a dead check
  becomes live. The cost is the risk that was just paid down; the benefit is an errno
  nobody can observe without writing two bugs at once.

---

## Priority 2 — ruling on the deliberate divergence: **acceptable**, on three measured grounds

At `withlong/` with `cap` in the band between the smallest and largest record, the kernel
serves and umbra answers `ERANGE`.

**1. It is unreachable by everything in claimed scope.** A path component is capped at
255 bytes — measured: 255 creates, 256 and 300 are refused *File name too long*. So the
largest possible single record is `48 + 255 + 1 = 304` bytes. `fts` issues
`getattrlistbulk` with a **32 768-byte** buffer (measured, round 1), which fits at least
107 records of the worst case. **`consumed == 0` is arithmetically unreachable for
`/bin/ls` and every fts-based utility.** The divergence cannot be reached by any program
in the claimed surface; it needs a caller that chooses a buffer under ~304 bytes.

**2. It is benign for a conforming consumer, measured.** I wrote the consumer the
question is about — start at 32 bytes, double on `ERANGE`, drain to EOF:

```
HOST   short    : total=3 calls=5 grows=1 final_cap=64
ROUTED short    : total=3 calls=5 grows=1 final_cap=64      <- identical
HOST   withlong : total=4 calls=8 grows=3 final_cap=256
ROUTED withlong : total=4 calls=6 grows=3 final_cap=256     <- same entries, same grows, same final buffer
```

Same entry count, same number of grows, same converged capacity. Only the call count
differs (6 vs 8), because umbra's sorted order packs the long record first and needs
fewer partial pages — and a syscall's *call count* is not a contract. It terminates,
because `ERANGE` is bounded by the largest record.

**3. umbra returns `ERANGE` on a strict superset of the capacities where the kernel
does.** The kernel itself answers `ERANGE` for `cap < 56` on `short/` and `cap < 64` on
`withlong/`. So a consumer that handles the kernel's `ERANGE` handles umbra's, and one
that treats `ERANGE` as fatal is already broken against the kernel. The only consumer
that could be correct against the kernel and broken against umbra is one that depends on
the kernel's **enumeration order** to decide which entries arrive first — and no
directory API guarantees an order.

And the alternative is worse in the dimension this project cares about most: serving
what fits would mean emitting out of byte-sorted order, which is what makes
`directory_next`'s paging deterministic across calls. A later page would then repeat or
skip entries — a silent wrong answer traded for a loud errno. **The fix report's
reasoning is right and I endorse it.** What I would add is ground 1, which is stronger
than the report claims for itself: this is not merely a defensible divergence, it is one
no claimed consumer can reach.

---

## Priority 3 — shadowing audit: **nothing newly exposed; one check deliberately dead**

This pass moved a check, reordered two, and converted an `Err` to a `Deny` across a crate
boundary, so I looked specifically for the N3‑2 shape.

**Newly reachable — nothing.** Moving `resolve_directory`'s bound check from first
statement to after `routed_binding`/`lookup`/kind-check means those three now run before
it. None is an errno-less exit a tracee can drive: `lookup` and `merged` judge storage,
and `routed_binding` and the kind check both `Deny`. `merged` is still *after* the bound
check, so a zero-bound request cannot provoke a directory listing's worth of NFS round
trips before being denied.

**Newly dead — one, deliberately.** `resolve_directory`'s `max_bytes == 0` →
`Deny(EINVAL)` is now unreachable in production, from both directions: for a **bound**
descriptor `io_buffer`'s empty-buffer `EINVAL` fires at the supervisor and `resolve` is
never called; for an **unbound** one `routed_binding`'s `EBADF` now precedes it. Its only
exerciser is the new engine test. That is the right outcome and the report says why —
relying on ordering is the bet that failed in N3‑2 — so the check is defence in depth,
not dead code to remove. The upper half (`> MAX_IO_BYTES`) is likewise unreachable behind
`directory_bytes`' clamp and is documented as asserting its own unreachability.

**The `Err` → `Deny` conversion cannot over-convert.** Only `too_small`'s `ERANGE` can
arrive there; `record_bytes` and `count_records` carry no errno and
`AbiDirectoryEncoder`'s four internal failures carry none, so encoder
self-contradiction stays fatal. I checked each rather than assuming.

---

## Priority 5 — regressions: none

| check | result |
|---|---|
| `userspace_run` vs live Ganesha | **22 passed, 0 failed** |
| `run_fixtures` rewrite-backed matrix | **10 passed**, incl. `/bin/ls -l`, `/bin/ls -t` |
| side-by-side vs `a85a8471`, `--local-dev`, **22 flag shapes** | **0 divergences** |
| round‑2 request matrix, **20 shapes** routed | **0 run-enders**, all answered |
| round‑1 partial read → close → reopen, plus the paging positive control | `rc=2` partial; reopen `rc=5` from the **head**; `2/2/1/0` advancing, no gap or repeat |
| round‑1 `fts_children` fixture (the H2 exhibit) | `counted 5 entries, 707 bytes` = host |
| pure recursive `fts` descent | `count=6 dirs=3 errno=0` = host |
| `readdir` probe, mutated / **negative control** | passes, no `SKIP` / **fails on the names** |

---

## Priority 6 — standing items (1)–(7)

1. **Does `ls` route through the userspace client?** **Yes, unchanged.** Probe re-run on
   this build: mutated → `ok`, no `SKIP`, 4.18 s of real fixture work; unmutated with the
   probe still selected → **FAILED** at `userspace_run.rs:1978`, on the *names*.
2. **fd fence + virtual-cwd discipline.** **Preserved.** All six `fd0-*` shapes (a kernel
   descriptor below the fence) still pass through to the kernel and get its own answer;
   all unbound shapes answer `EBADF`; 22-shape side-by-side has no divergence. Nothing
   this pass touched the cwd path; the recursive descent still resolves correct full
   paths.
3. **#55–#120 mechanisms.** **Intact.** One `TRACED_STUBS` with `Delivery`
   exhaustiveness; `intercept()` still derives from it; the fence and its C/Rust twin
   unchanged; `replay_must_poison` untouched; `Err`-vs-`Emulate` at the routed-open
   refusals unchanged; `dirents` now has exactly two refusal constructors for its two
   tracee inputs plus one for its own faults.
4. **Silent-failure paths.** **Nothing silent found.** Both LOW notes are loud by
   construction; N4‑1 is not reachable at all. The dirfd-close path and its partial
   variant both still behave. fork/exec cwd: nothing propagates, `FsOp::Chdir`/`GetCwd`
   still undecoded.
5. **Doc-comment false invariants.** **None this round** — the first round of four where
   I can say that. N3‑3's overreaching sentence is corrected and correctly scoped
   ("attribute list", not "request"), with the two downstream checks named and a pointer
   to the input table. The input table's own claims I checked one by one and they hold;
   its only gap is N4‑1, which is a missing row rather than a false statement.
6. **Parallel-admission-list class.** **Nothing.** One `TRACED_STUBS`, one
   `DARWIN_ARM64_ABI` with two production readers, one attrlist decode, `abi.rs` reading
   `dirents`' constants; both optional-method `None`s on a `ReadDir` are now
   `ProtocolMismatch`.
7. **Could green CI still be skipping proofs?** **The round-3 gap is closed and I found
   no new one.** The four new tests run in the default `cargo test --workspace` and all
   four discriminate against the pre-fix code. I checked the two sweeps for the
   `is_err()`-shaped hole specifically: `every_output_bound_refusal_carries_a_bindable_errno`
   asserts `Errno(34)` per capacity **and** `assert_eq!(refusals, one)` — an exact count,
   not a floor, so it can neither pass vacuously nor pass if any capacity below one
   record stops refusing. It also pins the boundary the arm must *not* catch (an empty
   directory encodes at capacity 0). The sort-order test asserts its premise
   (`long_name.as_bytes() < b"aaa"`). The engine pair asserts `Deny(Errno(9))` /
   `Deny(Errno(34))` and `planned.is_none()`, not `is_err()`. The uncovered surface I can
   still name is the *conjunction* space I tested by hand here — 15 shapes, all passing,
   none of them in any suite — but that is a breadth observation, not a gap in a proof
   that exists.

---

## On the budget, stated plainly

**I found no fourth instance of the errno-less class, and I looked for it on its own
axis rather than on the axis the last fix used.** N4‑1 is a missing row in a table
describing an input that is measurably unreachable; N4‑2 is errno ordering under a
conjunction of two program bugs, where every shape already answers correctly and where I
recommend *no change*. Neither is a case of a tracee-supplied value reaching an `Err`
with no errno — which is the pattern whose recurrence was the thing to watch — and I
would not want either mistaken for one.

If it helps the disposition: the two notes together are the smallest findings of the four
rounds by a wide margin, both are optional, and the mechanism questions that drove rounds
1–3 (does `ls` really route; is the fence intact; is the class closed; do the proofs
discriminate) are all now answered affirmatively by measurement I took myself. I have no
reservation about this going to publish.

**What I would carry forward as the durable lesson**, since it is the thing that actually
converged this: rounds 1–3 each enumerated a *structure* (constructors, then one
function's exits, then inputs) and each structure was bounded by a crate. The input
enumeration worked because inputs are the only one of the three that follows the data
across crate boundaries. That is worth writing down somewhere more permanent than a fix
report.

---

## Gates re-run here

| gate | result |
|---|---|
| `cargo build --workspace --bins`, `+ --features transport-raw --bins` | clean |
| `userspace_run` / `run_fixtures` | 22 / 10, 0 failed |
| side-by-side vs `a85a8471`, 22 flag shapes | 0 divergences |
| 15 two-fault conjunctions, routed, one run each | **0 run-enders**, 15 answered |
| round-2 request matrix, 20 shapes | **0 run-enders** |
| growing paging consumer, host vs routed, 2 directories | same entries, grows and final capacity |
| `readdir` probe / negative control | passes / fails on the names |

`fmt`/`clippy`/`cargo test --workspace` not re-run; you reported them clean (832) and
nothing here is a compile or lint issue.
