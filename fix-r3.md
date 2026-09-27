# fix-r3 — `dg-29vwer0f` / #121, round 3

**Input:** `review-synthesis-r3.md` (3 findings), `review-correctness.md` round 3
(N3‑1 … N3‑3), `review-scope.md` round 3 (**clean, no findings**).

**The instruction was not "fix these two" and I did not treat it as such.** It
was: *enumerate the tracee-supplied inputs, not the constructors and not the
exits.* That enumeration is §1, it is written into the code above
`abi.rs`'s `directory_bytes`, and it is what found the part of N3‑1 the review
did not reach — attaching `ERANGE` to the error is **not sufficient**, because
the supervisor's `resolve` arm is `Err(e) => return Err(e)` and never unwraps an
errno from a resolution failure. Without also converting it to a `Deny` inside
the engine, the fix would have measured as no fix at all.

**Outcome:** both MEDIUMs closed and exhibited before/after. The capacity matrix
went from **9 of 15 shapes ending the run** to **zero**. All eight
unbound-descriptor shapes now answer `EBADF`, matching the kernel. Round 2's
ten-shape matrix is unregressed.

---

## 1. The input enumeration

A `getattrlistbulk(fd, attrlist, buffer, size, options)` hands umbra **six**
tracee-controlled values — not two. The synthesis said two (the attribute list
and the buffer size); that is the count of *contents*, and it omits the
descriptor, the two pointers and the options word. All six are walked here, each
to every path it reaches and the disposition it terminates in. The table is
committed above `directory_bytes` so it lives beside the input it was missing.

