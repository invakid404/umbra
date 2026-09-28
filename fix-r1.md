# fix-r1 — `dg-29vwer0f` / #121, round 1

**Input:** `review-synthesis-r1.md` (17 ranked findings), `review-correctness.md`
(F1–F9), `review-scope.md` (S1–S7). Scope was clean; three HIGH correctness
defects were not.

**Outcome:** all three HIGH defects fixed and each **exhibited failing before and
passing after**. All four MEDIUMs addressed — one of them as a *measured
rebuttal* rather than the fix the review proposed. All eight LOWs landed.

**One premise in the review is wrong and it changed the fix.** The synthesis
offered `ENOTSUP`(45) as the H1 remedy because it is "what the kernel itself
answers for an unsupported attribute set". Measured on this host, the kernel
**serves** the wider set — `common=0x82079e0b file=0x0000022d` returns 4 records,
errno 0. That is precisely why master's `ls -l` exits 0, and it means an errno
*alone* could not have fixed H1: it would have replaced a stopped run with a
wrong answer on a descriptor umbra does not own. The ordering fix was mandatory;
the errno is worth having as well, and both landed. §H1 below.

---

## 1. The three HIGH defects

### H1 — the attribute-set refusal ran before the descriptor fence

**Finding (synthesis 1 / F1):** `RequestedAttributes::decode` was called from
`decode_entry`, which runs before `events.rs`'s descriptor-floor test, so a
refusal fired for every descriptor in the process — on registries with no
`descriptor_floor` at all.

**Reproduced before the fix**, side by side against a fresh `a85a8471` build at
`/tmp/graph-dg-29vwer0f/master-probe`, `--local-dev`, identical workspaces:

| flag | master | branch (before) |
|---|---|---|
| none, `-a`, `-1`, `-R` | exit 0 | exit 0 |
| `-l` `-t` `-i` `-p` `-S` `-F` `-s` `-n` | **exit 0** | **exit 1, `UnsupportedCapability during dirents`** |

Eight flags, exactly the set the reviewer named.

**What I measured before choosing the fix.** I probed the kernel directly with
the exact request shapes rather than trusting the ENOTSUP premise:

```
bitmapcount=5 common=0x8200000b file=0x00000001 opts=8  -> rc=4  errno=0   (served set)
bitmapcount=5 common=0x82079e0b file=0x0000022d opts=8  -> rc=4  errno=0   (the ls -l set — SERVED)
bitmapcount=5 common=0x8200000b file=0x00000001 opts=0  -> rc=4  errno=0
bitmapcount=3 common=0x8200000b file=0x00000001 opts=8  -> rc=4  errno=0
bitmapcount=5 common=0x8200000b fork=0x00000001 opts=8  -> rc=-1 errno=34  (ERANGE)
bitmapcount=5 common=0x80000000 file=0          opts=8  -> rc=-1 errno=22  (EINVAL)
```

The kernel answers `ENOTSUP` for none of them. So `ENOTSUP` is **umbra's own**
answer, not a copy of the kernel's, and it is only ever correct for a descriptor
umbra owns. Recorded in `dirents.rs`'s `unserved()` and in the
`nfs-userspace` README, both of which previously implied otherwise.

**Fix — ordering first, errno second.**

1. `decode_entry`'s 461 arm no longer reads tracee memory and no longer
   validates. It produces `FsOp::ReadDir { fd, max_bytes }` from registers only.
2. New `SyscallAbi::directory_request(&regs) -> Result<Option<IoBuffer>>`, a
   plain memory-free call shaped exactly like `io_buffer`, reporting where the
   `attrlist` block is (x1, fixed width). It also checks the half of the request
   that lives in a register (`options`, x4) and refuses with `ENOTSUP`.
3. `Supervisor::unserved_directory_request` reads that block **after** the floor
   test and validates the bitmaps with `dirents::RequestedAttributes::decode`.
   A refusal is bound and answered through `deny_to_tracee`, like
   `fstat(vfd, NULL)`'s `EFAULT`.
4. `RequestedAttributes::decode` lost its `options` parameter (the two halves now
   live in the two layers that own them) and its refusal now carries
   `Errno(45)`.

