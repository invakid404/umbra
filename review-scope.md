# review-scope — `dg-29vwer0f` / #121, round 4

**Slice:** jj change `psvqmvlm` / `3480713b`, amended, one change above master
`a85a8471`. 31 files, +5324/-135.

**Authority:** the `RATIFICATION` section of `dg-29vwer0f-design-gate.md`
(revision 13). `impl.md`, `fix-r1.md`, `fix-r2.md` and `fix-r3.md` are claims to
be checked.

**Archives:** rounds 1 (S1–S7), 2 (R1–R5), 3 (clean).

---

## Verdict

**Scope holds. One finding, and it is bookkeeping rather than mechanism.**

The mechanism footprint of this pass is remarkably small and I measured it rather
than reading it off the fix report: **two statements in `resolve_directory`, one
new constructor plus its single call site in `dirents.rs`, and four new tests.**
`abi.rs` and `events.rs` changed **comment-only** — zero non-comment lines
against round 3. Nothing else in the tree moved.

Items 1, 2 and 4 are all within the ratified envelope, and item 1 — the one that
touches `resolve`'s return contract — I verified at both ends rather than
reasoning from the rule's text.

**The finding:** `fix-r3.md` §7 says "Net test change: **+4**, no test removed."
Measured, it is **+3**, and one test *was* removed. The guardrail is unaffected
(the removed test is one this slice added in round 1, not an at-master test) and
the gate figure of 832 is correct. But it is a quantitative claim that does not
reconcile, in the same sentence that reassures about the test-body guardrail that
has been misreported before.

**Fifth test-body sweep: still exactly three, still the same three.** No fourth.
`impl.md` §6 still names all three accurately and still records that the count
was wrong twice.

Recommendation: **proceed to `merge_gate`**, as round 3 already did. F1 is one
line of markdown.

---

## Scope guardrail table

Basis: **re-measured** = re-run against `3480713b`; **file-identical** = that
file's diff hunk byte-compared against `20ccf7db` and unchanged, so round 3's
measurement stands; **judged** = ratification applied to measured facts.

### Carried forward

