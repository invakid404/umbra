# fix-r2 — `dg-29vwer0f` / #121, round 2

**Input:** `review-synthesis-r2.md` (9 findings), `review-correctness.md` round 2
(R1–R6), `review-scope.md` round 2 (R1–R5). Round 1's three HIGH defects were
confirmed closed by the reviewers' own measurement; scope clean for a second
round.

**Outcome:** the one new MEDIUM (N1) is fixed, and **the ten-shape request matrix
now answers every shape to the tracee — zero run-enders, against ten before.**
N2 is fixed in both halves, the second of which the new test found rather than
the review. N3's two doc claims corrected and the pre-existing class it names
filed as [#126]. N4–N6 markdown. N7 fixed rather than recorded. N8 recorded
rather than fixed, with the reasoning in the code.

**The synthesis's instruction was the right one and it paid twice.** "Do not just
fix the header arm — enumerate every exit." Enumerating them turned up a second
fail-open exit in `unserved_directory_request` that neither review named: an ABI
returning `Ok(None)` explicitly still skipped validation, even after the default
was made to refuse. The test written to walk the exits is what caught it.

[#126]: https://github.com/invakid404/umbra/issues/126

---

## N1 — the relocated refusal's second exit

### Reproduced first

Ten request shapes, **one per process** so a run-ending refusal cannot hide the
others, on an owned virtual dirfd against the live fixture. Kernel column
measured on the host with the same binary.

| shape | kernel | routed, **before** | routed, **after** |
|---|---|---|---|
| served set | `rc=2 errno=0` | `rc=3 errno=0` | `rc=3 errno=0` |
| `common=0x82079e0b file=0x22d` (`ls -l`) | `rc=2 errno=0` | `errno=45` | `errno=45` |
| `forkattr=1` | `errno=34` | `errno=45` | `errno=45` |
| `volattr=1` | `errno=22` | `errno=45` | `errno=45` |
| `dirattr=1` | `rc=2 errno=0` | `errno=45` | `errno=45` |
| `options=0` | `rc=2 errno=0` | `errno=45` | `errno=45` |
| `attrlist=NULL` | `errno=14` | `errno=14` | `errno=14` |
| zero-length output buffer | `errno=22` | `errno=22` | `errno=22` |
| **`reserved=0x1234`** | **`rc=2 errno=0`** | **RUN ENDED** | **`rc=3 errno=0`** |
| **`bitmapcount=3`** | **`rc=2 errno=0`** | **RUN ENDED** | **`errno=45`** |

(`rc=3` vs the kernel's `rc=2` is the workspace: the routed directory holds three
entries, the host probe directory two.)

### What I measured before choosing the fix

The review offered `EINVAL`(22) or `ENOTSUP`(45) for the header arm. Neither is
"what the kernel gives", and the reason matters — I swept the header fields with
the served bitmaps and compared **reply bytes**, not just return codes:

```
bitmapcount=5 reserved=0x0000 -> rc=2 errno=0  rec0 len=64 bytes=400000000b000082…626574610000000000000000
bitmapcount=5 reserved=0x1234 -> rc=2 errno=0  rec0 len=64 bytes=400000000b000082…626574610000000000000000
bitmapcount=5 reserved=0xffff -> rc=2 errno=0  rec0 len=64 bytes=400000000b000082…626574610000000000000000
bitmapcount=3 reserved=0x0000 -> rc=2 errno=0  rec0 len=64 bytes=400000000b000082…626574610000000000000000
bitmapcount=4 / 6 / 0        -> rc=2 errno=0  rec0 len=64 bytes= identical
```

**The kernel ignores both fields completely** — every shape returns a
byte-identical five-group reply, including `bitmapcount=0` and `6`. So the two
fields are not alike, and I treated them differently:

- **`reserved` is a don't-care and is now ignored.** Its name says so, the kernel
  proves it, and it is the *reachable* case: `struct attrlist` is
  `{u_short bitmapcount; u_int16_t reserved; attrgroup_t commonattr; …}`, so any
  program assigning the fields it cares about on a stack struct without `memset`
  leaves it holding stack garbage. Refusing on a don't-care field was the actual
  bug; giving it an errno would have left umbra refusing a request the kernel
  serves, for a field that does not exist as far as the reply is concerned.
- **`bitmapcount` is request content** — it declares how many of the five
  attrgroup words the caller means — so umbra refuses a value it does not model,
  through the **same constructor** as its sibling: `ENOTSUP`(45), answered to the
  tracee, never fatal. `EINVAL` would assert the request is malformed, and it
  measurably is not.

### The fix, structural rather than per-arm

1. `RequestedAttributes::decode` has **one** refusal constructor now. `unserved()`
   (UnsupportedCapability + `Errno(45)`) is the only way a tracee-supplied request
   can be refused.
2. The errno-less constructor was **renamed** `invalid` → `encoder_fault`, with a
   doc saying it is for a bad `DirectoryEntry` from the overlay or a reply this
   encoder mis-packed — cases with no program to answer, where ending the run is
   correct. Its three remaining callers are all encoder-internal. A future arm
   cannot reach for it by accident without reading what it is for.
3. **A sweep test replaces the prose claim.**
   `every_attribute_request_refusal_carries_a_bindable_errno` walks every field
   the decode reads — all five attrgroup words flipped three ways, plus nine
   `bitmapcount` values — and asserts each refusal carries `Errno(45)`. The old
   test used a bare `is_err()`, which the run-ending shape satisfied, four lines
   below a comment explaining why that is not enough.
   `a_nonzero_reserved_word_is_ignored_exactly_as_the_kernel_ignores_it` pins the
   other half.
4. `unserved_directory_request`'s doc now **enumerates all seven exits** and says
   which may end the run and why, so the next arm added has a list to join.

---

## N2 — the fail-open default, and the second exit the enumeration found

**Decided deliberately: the default now refuses**, matching `encode_stat`'s
reasoning rather than contradicting it. An ABI that services no directory reads
loses nothing — the caller only asks for a `ReadDir`, which such an ABI never
produces — and an ABI that decodes 461 without modelling the request can no
longer reach the encoder silently.

**Then the enumeration found the other half.** I wrote
`every_exit_of_the_directory_request_check_is_answered_or_deliberately_fatal` to
drive all seven exits through the supervisor, and it failed on the exit I had
just *documented* as fatal: `Ok(None)` from an ABI that returns it **explicitly**
still returned `Ok(None)` and skipped validation. Fixing the trait default does
not fix the call site. That exit is now a `ProtocolMismatch`, matching
`io_binding`'s `None` on the same operation — the asymmetry the review named.

This closes the review's "no coverage in either direction": there was no
`SyscallAbi` double in the tree producing `FsOp::ReadDir`, so nothing reached
`unserved_directory_request` at the supervisor level at all. There is one now,
and the distinction it tests is **"answered to the tracee or fatal to the run"**,
which is the one the defect got wrong.

---

## N3 — two doc claims, and the class behind one of them

- `directory_request`'s comment claimed a bad pointer is "answered `EFAULT` here
  rather than faulting the supervisor's read". Corrected: the guard covers null
  and overflow only, and an unmapped but otherwise valid address **does** fault
  and end the run. The comment now says which two shapes it catches and that the
  rest is a pre-existing class.
- The ABI test's "Both refusals carry `ENOTSUP`" — N1's doc twin, inside the test
  meant to pin the property. Rewritten to say the claim *was false when written*,
  name the two tests that now sweep for it, and keep that history rather than
  quietly deleting it.

**Filed, not fixed:** [#126] for the unmapped-pointer class (four surfaces —
`fstat`, `open`, `read`, the two directory buffers — one remedy), and [#127] for
the buffered-stdio-writes-zero-bytes disposition the reviewer also asked be
tracked. Both record that they are pre-existing and that #121 neither introduced
nor changed them.

[#127]: https://github.com/invakid404/umbra/issues/127

---

## N4, N5, N6 — markdown

- **N4.** `impl.md` §6 now names **three** test-body edits and says plainly that
  this count has been wrong twice, that the failure both times was the
  *self-report* rather than the edit, and what sweep finds them. `fix-r1.md` §5's
  "the two" is corrected in place, as a correction rather than an overwrite.
- **N5.** §6's heading paragraph now states that `fix-r1.md` and `fix-r2.md` are
  authoritative where they differ, that §1's three deviations are **four** (the
  new ABI method is the fourth), and that a validation moved across the
  descriptor fence. A human reading `impl.md` alone now learns both.
- **N6.** The README's `-R` row no longer records an open question. It carries
  the actual cause: recursive `fts` re-reads each directory via `fts_children`,
  round 1's truncation was the empty-remainder cache, and the eviction fixed it.
  It also keeps the part that is still true and load-bearing — recursion is not
  claimed, not tested, and not refusable, because when `fts` declines to descend
  it issues no syscall.

**On F4, since I was the one who could not reproduce it:** the reviewer's
round-1 measurement was real and my rebuttal was right for the wrong reason. My
fixture read each directory once; theirs read each twice (`fts_children` plus
`fts_read`'s own build), which is what made it an H2 exhibit. Two independent
"cannot reproduce" results, both correct, both measuring the wrong shape. The
lesson I am taking is the synthesis's: **a negative result from a fixture I wrote
is evidence about my fixture.** The README row now carries the mechanism rather
than the disagreement.

---

## N7, N8 — the judgement calls

**N7 — fixed, not recorded.** An unserved attribute set on a descriptor umbra
never issued answered `ENOTSUP`(45) before `routed_binding`'s `EBADF`(9). The
review called it cosmetic; I fixed it, because "umbra diverging from the kernel
on a descriptor whose validity it has not checked" is H1's shape one level down,
and the fix is a guard clause. A `ReadDir` whose descriptor is not in
`context.fds` now skips the attribute check and lets `resolve` answer — reading
`context.fds` in the supervisor is `allocate_descriptor`'s own idiom, not a new
channel. Measured after:

```
unbound-served    rc=-1 errno=9 (Bad file descriptor)
unbound-wide      rc=-1 errno=9      <- was errno=45
unbound-options0  rc=-1 errno=9      <- was errno=45
unbound-bmc3      rc=-1 errno=9      <- was a run-ender
unbound-reserved  rc=-1 errno=9      <- was a run-ender
```

All five now agree with the kernel and with `fstat`/`fchdir`/`close`.

**N8 — recorded, not fixed**, with the reasoning in the code beside the eviction
rather than only here. `Overlay::directories` keeps entries for a descriptor
abandoned by `exec`. They are **unreachable** rather than stale — `DirectoryKey`
carries `exec_generation`, which the exec advances — so this is accumulation,
bounded by `MAX_DIRECTORY_ENTRIES` per entry and by the run deadline. Evicting
them needs a new overlay hook for an event the engine is not told about, which is
more surface than an unreachable entry is worth. The comment says what would make
it a correctness issue instead.

---

## Measured versus inferred

**Measured this pass:** the ten-shape matrix before and after, one shape per
process, with the kernel's answer for each captured separately on the host; the
header-field sweep comparing **reply bytes** across `reserved` ∈ {0, 0x1234,
0xffff} and `bitmapcount` ∈ {0, 3, 4, 5, 6}; the five unbound-descriptor shapes
before and after N7; 25 flag shapes side-by-side against `a85a8471` (0
divergences); the full `userspace_run` suite; the `readdir` probe and its
negative control; `run_fixtures` including the `-l`/`-t` rows; all three local
gates.

**Inferred, not measured:** that the `bitmapcount` refusal is the right call
rather than serving it as the kernel does — the kernel measurably ignores the
field, so serving would also have been defensible; I chose refusal because
umbra's encoder produces exactly one layout and the declared extent is request
content, and I am recording it as a judgement rather than a measurement; that
N2's default-refusal is unreachable today (the review established this and I did
not re-derive it); that #126's remedy shape is right — I measured the behaviour
across four surfaces but did not prototype the fix.

**Not fixed, with reason:** the unmapped-pointer class (#126) and buffered stdio
(#127), both pre-existing and both out of this slice's scope; N8 above. The
overlay README's "either" after a two→three fix (scope R4) — the reviewer
recorded it as below the bar and I agree, but it is a one-word drift inside a
correction, so I fixed it anyway while I was in the file.

---

## Gates

**Figures below are at head `20ccf7db`** — the commit this pass produced, which
review round 3 saw. `impl.md` §7.1 is the canonical per-commit table; the current head reports 832.

| gate | result, at `20ccf7db` |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace --all-targets` | **829 passed**, 3 ignored (`7bd1a95c`: 826; master `a85a8471`: 813) |
| `userspace_run` vs live Ganesha | **22 passed, 0 failed** |
| `readdir` probe, mutated | passes |
| `readdir` probe, **negative control** | **fails on the names** |
| `run_fixtures` rewrite-backed matrix | **10 passed**, incl. `/bin/ls -l`, `/bin/ls -t` |
| side-by-side vs `a85a8471`, 25 flag shapes | **0 divergences** |
| **ten-shape request matrix, routed** | **10 answered to the tracee, 0 run-enders** (was 8/2) |
| unbound-descriptor shapes | **5 answered `EBADF`** (was 3 answered, 2 run-enders) |

Net test change: **+3** — two in `dirents` (the refusal sweep, the `reserved`
don't-care) and one in the supervisor (every exit of the request check). No test
removed; no existing body edited this pass, so the count stays at the three now
disclosed in `impl.md` §6.

**Still standing:** PAUSE BEFORE MERGING. Nothing merged, nothing pushed, one
amended change with the trailers exact.