The IPC cost is one `Request`/`Response` pair; no callback loop, because the
method takes no memory. That was the deciding constraint — a memory-taking ABI
method would have needed the bespoke callback loop `decode_entry` has.

**Verified after the fix**, same harness, all thirteen flag shapes including
`-la`: **master and branch identical, exit 0 everywhere.**

**Routed registry, after the fix:** `ls -l` and `-t` now exit 1 with
`finished: Some(Code(0))` — the tracee gets an errno and the run survives, where
before the run ended with `errno: None`. Plain `ls` and `-R` exit 0.

### H2 — a reused descriptor inherited the previous enumeration's empty remainder

**Finding (synthesis 2 / F3):** `Overlay::directories` was inserted and never
removed, while `allocate_descriptor` reissues the lowest free number, so a second
enumeration of the same directory hit a cached empty remainder.

**Reproduced before the fix** with a two-pass `fts` fixture, read back out of the
Ganesha export:

```
HOST CONTROL                ROUTED (before)
1 alpha.txt                 1 alpha.txt
1 beta.txt                  1 beta.txt
1 gamma.txt                 1 gamma.txt
--                          --
2 alpha.txt                 (nothing)          exit 0
2 beta.txt
2 gamma.txt
```

**Fix.** `resolve_routed_close` records `(task, exec_generation, fd)` on the
`Plan` as `directory_closed`; `commit` applies it with a `retain`, in the same
branch and under the same guard as the `directory_next` insert it mirrors —
observed success, nothing journalled.

**Why `commit` and not `resolve`:** a refused close leaves the descriptor open
with its enumeration mid-flight, and evicting there would restart a partial read
from the beginning and re-serve entries the tracee already had. This is the rule
`directory_next` already follows one field up.

**The partial-read variant the review flagged as unexhibited is fixed by the same
line**, and deliberately so: dropping the pages sends a reopen back through
`merged()`, which is what a fresh `open` means. I did not build a separate
fixture for it — the eviction is unconditional on close, so there is no path by
which a partial remainder survives one and not the other.

**Verified after the fix:** both passes list all three names.

### H3 — `getattrlistbulk` on an unbound descriptor stopped the run

**Finding (synthesis 3 / F2):** `resolve_directory` used a raw
`context.fds.get(&fd).ok_or(StaleHandle)` instead of `routed_binding`.

**Reproduced before the fix**, one fixture, four calls on the same unbound
virtual descriptor 4500, output read back through the client:

```
fstat            rc=-1 errno=9
fchdir           rc=-1 errno=9
<run ended here — getattrlistbulk and close never recorded>
```

**Fix.** Routed through `Overlay::routed_binding`, exactly as
`resolve_routed_fchdir` twenty lines above already did. I also added the
`ENOTDIR` case while I was there — a directory read on a descriptor that is not a
directory would otherwise have reached `merged()` on a file path and raised the
same run-ending `Err`. That is the same asymmetry, one operand shape over.

**Verified after the fix:**

```
fstat            rc=-1 errno=9
fchdir           rc=-1 errno=9
getattrlistbulk  rc=-1 errno=9
close            rc=-1 errno=9
run finished: Some(Code(0))
```

---

## 2. The MEDIUMs

### M1 — the missing proof, landed with H1

`run_fixtures`'s `/bin/ls` rows carried no flag variant on any registry, which is
why CI stayed green through H1. The `cases` table gained a leading-flags column
and two rows: `("/bin/ls", &["-l"], "", Expect::ReadOnly)` and the same for
`-t`. All existing rows carry `&[]`; nothing was removed or loosened.

**The new rows were themselves negative-controlled.** I temporarily reintroduced
the ordering defect (moved `unserved_directory_request` above the floor test),
rebuilt, and re-ran the matrix:

```
PASS local /bin/ls <workspace>        <- plain ls still passes
PASS local /bin/ls absent             <- and so does the absent operand
panicked at run_fixtures.rs:655       <- the -l row FAILS
test result: FAILED
```

Then restored and re-ran: all ten cases pass. So the row detects exactly the
defect it exists for, and is not satisfied by the plain rows' cause.

### M2 — recursive `fts`: a measured rebuttal, and a different doc fix

**I could not reproduce F4's silent truncation, and I am reporting that rather
than making the change it implies.**