| # | input | where it goes | terminates in |
|---|---|---|---|
| 1 | `fd` (x0) | supervisor descriptor floor → `Overlay::routed_binding` → directory-kind check | resumed to the kernel below the floor; `Deny(EBADF)` unbound or pathless; `Deny(ENOENT)` if the name stopped resolving; `Deny(ENOTDIR)` on a non-directory |
| 2 | `attrlist` **pointer** (x1) | `directory_request`'s null/overflow guard → the supervisor's read of the block | `EFAULT` null/overflow; unmapped-but-plausible faults the read → **[#126]**, the pre-existing tree-wide class shared with `fstat`/`open`/`read` |
| 3 | `attrlist` **contents** | `RequestedAttributes::decode` | `ENOTSUP`, swept by `every_attribute_request_refusal_carries_a_bindable_errno`; `reserved` is a measured don't-care and refuses nothing |
| 4 | `buffer` **pointer** (x2) | `io_buffer`'s null/overflow guard → the write of the reply | `EFAULT` null/overflow; unmapped → **[#126]** |
| 5 | **`size` (x3)** | `directory_bytes` clamps → `io_buffer` refuses zero → `resolve_directory`'s bound check → `dirents::encode`'s capacity arm | **the descriptor is decided first**, so a zero size on a descriptor umbra never issued is **`EBADF`**, not `EINVAL` — measured both ways against the kernel. `EINVAL` is the answer only once the descriptor has validated, from whichever of the two zero checks reaches it first. **`ERANGE`** when no whole record fits. "Too large" is unrepresentable because of the clamp |
| 6 | `options` (x4) | `directory_request` | `ENOTSUP` |

**Rows 1–4 and 6 were already sound** — I traced each rather than assuming the
earlier rounds had. **Row 5 was the whole of this round**: it reaches four checks
and *two* of them held errno-less `Err`s. That is why enumerating constructors
(round 2) and exits-of-one-function (round 2) both missed it — neither axis
crosses a crate boundary, and row 5's two holes are in `umbra-overlay` and
`umbra-platform`, downstream of the function whose exits were listed.

**The rule the table now states, for whoever adds the next check:** a value in
this table may only be refused with an errno attached, **and inside
`umbra-overlay` that errno must become `ResolvedAction::Deny` before it leaves
`resolve`.** An errno-less `Err` is reserved for umbra contradicting itself.

[#126]: https://github.com/invakid404/umbra/issues/126

---

## 2. N3‑1 — the capacity arm

### Measured first, kernel beside it

`ERANGE`(34) confirmed rather than assumed, and the review's premise holds this
time:

```
KERNEL short/    cap=1/8/32/55 -> ERANGE(34);  cap=56 -> rc=1;  140 -> rc=2;  4096 -> rc=3
KERNEL withlong/ cap=1/32      -> ERANGE(34);  cap=64 -> rc=1;  140 -> rc=2;  4096 -> rc=4
```

### The capacity matrix, before and after

Two directories, one capacity per process. `short/` = `aaa bbb ccc`;
`withlong/` = those three plus one 200-character name.

| dir | `max_bytes` | kernel | routed **before** | routed **after** |
|---|---|---|---|---|
| short | 1 | `ERANGE` | **RUN ENDED** | `ERANGE` |
| short | 8 | `ERANGE` | **RUN ENDED** | `ERANGE` |
| short | 32 | `ERANGE` | **RUN ENDED** | `ERANGE` |
| short | 55 | `ERANGE` | **RUN ENDED** | `ERANGE` |
| short | 56 | `rc=1` | `rc=1` | `rc=1` |
| short | 64 / 128 / 140 / 4096 | served | served | served |
| withlong | 1 | `ERANGE` | **RUN ENDED** | `ERANGE` |
| withlong | 32 | `ERANGE` | **RUN ENDED** | `ERANGE` |
| withlong | 64 | `rc=1` | **RUN ENDED** | `ERANGE` |
| withlong | 140 | `rc=2` | **RUN ENDED** | `ERANGE` |
| withlong | 256 | `rc=3` | `rc=1` | `rc=1` |
| withlong | 4096 | `rc=4` | `rc=4` | `rc=4` |

**Nine run-enders to zero.** `short/` now matches the kernel exactly across every
capacity.

### The two rows where umbra still differs from the kernel, and why that is right

`withlong/` at 64 and 140: the kernel **serves**, umbra answers `ERANGE`. That is
the sort-order difference and it is inherent, not a residual defect.
`Overlay::merged` returns entries byte-sorted — `"LLL…"` (0x4C) before `"aaa"`
(0x61) — so the 256-byte record is packed first and nothing fits. The kernel's
enumeration order happened to put the short names first.

I considered and rejected matching the kernel by skipping the oversized entry and
serving what fits: byte-sorted order is what makes `directory_next`'s paging
deterministic across calls, and serving out of order would make a later page
repeat or skip entries — a silent wrong answer traded for a loud errno. So the
difference stays, and it is now **documented at the mechanism** rather than left
for the next reader to re-derive, in `encode`'s doc and in a test that asserts
the premise (`long_name.as_bytes() < b"aaa"`) rather than assuming it.

### The fix, and the half the review did not reach

1. `dirents::encode`'s inline errno-less `UmbraError::new` became `too_small()`,
   a named constructor carrying `Errno(34)` — the sibling of `unserved()`, so the
   module's two refusals are now exactly its two tracee-supplied inputs and both
   end in a bindable errno.
2. **`resolve_directory` converts an errno-carrying encoder error into
   `ResolvedAction::Deny`.** This is the step attaching the errno does not
   accomplish on its own: the supervisor's arm is `Err(e) => return Err(e)`, so
   an errno on a resolve failure is inert. Encoder errors *without* an errno —
   no bound buffer, a length disagreeing with the operation, a re-walk finding a
   different count — are umbra contradicting itself and still stop the run. The
   conversion happens before `self.planned` is set, so no `Plan` exists and
   nothing is journalled.

---

## 3. N3‑2 — the shadowing regression from my own N7 fix

### Measured, with the kernel's precedence

```
HOST  unbound fd 4500 + cap=0     -> EBADF(9)     <- the kernel decides the DESCRIPTOR first
HOST  unbound fd 4500 + cap=4096  -> EBADF(9)
HOST  valid fd        + cap=0     -> EINVAL(22)   <- only then the argument
ROUTED unbound + cap=0 (before)   -> RUN ENDED: "invalid directory output bound"
```

### The fix: the kernel's order, and the check hardened rather than relied upon

`resolve_directory`'s bound check was its **first** statement and an errno-less
`Err`. It is now **after** `routed_binding` and the directory-kind check, and it
answers `Deny(EINVAL)`.

Both halves matter and I want to be explicit that the ordering alone would have
been enough to close the finding. I hardened the check anyway, because the
finding *is* that a check nobody could reach became reachable by one edit
elsewhere. Relying on `routed_binding` to get there first is the same bet that
`io_buffer`'s `EINVAL` would always get there first — which is the bet that
failed. The upper half (`> MAX_IO_BYTES`) stays as an assertion of its own
unreachability, with the clamp named.

### The shadowing audit the synthesis asked for

*What else was that guard shadowing?* The N7 guard skips `io_binding`,
`directory.set` and `unserved_directory_request`. Working through what each
covered:

| shadowed by the guard | errno-less downstream check it exposed | status |
|---|---|---|
| `io_buffer`'s empty-buffer `EINVAL` | `resolve_directory`'s bound check | **the finding; fixed** |
| `io_buffer`'s null/overflow `EFAULT` | nothing new — the pointer is only used by the encoder's write, which is not reached (below) | clean |
| `directory.set(buffer)` | `AbiDirectoryEncoder`'s "reached the encoder with no output buffer bound" — errno-less, **would end the run** | **unreachable, by implication rather than by equality** — see the correction below |
| `unserved_directory_request` | the attribute checks — all carry errnos | clean |

**Correction to the third row, made in the CI/CR pass.** It originally said the
guard "fires exactly when `!context.fds.contains_key(fd)`, which is precisely
`routed_binding`'s `Deny(EBADF)` condition". That equality is false, and
CodeRabbit was right to flag it: `routed_binding` denies on **three** conditions
— no binding, a binding with no `logical_path`, and a name that no longer
resolves — where the guard tests only the first.

The unreachability argument survives, because it needs the implication and not
the equality: the guard fires **only if** there is no binding, and no binding
**implies** `routed_binding` denies. Guard ⊆ denial is the direction that
matters, and it is the direction that holds. A bound descriptor with no
`logical_path` simply does not take the guard's path at all — it goes through
`io_binding` normally and is denied `EBADF` inside `resolve`, which is the same
answer by the ordinary route.

The third row is the one worth naming: it is a second errno-less check the same
guard exposed, and it is inert only because the descriptor denial now precedes
it. Confirmed empirically as well as structurally — `unbound-served` at
cap=4096 answers `EBADF`, and it would have reached the encoder otherwise.

### After

All eight unbound shapes, against a kernel that answers `EBADF` for every one:

```
unbound-zerocap (cap=0)  errno=9    <- was a RUN ENDER
unbound-served  (cap=4096) errno=9
unbound-served / wide / options0 / bmc3 / reserved / nullal   all errno=9
```

And the bound-descriptor zero-buffer case still answers `EINVAL`(22), which is
the kernel's answer for that shape.

---

## 4. N3‑3 — the sentence that overreached

Corrected rather than deleted, and scoped to what is true: the seven exits are
unreachable by a tracee's choice of **attribute list**, not of **request**. The
correction says why the distinction matters — a request outlives the function,
and the buffer size is the part that does — names the two downstream checks that
proved it, and points at the input table as the other half. Exits there, inputs
here.

---

## 5. Tests — the gap the review named as the one still open

Round 3's item (7) said nothing exercised a too-small output buffer or a
zero-length buffer on an unbound descriptor. Four new `#[test] fn`s:

| test | pins |
|---|---|
| `every_output_bound_refusal_carries_a_bindable_errno` | sweeps **every** capacity below one record and asserts `Errno(34)` on each — the errno, not `is_err()`, which the run-ending form satisfies. Counts the refusals so it cannot pass vacuously |
| `a_long_name_that_sorts_early_refuses_a_capacity_a_short_directory_serves` | the data-dependency itself: 140 serves `short`, refuses `with_long`, and asserts the byte-order premise rather than assuming it |
| `a_directory_read_decides_the_descriptor_before_the_output_bound_and_answers_both` | unbound + `max_bytes: 0` → `Deny(EBADF)`; bound + `max_bytes: 0` → `Deny(EINVAL)`. The ordering **and** the disposition |
| `an_encoder_refusal_is_answered_when_it_carries_an_errno_and_fatal_when_it_does_not` | the seam that makes `ERANGE` reach a program at all, both directions, plus that neither leaves a `Plan` to journal |

All four fail against the pre-fix code: the two sweeps on the errno assertion,
the engine pair on `Deny` versus `Err`.

---

## 6. Measured versus inferred

**Measured this pass:** the kernel's answer across nine capacities on two
directories; the routed capacity matrix before and after, one capacity per
process, 15 shapes; the kernel's descriptor-before-argument precedence
(unbound+zero → `EBADF`, bound+zero → `EINVAL`); all eight unbound shapes after
the fix; round 2's ten-shape request matrix, unregressed; 25 flag shapes
side-by-side against `a85a8471` (0 divergences); `userspace_run` 22/22;
`run_fixtures` 10/10; the probe and its negative control; all three gates.

**Inferred, not measured:** that the encoder's "no output buffer bound" check is
unreachable — I argued it structurally (the guard's condition is exactly
`routed_binding`'s denial condition) and corroborated it with the
`unbound-served` run, but I did not construct a case that reaches the encoder
with no binding to confirm it cannot happen; that rows 1–4 and 6 of the input
table are complete — I traced each to its disposition, but only row 5 was
re-measured end to end this round, the others resting on rounds 1–2's matrices.

**Rebuttal, with measurement:** the synthesis says a `getattrlistbulk` request
has "exactly two tracee-supplied values". It has six. The other four were
already sound, so the conclusion was right, but the count is what bounds the
enumeration — and an enumeration bounded at two would have stopped before the
descriptor, the two pointers and the options word. I enumerated six.

**Not fixed, with reason:** the two `withlong/` rows where umbra answers `ERANGE`
and the kernel serves (§2 — inherent to byte-sorted paging; matching the kernel
would trade a loud errno for a silent wrong answer); [#126] and
[#127](https://github.com/invakid404/umbra/issues/127), both pre-existing and
tracked; N8 from round 2, still recorded rather than fixed and confirmed sound by
round 3's own ruling.

---

## 7. Gates

**Figures below are at head `3480713b`** — the commit this pass produced, which
review round 4 and then CI round 1 saw. `impl.md` §7.1 is the canonical per-commit table; 832 still holds at the current head, but it was measured here **without** `UMBRA_TEST_FIXTURE_PATH`, so the eleven `umbra-platform-macos` fixture cases reported `ok` without executing; `ci-fix-r1.md` §2 is that discovery.

| gate | result, at `3480713b` |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace --all-targets` | **832 passed**, 3 ignored (`20ccf7db`: 829; `7bd1a95c`: 826; master `a85a8471`: 813) |
| `userspace_run` vs live Ganesha | **22 passed, 0 failed** |
| `run_fixtures` rewrite-backed matrix | **10 passed**, incl. `/bin/ls -l`, `/bin/ls -t` |
| `readdir` probe, mutated / **negative control** | passes / **fails on the names** |
| side-by-side vs `a85a8471`, 25 flag shapes | **0 divergences** |
| **capacity matrix, routed, 15 shapes** | **0 run-enders** (was 9) |
| **unbound-descriptor shapes, 8** | **all `EBADF`** (was 7 answered, 1 run-ender) |
| round-2 ten-shape request matrix | **unchanged, 0 run-enders** |

Net test change: **+3** — four `#[test] fn`s added and **one superseded and
removed**, `a_buffer_too_small_for_one_record_is_refused`, whose single
`is_err()` assertion `every_output_bound_refusal_carries_a_bindable_errno`
replaces with a sweep over every capacity. No existing `#[test]` body was
edited, so `impl.md` §6's three-edit disclosure stays accurate.

**This figure was wrong when first written** ("+4, no test removed") and is the
fourth quantitative self-report in this slice that did not reconcile — after the
two test-edit undercounts and the 822/813 baseline. CodeRabbit, the scope review
and the driver each caught it independently. The count is now quoted from the
command rather than derived from memory of what the pass did:

```
$ grep -rn '#\[test\]' crates/ --include='*.rs' | wc -l
908          # this change
887          # master@a85a8471
```

905 before this pass, 908 after. The lesson `impl.md` §6 already records for
test-*edit* counts — *"the sweep that finds these is the check to run rather
than this paragraph"* — applies to test-*delta* counts identically, and §6 now
says so.

**Still standing:** PAUSE BEFORE MERGING. Nothing merged, nothing pushed, one
amended change with the trailers exact.