| # | Guardrail | R3 | R4 | Basis | Evidence |
|---|---|---|---|---|---|
| **G1** | Only ratified waivers touched | PASS | **PASS** | re-measured | 31 files. Four source files changed, two of them comment-only; one test file newly in the diff, purely additively; three new markdown records. Every change traces to a round-3 correctness finding. |
| **G2** | `ls -l`/`-la` (`listxattr`(240)) absent | PASS | **PASS** | re-measured | No decode for 240 anywhere. The served attribute set is untouched (`dirents.rs`'s only code change is the `too_small` constructor). `-l` still refused, not served. |
| **G3** | `ls -@`/ACLs absent | PASS | **PASS** | re-measured | No hits in added non-markdown lines. |
| **G4** | `ls -R` descent absent | PASS | **PASS** | re-measured | No recursion mechanism. `run_fixtures.rs`, `userspace_run.rs` and the C fixture are all **file-identical** to round 3, so no `-R` case was added and the fixture still calls `FTS_SKIP`. The nfs README is file-identical, so round 3's `-R` row stands as verified. |
| **G5** | Symlink-following absent | PASS | **PASS** | re-measured | `DirectoryEntry` and `FsOp::ReadDir` untouched. |
| **G6** | xattrs in `getattrlistbulk` absent | PASS | **PASS** | re-measured | Served bitmap and `options` restriction unchanged. |
| **G7** | `getdirentriesattr`(222) absent | PASS | **PASS** | re-measured | No decode arm for 222. |
| **G8** | `getdirentries`(196)/`(344)` absent | PASS | **PASS** | re-measured | Grepped `abi.rs` for `196|344|240|222|326 =>`: **no decode arms**. |
| **G9** | `opendir`/`readdir`/`closedir`(3) absent | PASS | **PASS** | re-measured | No hits in added non-markdown lines outside comments. |
| **G10** | `chdir`(12) absent | PASS | **PASS** | re-measured | No decode for 12. |
| **G11** | `getcwd`(326) absent | PASS | **PASS** | re-measured | No decode for 326. |
| **G12** | **ANY fork/exec cwd propagation absent** | PASS | **PASS** | re-measured | `supervisor/lib.rs` and the `ChangedCwd` arm in `events.rs` are unchanged (events.rs has no non-comment change). Standing condition 2 untriggered. |
| **G13** | **#123: no preflight script** | PASS | **PASS** | re-measured | `scripts/` byte-identical to master; no `*preflight*` file in the tree. |
| **G14** | **#123: no guard, no `.git` check** | PASS | **PASS** | re-measured | **Fifth sweep, each hit attributed: 8 hits, all in committed markdown** (`review-scope.md` 5, `fix-r1.md` 2, `review-synthesis-r1.md` 1). Zero in code, zero in `ci.yml`. |
| **G15** | `ci.yml` is the probe, not #123 | PASS | **PASS** | file-identical | Byte-identical across all four revisions. |
| **G16–G18** | Decode 461, decode 13, `io_buffer` arm for 461 | PASS | **PASS** | re-measured | `abi.rs` has **zero non-comment changes** this pass, so all three stand exactly as verified in rounds 1–3. |
| **G19** | `getattrlistbulk` reply encoder | PASS | **PASS** | re-measured | Encoder body untouched; the one code change is the refusal constructor — **G67**. |
| **G20** | `TRACED_STUBS` rows 461/13/399/6 and no others | PASS | **PASS** | re-measured | `abi.rs` comment-only, so the list is unchanged; re-confirmed one list, same four rows. |
| **G21** | Engine `Fchdir` arm + supervisor plumbing | PASS | **PASS** | re-measured | `resolve_routed_fchdir` untouched; `supervisor/lib.rs` file-identical. |
| **G22** | Directory refusal lifted, narrowed not deleted | PASS | **PASS** | re-measured | The `O_APPEND` `Err` and the writable-directory guard are outside this pass's two changed statements. |
| **G23** | G-ii wiring; **overlay contract intact** | PASS | **PASS** | re-measured | Re-checked because `resolve_directory` changed: `resolve_directory` still hard-requires the injected encoder, and that requirement is *still* an errno-less `Err` — correctly, since a missing encoder is umbra contradicting itself, not a tracee input. `DirectoryEncoder`, `set_directory_encoder` and `platform/src/lib.rs` all unchanged. |
| **G24** | #125 filed, not acted on | PASS | **PASS** | re-measured | `StatEncoder`/`encode_stat`/`routed_stat_write` appear only as prose. |
| **G25–G27** | `ls` case; entry-based probe; probe discipline | PASS | **PASS** | file-identical | `userspace_run.rs`, `directory.rs` and `run_fixtures.rs` all byte-identical to round 3. |
| **G28** | Three doc corrections confined to the waiver | PASS | **PASS** | re-measured | `umbra_interpose.c`, `capability.rs` and both READMEs are **all file-identical** to round 3. All three waivers closed and unreopened. |
| **G29** | The corrections are factually right | PASS | **PASS** | file-identical | Unchanged since round 3's verification. |
| **G30** | §8 unsupported-modes documentation | PASS | **PASS** | file-identical | nfs README unchanged. |
| **G31** | `TRACED_STUBS` single list + `Delivery` exhaustiveness (#116) | PASS | **PASS** | re-measured | One list; `delivery()` and `intercept()` untouched. |
| **G32** | Descriptor fence and its C/Rust twin | PASS | **PASS** | re-measured | `umbra_interpose.c` file-identical so `umbra_owns` is untouched; the twin test's asserted string still byte-identical; the fence's own arm in `events.rs` unchanged (comment-only pass). |
| **G33** | **Err-vs-`Emulate`/`Deny` rule and its dangling-`Prepare` reasoning** | PASS | **PASS** | re-measured, **specifically** | The row most exposed by this pass, re-derived at both ends rather than carried forward — **G66**. |
| **G34** | `replay_must_poison` | PASS | **PASS** | re-measured, **specifically** | `engine/tests.rs` is **newly in the diff** this pass, so this needed real re-verification rather than absence: its hunk has **zero deletions** and `replay_must_poison` appears **nowhere** in it. `umbra-core` still 0 files; `journal.rs` still absent. The file gained two `#[test] fn`s and nothing else. |
| **G35** | Setup / recovery / cleanup / validation guarantees | PASS | **PASS** | re-measured | **Nothing removed.** Two errno-less `Err`s became errno-bearing refusals and one of those became a `Deny` — strictly more of the tracee's inputs answered rather than fewer checks. The D1 validation block (`consumed`/`total`/`max_bytes`) is intact and unmoved. Two prose-grade guarantees became tests (a capacity sweep asserting `Errno(34)` on every sub-record capacity, with a refusal count so it cannot pass vacuously; and an engine pair pinning `Deny` vs `Err` at the encoder seam). |
| **G36** | **NEW `#[test] fn`s only** | PASS | **PASS** | re-measured | **Fifth sweep: no fourth edit.** Exactly the same three modified bodies as rounds 2–3; `engine/tests.rs` gained 2 with MODIFIED=0. `impl.md` §6 still says "Three edits … all flagged" and keeps the history. One test was **removed**, but it is `a_buffer_too_small_for_one_record_is_refused`, introduced by this slice in round 1 — not an at-master test, so the guardrail is untouched. It is the subject of **F1**. |
| **G37–G39** | New files; toy C fixture; Cargo deps/features | PASS | **PASS** | file-identical | No new source files. All three `Cargo.toml` and `Cargo.lock` byte-identical across all four revisions — **no dependency added in any pass** beyond round 1's test-only `uuid`. |
| **G40** | `impl.md` committed | PASS | **PASS** | re-measured | Precedent unchanged; §6 unchanged and still accurate. |
| **G41** | Audit records preserved | PASS | **PASS** | re-measured | Nothing under `graph-audits/` modified. |
| **G42** | No new setup/mounts/drivers/privileged steps | PASS | **PASS** | file-identical | `ci.yml` unchanged. |
| **G43** | PAUSE BEFORE MERGING | PASS | **PASS** | re-measured | One amended change `3480713b`, parent `master@a85a8471`, nothing pushed. |
| **G44** | Standing condition 1 — cwd measured | PASS | **PASS** | file-identical | Unchanged. |
| **G45** | Every #55–#120 mechanism intact | PASS | **PASS** | re-measured | G31–G35 each re-derived at `3480713b`, G33 and G34 in detail. |
| **G46–G47** | Falsified docs corrected; quantitative claims reconcile | PASS | **PASS / FAIL** | re-measured | Docs: both READMEs file-identical, so closed. Quantitative: **F1** — the first claim in four rounds that does not reconcile since round 1's S4. |
| **G48** | `SyscallAbi::directory_request` + provider pair | PASS | **PASS** | file-identical | `platform/src/lib.rs` and `provider.rs` both byte-identical to round 3 — the trait default and the IPC pair are untouched, so rounds 2–3's rulings stand unchanged. |
| **G49–G55** | run_fixtures column; S1–S7 fixes; `decode`'s `options`; review artefacts; sweep meaningfulness; count reconciliation; extra `ENOTDIR` | PASS | **PASS** | file-identical / re-measured | Every file carrying these is byte-identical to round 3 except the count (G47/F1) and `resolve_directory`'s `ENOTDIR`, which is unchanged — this pass added statements *after* it, not to it. |
| **G56–G57** | R1 closed (§6 names three); R2 closed (§6 accurate for both passes) | PASS | **PASS** | re-measured | `impl.md` §6 unchanged and still correct for a third consecutive check. |
| **G58–G62** | Default flip; `reserved` confined; test double test-only; R4; R3 (`-R` row) | PASS | **PASS** | file-identical | All the files carrying these are byte-identical to round 3. |
| **G63** | Which committed artefacts describe the current revision | PASS | **PASS**, noted | re-measured | Refreshed for `3480713b`: `impl.md`, `fix-r1/r2/r3.md`, `review-synthesis-r3.md`, and `review-scope.md` once this lands. Describing `20ccf7db`: `review-correctness.md` and, until this file lands, `review-scope.md`. Normal mid-cycle state, as in round 3. No action. |
| **G64** | #126 and #127 filed, **not acted on** | PASS | **PASS** | re-measured | Neither acted on. #126 is now *cited by name* in the committed input table (rows 2 and 4) as the disposition for an unmapped pointer — citing a tracked issue as the honest current answer is the opposite of acting on it. No mapping probe anywhere; `umbra_interpose.c` and `provider.rs` file-identical. |
| **G65** | N8 recorded rather than fixed | PASS | **PASS** | re-measured | `engine.rs`'s only changes this pass are the two statements in `resolve_directory`; the N8 comment and the eviction are untouched. |

### New rows for this pass

| # | Guardrail | Result | Basis | Evidence |
|---|---|---|---|---|
| **G66** | **Converting an `Err` to a `Deny` inside `resolve` does not breach the Err-vs-`Emulate` rule** | **PASS, in scope** | measured at both ends | Verdict below (item 1). |
| **G67** | **The `ERANGE` refusal is confined to the capacity arm; the served path is unchanged** | **PASS** | measured | `dirents.rs`'s only code changes are the `too_small()` constructor (`ErrorKind::InvalidInput` preserved, `Errno(34)` added) and its single call site replacing an inline `UmbraError::new`. The encoder's packing, `count_records`, `decode` and the served constants are untouched. The new `EINVAL`(22) is `resolve_directory`'s bound check changing disposition, not a new check. **Both are more refusal surface answered to the tracee, and neither narrows what is served** — the round-2 ten-shape matrix is reported unregressed and `short/` now matches the kernel at every capacity. |
| **G68** | **The committed input-enumeration table and its normative rule** | **PASS, in scope** | judged | Verdict below (item 2). |
| **G69** | **The deliberate divergence from the kernel at a sub-record capacity** | **PASS, in scope** | judged | Verdict below (item 4). |
| **G70** | `engine/tests.rs` entering the diff is purely additive | **PASS** | measured | Zero deletions in its hunk; two new `#[test] fn`s and no other content; no existing body modified; `replay_must_poison`'s test untouched. This file's absence had been part of G34's basis for three rounds, so its arrival needed checking rather than noting. |

---

## Verdict on item 1 — error dispositions across two crates, and an `Err` → `Deny` inside `resolve`

**Verdict: in scope, and not wider than the finding required.** The `Deny`
conversion I verified mechanically at both ends rather than reasoning from the
rule's text, because this is the rule most exposed.

**What actually changed, measured:** two statements, both inside
`resolve_directory`. The bound check's errno-less `Err` became
`Ok(ResolvedAction::Deny(Errno(22)))`, and the encoder-error path became
`match e.errno { Some(errno) => Deny(errno), None => Err(e) }`. Plus, in
`dirents.rs`, one named constructor carrying `Errno(34)` replacing an inline
errno-less `UmbraError::new`. That is the whole mechanism footprint across both
crates.

**Why the `Deny` step is necessary rather than gold-plating.** Attaching the
errno alone measures as no fix: the supervisor's arm is `Err(e) => return Err(e)`
and never unwraps an errno from a resolution failure. So the choice was not
"errno or `Deny`" — it was "`Deny`, or leave the run ending." The narrow reading
of the finding would have produced a change that passed review and fixed nothing.

**Why it does not breach G33.** The rule at `engine.rs:2596` says POSIX refusals
here are `Err` rather than failure-shaped **`Emulate`**, because the supervisor
refuses an `Emulate` only *after* `prepare` has journalled. The hazard is specific
to `Emulate`. The same comment names `Denied` as "this function's idiom for 'POSIX
says no'". So this conversion moves *toward* the rule's own preferred idiom. I
confirmed the mechanical precondition at both ends:

- **Engine side:** the three `Deny` returns are at `engine.rs:4550`, `:4577` and
  `:4620`; `self.planned = Some(Plan { … mutation: false … })` is at `:4668`. Every
  `Deny` is an early return before any `Plan` exists.
- **Supervisor side:** `if let ResolvedAction::Deny(errno) = action` at
  `events.rs:619` returns `deny_to_tracee` **before**
  `self.namespace.prepare(id, &action)` at `:628`. A `Deny` never reaches
  `prepare`, so it cannot journal.

**A dangling `Prepare` is therefore structurally impossible for a `Deny`**, at
both layers independently. The rule is preserved and, on the specific path this
slice added, better served than before.

**And the errno-less `Err` was kept exactly where the rule wants it:** the missing
encoder (`"ReadDir requires injected native directory encoder"`), a length
disagreeing with the operation, and a re-walk finding a different count all still
raise. Those are umbra contradicting itself, with no program to answer. The split
is along the right seam, and I checked each surviving `Err` rather than taking the
claim.

The ordering change (the bound check moved after `routed_binding` and the
directory-kind check) is in scope for the same reason round 3's item 6 analysis
ran: it makes umbra agree with the kernel's descriptor-before-argument precedence
on a path this slice created, and it narrows nothing.

---

## Verdict on item 2 — a normative rule committed into the codebase

**Verdict: in scope. I agree it is the best thing in the pass, and here is why
that is a scope judgement and not just praise.**

Three properties make it in-scope rather than a durable artefact beyond the
finding:

1. **It compiles to nothing.** I confirmed `abi.rs` has **zero non-comment
   changes** this pass — the table and the rule are a doc comment on
   `directory_bytes`, a private function in a file this slice already owns under
   ratified items 1 and 2. No mechanism, no test, no build input.
2. **It is correctly scoped and does not set tree-wide policy.** The wording is
   "a value **in this table** may only be refused with an errno attached, and
   inside `umbra-overlay` that errno must become `ResolvedAction::Deny` before it
   leaves `resolve`." It binds this input set and this function, not the
   repository. That distinction is what separates in-scope documentation of an
   invariant the slice established from out-of-scope policy-setting it has no
   standing to do, and the wording falls on the right side. I checked it for
   exactly that.
3. **It is the remedy pattern the gate already blessed, applied where the type
   system cannot reach.** The audit's §G records that `intercept()` matches
   exhaustively on `Delivery` "so a row whose disposition nobody chose will not
   compile — the #116 remedy working as designed." Here no type can enforce it, so
   a committed enumeration beside the input is the next best thing.

The stronger argument is empirical, from this slice's own history. Three passes
enumerated three different axes and each missed what the next one found:
constructors (round 2) missed an exit, exits-of-one-function (round 2) missed an
input, and neither crosses a crate boundary — which is exactly where row 5's two
holes were. A committed rule that names the axis is the thing that stops a fourth
round of this. Leaving it in `fix-r3.md` would lose it, because the fix reports do
not land on master (verified in round 2: all of them are absent there), and rounds
1–3 repeatedly showed that fix-report-only knowledge is what gets re-derived
wrongly.

**Accuracy check, since a committed rule that is wrong is worse than none:** I
verified rows 1, 3, 5 and 6 of the table against the code directly this round, and
rows 2 and 4 in rounds 2–3. Row 5's claim that `EINVAL` comes "from whichever of
the two reaches it first" matches what I read — `io_buffer`'s zero refusal and
`resolve_directory`'s bound check now both answer 22.

---

## Verdict on item 4 — a knowing divergence from the kernel

**Verdict: within the ratified envelope. No fresh ratification needed.** Judged.
Worth *notice* at `merge_gate` for one specific reason, given at the end.

The ratified objective is "serve directory reads on a virtual descriptor, so `ls`
works on `nfs-userspace` as it already does on the rewrite-backed registries",
with the success criterion `ls` exit 0, correct entries, host untouched. **Nothing
in the ratification is premised on byte-for-byte kernel parity at arbitrary buffer
sizes**, and the criterion is untouched: `ls` sizes its own buffer (32 KiB,
measured in the audit and re-measured in `impl.md` §3), so reaching this case
needs a caller-supplied capacity between one byte and one record on a directory
whose byte-first entry is large. `ls` never does that, and the 25-shape
side-by-side against master reports zero divergences.

**The direction is the conservative one, and the alternative is worse.** umbra
answers a real errno where the kernel would serve fewer entries. Matching the
kernel would mean skipping the oversized entry and serving out of byte-sorted
order, which is what makes `directory_next`'s paging deterministic across calls —
so a later page would repeat or skip entries. That trades a loud errno for a
silent wrong answer, which is precisely what the engine's stated posture refuses
("Refusals rather than wrong answers, in every case the engine cannot represent",
the overlay README line I verified in round 2). Choosing the refusal is the
ratification's own preference, not a departure from it.

**And it is not the first such divergence — it is the same species as one already
ratified.** Rounds 2–3 established by measurement that the kernel **serves**
`common=0x82079e0b file=0x22d`, and umbra refuses it with `ENOTSUP`. That
divergence is ratified: §8 fail-closes everything outside the bounded attribute
subset. So this slice already diverges from the kernel by design, with the human's
approval, wherever umbra cannot represent what was asked. `ERANGE` at a
sub-record capacity is umbra refusing what it cannot represent, which is the same
rule one input over.

**Where it is documented, and the one thing worth the human's eye.** It is
documented at the mechanism — `encode`'s doc comment states both measurements
side by side and a test asserts the byte-order premise (`long_name < b"aaa"`)
rather than assuming it. That is the right place for the *cause*. But it is the
first divergence that is a function of a **capacity** rather than of a **mode**,
so it is not covered by the §8 fail-closed list, and the nfs-userspace README —
which is where every other divergence is enumerated for a reader — is **silent**
on it (I checked: its only "capacity" mention is unrelated transport paging).
Silence is not falsehood and the README claims nothing this contradicts, so
**there is no finding here and no action is required.** I record it only because
if the human wants all divergences discoverable in one user-facing place, that
table is where the others live.

---

## Finding

### F1 — `fix-r3.md` §7's test-count claim does not reconcile (LOW; bookkeeping, not mechanism)

`fix-r3.md` §7 states: *"Net test change: **+4**, no test removed."*

Measured: **+3**, and one test was removed.

| | count |
|---|---|
| `#[test]` attributes at `20ccf7db` | 905 |
| at `3480713b` | **908** |
| `crates/umbra-overlay/src/engine/tests.rs` | 162 → **164** (+2) |
| `crates/umbra-platform/src/dirents.rs` | 11 → **12** (+2 new, −1 removed) |

`a_buffer_too_small_for_one_record_is_refused` is gone, superseded by
`every_output_bound_refusal_carries_a_bindable_errno` and
`a_long_name_that_sorts_early_refuses_a_capacity_a_short_directory_serves`. Four
tests were indeed *written*, which is what §5's table lists correctly; the net is
+3 because one was replaced.

**What this does not affect.** The removed test is one **this slice introduced in
round 1**, not a test that exists at `master@origin`, so G36 is untouched and no
at-master coverage was lost. The replacement is strictly better and is the thing
earlier rounds asked for — a bare `is_err()` (which the run-ending form satisfied)
replaced by a sweep asserting `Errno(34)` on every sub-record capacity, with a
refusal count so it cannot pass vacuously. **And the gate figure is correct:**
813 + 21 new attributes − 2 gated behind `userspace_run.rs:79`'s
`#![cfg(feature = "transport-raw", …)]` = **832**, matching the reported result, so
the arithmetic that matters reconciles.

**Why I am reporting it anyway.** It is the same class as round 1's S4, it is one
of two quantitative claims in the report, and it sits in the sentence that goes on
to reassure about the test-body guardrail — the guardrail that was misreported in
rounds 1 and 2 and is enforceable only through the self-report plus a sweep. A
"no test removed" that is not quite true is worth a line, in that sentence more
than anywhere else.

*Action for `merge_gate`:* correct §7 to "+3 net, four written, one superseded."
No code change; the replacement itself needs no defence.

---

## Measured vs judged

**Measured this round:** the full 5616-line diff, split per file and byte-compared
against `20ccf7db` (5 source/test files changed, 16 byte-identical); the fifth
`#[test]`-body sweep; per-file attribute counts at both revisions and the name-level
diff of `dirents.rs`'s test list; the reconciliation to 832; the #123 sweep with
per-file attribution; every non-comment line of the four changed source files
(`abi.rs` and `events.rs`: zero); `resolve_directory`'s statement order and the
line positions of all three `Deny` returns against `self.planned`; the supervisor's
`Deny` arm at `events.rs:619` against `prepare` at `:628`; the surviving
errno-less `Err`s; the Err-vs-`Emulate` rule text at `engine.rs:2596`;
`engine/tests.rs`'s hunk for deletions and for `replay_must_poison`; the input
table's six rows against the code; the nfs README's silence on capacity; the
absence of decode arms for 196/344/240/222/326.

**Judged:** items 1 (whether the `Deny` conversion is wider than the finding
required), 2 (whether a committed normative rule is in scope, and whether its
wording over-reaches), 4 (whether a knowing divergence needs the human); whether
F1 clears the bar for a finding.

**Accepted from you, not re-verified:** the three gates (832 passed, 0 failed, 3
ignored) — though I re-derived the reconciliation independently for the fourth
round; the single amended change and byte-exact trailers;
`provider/transport.rs`'s absence; your probes showing both MEDIUMs closed.

**Still unverified by anyone:** `memoria check`, unrunnable here (`.jj`, no
`.git`), ratified workflow-only, handled at the `publish` node.

---

## Bottom line for `merge_gate`

- **Scope clean for a fourth round on the out-of-scope list**, which has not been
  touched in any pass: `ls -l` refused not served, `-R` descent absent, no xattrs,
  no 222/196/344/3, no 12/326, no fork/exec cwd, `umbra-core` untouched throughout,
  no dependency added beyond round 1's test-only `uuid`.
- **Items 1, 2 and 4 are all in scope**, each for a reason worth recording: item 1
  because `Deny` is the idiom the rule itself names and never reaches `prepare` at
  either layer; item 2 because it compiles to nothing, is scoped to its own table,
  and is the #116 remedy where no type can enforce; item 4 because refusing what
  umbra cannot represent is the posture the ratification already approved for the
  attribute set, one input over.
- **G36 passes a third consecutive time** and the sweep found no fourth edit.
  `engine/tests.rs` entering the diff is purely additive and `replay_must_poison`
  is untouched.
- **F1 is one line of markdown.** Nothing else outstanding from scope review
  across all four rounds except `memoria check` at `publish` and the standing
  PAUSE.