Measured on the routed registry against host controls, after restoring the
provider's `Stdio::inherit` temporarily so real binaries' output was observable:

| subject | host | routed |
|---|---|---|
| `/bin/ls -R` on `tree/{top.txt, a/{mid.txt, b/deep.txt}}` | 8 lines | **byte-identical** |
| `/usr/bin/find` on a 3×(3+3) tree | 25 lines | **25** |
| `/bin/ls -R` on the same wider tree | 36 lines | **36** |
| recursive `fts` fixture (`FTS_PHYSICAL\|FTS_NOSTAT`) | 9 entries, errno 0 | **9 entries, errno 0** |

I also re-ran the fixture with the H2 eviction **reverted**, in case truncation
was an H2 symptom: still 9 entries. So it is not that either.

I cannot explain the reviewer's measurement and I am not claiming they erred —
the most likely difference is fixture flags (my own `umbra-userspace-listing.c`
calls `fts_set(FTS_SKIP)` deliberately to stay non-recursive, and a fixture
adapted from it would show exactly "never yields `tree/a` as `FTS_D`"). What I
can say is that the two **real binaries the README row is about** match the host
exactly, on two tree shapes, before and after every fix in this pass.

**What I changed instead.** The row's problem is real but in the other direction:
it called the whole unclaimed set "fail-closed", and for recursion that is false
*because recursion apparently works*. Claiming it works would absorb out-of-scope
surface; leaving "fail-closed" would be a false invariant. So the row was split:

- the metadata-requesting modes keep an accurate fail-closed claim, now with the
  `ENOTSUP`-after-the-fence disposition spelled out;
- `-R` gets its own row saying it is **not claimed and not tested**, that it
  appears to work, that the round-1 review measured the opposite, and that the
  discrepancy is recorded here rather than resolved by assertion.

**Making the descent fail loudly was considered and is not possible**: when `fts`
declines to descend it issues no syscall, so umbra is never asked anything and
has nothing to refuse. There is no hook.

### M3 — `umbra-overlay/README.md`

"A routed `Open` of a directory is refused: directory reads are not routed" —
made false by this slice, and the file was absent from the diff. Replaced with
the writable-vs-read-only split and the `EBADF`/`ENOTDIR`/`ENOTSUP` dispositions,
including the after-the-fence ordering. The pre-existing "two cargo features"
claim (three since #120) was corrected in the same edit, as the review suggested.

### M4 — the false invariant in `events.rs`

The comment asserting all four new calls "resume into the kernel exactly as they
did when they were not breakpointed at all" was the invariant H1 violated. It is
now corrected **and keeps its own history**: it records that the sentence was
false when written, that it was true of three calls and not the fourth, what made
the fourth different, and that this test is the only gate the four calls have.
Deleting it quietly would have lost the reason.

---

## 3. The LOWs

| # | what | where |
|---|---|---|
| L1 | `close`(6) self-contradiction inside the granted waiver | `umbra_interpose.c` entry 6 rewritten: `close`(6) is **both** interposed and in `TRACED_STUBS`, which is the newly-true fact; entry 2 no longer lists it as one this file "does not" route; the waiver paragraph now reads "was interposed and *only* interposed … it is now [in `TRACED_STUBS`]" |
| L2 | "the one edit" — there were two | `impl.md` §6 now names both, with the `abi.rs` one marked *voluntary* (the test iterates only the numbers it lists, so it would have passed unchanged) |
| L3 | baseline 822 → 813 | **Measured myself**: `cargo test --workspace --all-targets` at `a85a8471` is **813**. Branch is **826**. Reconciles: the review's expected 825 (+12), plus my two new ABI tests, minus the one this pass replaced |
| L4 | D1's "nothing was given up" | `impl.md` §1.3 rewritten: an engine-derived binding was traded for an encoder-supplied one, the compensating walk lives inside the thing being policed, and it is still the right trade because the old form was unsatisfiable truthfully |
| L5 | `PACKING` cites the wrong offset | **Re-measured both ways.** In the served set `FILEID` is at `0x28` and *is* 8-aligned, so it proves nothing. Dropping `ATTR_CMN_DEVID` (`common=0x82000009`) moves it to `0x24`, 4-aligned, no pad — that is the evidence, and the comment now cites it with both layouts side by side and says why the served shape cannot be the witness |
| L6 | parallel constants | `abi.rs`'s private `ATTR_BIT_MAP_COUNT`/`ATTRLIST_BYTES` replaced with `use dirents::…`, so `setattrlistat`(524) and the 461 path read one source |
| L7 | stale enumerations | `Deny` list: "six things" → **eight**, with `resolve_routed_fchdir`/`resolve_directory` added to the `EBADF` item and two new items (`ENOTDIR`, the supervisor's `ENOTSUP`). `path_operands`' "deliberately absent" note now names all six descriptor-only numbers. "the three it added" → four. "five mutation probes" → six (both sites). C/Rust twin test message now names `Fstat \| ReadDir \| Fchdir \| Close`. README unclaimed-mode list replaced with the actual trigger ("any mode that makes `fts` ask for metadata") plus the eight flags |
| L8 | `impl.md §1.5` dangle | Repointed to `umbra-platform-macos/README.md`, which genuinely carries the arming order (`ARMING`, `Session::arm_interposer`) — verified, not assumed |

---

## 4. Measured versus inferred, this pass

**Measured (ran it, this session):** the H1 side-by-side against master before
and after, 12 flag shapes then 13; the kernel's own errno for six attribute-set
shapes; the H2 two-pass fixture before and after, read out of the export; the H2
fixture again with the eviction reverted; the H3 four-call fixture before and
after; the M1 negative control (defect reintroduced, `-l` row fails, plain rows
pass, then restored); `ls -R` and `find` against host controls on two tree
shapes, with and without the eviction; a recursive `fts` fixture likewise; the
`FILEID` offset in two attribute sets; master's own `cargo test` count (813); the
full `userspace_run` suite; the `readdir` probe and its negative control; the
rewrite-backed matrix including the new rows.

**Inferred, not measured:** that the H2 partial-read variant is fixed — the
eviction is unconditional on close so no partial remainder can survive one and
not the other, but I did not build a small-buffer fixture to exhibit it; that
`close`(6) is genuinely needed beside `__close_nocancel`(399) — still unproven,
and round-1 review agrees the claim in `impl.md` needs no correction; that no
*other* program's wider `getattrlistbulk` newly reaches a refusal it previously
could not — the fence now protects descriptors umbra does not own, which is the
sharp edge, but I swept only the `ls` flag matrix and `find`.

**Not fixed, with reason:** F4's proposed change (see M2 — rebutted with
measurement, different doc fix applied instead). `memoria check` (S7) still
cannot run from this workspace: `.jj`, no `.git`, exit 4. That is #123's hazard
firing on the tooling, and the ratified remedy is workflow-only; the `publish`
node runs it from a git worktree.

---

## 5. Gates

**Figures below are at head `7bd1a95c`** — the commit this pass produced, which
review round 2 saw. `impl.md` §7.1 is the canonical per-commit table; the current head reports 832.

| gate | result, at `7bd1a95c` |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean, 0 issues |
| `cargo test --workspace --all-targets` | **826 passed**, 3 ignored (master `a85a8471`, measured: **813**) |
| `userspace_run` vs live Ganesha | **22 passed, 0 failed** |
| `readdir` probe, mutated | passes |
| `readdir` probe, **negative control** | **fails on the names**, as required |
| `run_fixtures` rewrite-backed matrix | 10 passed, including the two new flag rows |
| **side-by-side vs `a85a8471`, `--local-dev`** | **13 flag shapes, identical** |

Net test change this pass: +2 ABI tests (the 461 decode's freedom from the
attribute set and from tracee memory; `directory_request`'s reporting and its
register-half refusal), −1 (the test that pinned the *old* placement, replaced by
the two above), +2 matrix rows. All new `#[test] fn`s.

**Correction, made in round 2:** this sentence said "the only existing bodies
touched remain the **two** now disclosed in `impl.md` §6". It was **three** — the
C/Rust twin test's failure *message* also changed, in L7 of §3 above, which is
where round-2 scope review found it. The edit is message-only and the asserted
value is byte-identical; the count was the error. `impl.md` §6 now names all
three.

**Still standing:** PAUSE BEFORE MERGING. Nothing merged, nothing pushed, no
second jj change — the existing one is amended with the required trailers.
