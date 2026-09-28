# impl — `dg-29vwer0f` / #121: directory reads on a virtual descriptor

**Objective, as ratified:** serve directory reads on a *virtual* descriptor, so
`ls` works on `nfs-userspace` as it already does on the rewrite-backed
registries. Not "ship `ls`" — `/bin/ls` already exited 0 on `--local-dev` and
kernel-`nfs` before this slice, and still does (re-verified, §5).

**Outcome:** achieved and proven by execution.
`umbra run --registry <nfs-userspace> -- /bin/ls <dir>` lists the shadow's
merged entries and exits 0, host untouched, against the live NFS-Ganesha
fixture.

---

## 1. Read this first: three deviations from the ratified plan

All three were forced by measurement, none expands the scope, and each is stated
here rather than buried in a diff. Nothing in the ratified *objective* changed.

### 1.1 G-ii could not be implemented the way it was described — the wire format
had to move

The gate ratified **G-ii: "implement the `DirectoryEncoder` trait over the ABI
and call `set_directory_encoder` in production."** The first half is not
constructible:

* `pub trait DirectoryEncoder: Send` (`umbra-overlay/src/engine.rs`), and it is
  called from inside the overlay's `resolve`, in the supervisor's process.
* The production `SyscallAbi` is an IPC proxy holding `Rc<RefCell<Client>>`
  (`umbra-platform/src/provider.rs:132-134`) — **not `Send`**.
* `umbra-platform-macos` is a provider *executable*, not a library dependency of
  anything (`crates/umbra-cli/Cargo.toml` says so explicitly). Its code does not
  run in the supervisor's address space at all.

So no encoder that reaches the production ABI can satisfy the trait's bound, and
the ABI's code is in the wrong process regardless.

**What was done instead**, chosen to keep every ratified property of G-ii:
the `getattrlistbulk` wire format lives in `umbra-platform::dirents`, a crate
**both** the supervisor and the macOS backend already depend on. The
`DirectoryEncoder` (`umbra-supervisor/src/directory.rs`) is implemented over
that, and is injected in production. So G-ii's ratified benefits hold exactly:
**no overlay contract change, and `resolve_directory`'s paging and validation are
used as written.** The only deviation is *where the format's code physically
lives*, which the gate never specified. It is one format with two readers, which
is strictly better than the duplication G-ii was accepted as tolerating.

*Confidence: high.* The `Send`/`Rc` incompatibility is a compile-time fact, not
a judgement.

### 1.2 The injection point is one site, not the two the gate named

The gate named `run.rs:1205` and `:1587` ("session construction"). Injection is
instead in `Supervisor::launch_prepared` (`umbra-supervisor/src/events.rs`),
because:

* it is the **single** path every launched run takes, so two sites cannot drift
  — which is the entire #116 lesson this slice is a second instance of; and
* it is the first point where the negotiated ABI label is known, and the
  injection is gated on it (`budget.abi == dirents::DARWIN_ARM64_ABI`) and on the
  run being routed. At `run.rs:1205` the platform has not been connected yet.

*Confidence: high.* Proven live — see §4.

### 1.3 `resolve_directory`'s return-value check was wrong for the only ABI umbra
has, and was corrected rather than removed

`resolve_directory` required `outcome == Success { return_value: total_bytes }`.
Measured against the kernel: **`getattrlistbulk` returns a count of entry
records, not bytes** — four entries in 248 bytes return `4`. Telling `fts` it had
received 248 entries is worse than any refusal.

This was never exercised before: nothing implemented `DirectoryEncoder`, so no
directory read had ever run. The check was written for a `getdirentries`-shaped
call.

**It was changed to `return_value == encoded.consumed`.** Round-1 review was
right that "nothing was given up" overstated it, and the honest accounting is
this: the engine traded its one binding of the tracee-visible return value to an
**engine-derived** quantity (`total`, summed by the engine from the encoder's
memory writes) for a binding to an **encoder-supplied** one (`consumed`, which
arrives in the same struct as `return_value`). What compensates -- the record
re-walk -- lives inside `AbiDirectoryEncoder`, i.e. inside the thing
`resolve_directory`'s validation exists to police. That is a real narrowing of
the engine's *independent* check and it should not be recorded as free.

It is still the right trade, and the reason is that the old form was
unsatisfiable truthfully: `getattrlistbulk` returns a record count, so the only
way to satisfy `return_value == total` was to tell `fts` it had received 248
entries. The byte accounting is also still checked, in the same expression: the
writes must fit `max_bytes`, and must be non-empty whenever an entry was
consumed.
Binding the declared result to `consumed` additionally ties it to the *paging*
(`directory_next` continues from `entries[consumed..]`), so an encoder that told
the tracee one count and paged past another is now refused where previously only
its byte total was examined. Two further checks were **added**: the encoder
re-walks its own finished buffer with `dirents::count_records` and refuses a
disagreement, and `count_records` refuses any reply that does not walk exactly to
its end.

This is the one place a ratified detail ("validation used as written") could not
hold literally. I judged correcting the identity strictly better than either
translating the value downstream (which broke `observe_result`'s
planned-vs-observed guarantee — measured, it did) or relaxing the check.

*Confidence: high* on the measurement; *medium-high* on it being the change the
human would have chosen. Flagged for `merge_gate`.

---

## 2. The standing condition: what I measured about the working directory

**The human required this to be measured before being relied on. It was, first,
before any code was written — and the audit's inference was wrong in a way that
matters.**

Method: `/bin/ls` is a SIP platform binary, so a debuggable twin was made exactly
as `cache::resign` does (copy, `codesign -f -s -` with
`crates/umbra-platform-macos/ent.plist`). `lldb` breakpoints with
`-C "frame info"` plus argument reads; `dtrace` is unavailable (SIP).

**Measured, across every argv shape — absolute operand, relative operand, `.`,
and no operand at all:**

| claim | audit said | measured |
|---|---|---|
| what `fchdir`'s operand is | "chdir into the directory being read" (§A.3 trace annotation) | **the directory the process is already in.** `fts` opens `"."`, then `fchdir`s back to *that* descriptor. It never chdirs into the directory it reads. |
| argv shapes | "byte-identical fts sequences" (§B.1) | **not identical.** An absolute operand produces 2 `fchdir`s; a relative operand produces 3, plus a second `open(".")` and a *relative* open of the target. |
| `chdir`(12), `getcwd`(326) | zero calls | **confirmed, zero calls** in every shape |

**Conclusion: the logical working directory never moves during an `ls`.** The
engine's `Fchdir` arm and the supervisor's `ChangedCwd` effect are, for `ls`, an
identity update.

**So why route `fchdir` at all?** Not for the directory — for the *descriptor*.
Once the opens are routed, `fchdir`'s operand is a virtual descriptor (≥ 4096);
unrouted it reaches a kernel that does not know the number and answers `EBADF`,
which `fts` reports as `fts_read:` and exits 1 on. The cwd update is implemented
as ratified because a routed call that reports success while silently declining
to do the thing it names would be a worse answer than the refusal it replaced.

**Escalation check (standing condition 2): no fork/exec cwd propagation is
required.** `/bin/ls` does not fork. Nothing in this slice widens the cwd model
across `fork`/`exec`, and `FsOp::Chdir`/`GetCwd` remain declared and inert — no
decode for 12 or 326, no handler, and that is now documented rather than left
looking implemented.

*Confidence: high.* Directly measured, repeatedly, four argv shapes.

---

## 3. The `getattrlistbulk` wire format — measured, not recalled

The largest and riskiest ratified item. It was **not** derived from `sys/attr.h`
or from memory of XNU. A C probe issued the exact request `fts` issues against a
real directory on macOS 26.5.1 arm64, and the reply bytes were dumped and
decoded; a name-length sweep (1..13 bytes) and an attribute-set sweep pinned the
packing and alignment rules.

Measured request (the **only** shape umbra serves):
`bitmapcount 5`, `commonattr 0x8200000b`
(`RETURNED_ATTRS|FILEID|OBJTYPE|DEVID|NAME`), `fileattr 0x00000001`
(`FILE_LINKCOUNT`), `options 8` (`FSOPT_PACK_INVAL_ATTRS`), 32 KiB buffer.

Measured record rules, all of which surprised at least once:

* attributes are packed **tight, with no internal alignment** — the `u64`
  `ATTR_CMN_FILEID` lands at offset `0x24`, which is 4-aligned and not 8-aligned,
  and the kernel inserts no pad;
* the **record** is padded to a multiple of **8** (47→48, 49→56, 57→64);
* `attr_dataoffset` is relative to the `attrreference_t` field's own address;
* **a directory record omits the file attribute group entirely** and says so in
  its returned-attributes bitmap — it is 8 bytes shorter, not zero-filled,
  despite `FSOPT_PACK_INVAL_ATTRS`.

A verbatim capture of the kernel's own reply is checked into
`umbra-platform/src/dirents.rs` as `RECORDED_REPLY`, and the unit tests compare
this encoder's output against it field for field (masking only `DEVID` and
`FILEID`, which umbra deliberately answers differently and asserts separately).
**The format is therefore checked against the kernel, not against a second copy
of the constants.**

*Confidence: high for the served shape; deliberately zero for any other.* An
attribute set outside the measured one is refused **by name** at the decode,
because the bitmap *is* the reply layout. This fired for real during
implementation: the test fixture's first version used `fts_children(tree, 0)`,
which requests `common=0x82079e0b file=0x0000022d`, and the refusal named exactly
that. That is `ls -l` territory and is not claimed.

---

## 4. Proving the production path is live, by execution

The brief flagged this as the defect class most likely to bite: implement an
encoder, inject it nowhere, watch every gate pass while production stays broken.

**It was proven by running it, not by testing around it.** Against the live
Ganesha fixture at `127.0.0.1:12105`, with the workspace seeded
`alpha.txt seed.txt subdir/`:

```
umbra: run ... prepared
alpha.txt
seed.txt
subdir
umbra: run ... finished: Some(Code(0)) after 1 process exits
```

**An important trap, hit and diagnosed:** the tracee inherits the platform
provider's stdout, and providers are spawned `Stdio::null()`
(`umbra-core/src/provider/transport.rs`). The first successful run therefore
showed **exit 0 and no output**, which looks exactly like success and is
indistinguishable from an empty listing. The listing above was obtained by
temporarily switching that spawn to `Stdio::inherit()`, observing, and reverting
— the revert is verified: `transport.rs` is not in the diff.

This is why the ratified substitution matters so much and why the tests assert
the way they do. Two independent failures during this slice produced **exit 0
with an empty listing**: once when `FdState::directory` was still a hardcoded
`false`, and once before the encoder was reached at all. Exit code cannot
discriminate a served directory read from a refused one *or* from an empty one.

### The five things that had to be fixed after the first "working" build

Recorded because each is a place the design was wrong and measurement caught it:

1. `FdState::directory` was hardcoded `false` (`engine.rs:3801`) — correct while
   a routed directory open was impossible. `fts` saves its cwd with a bare
   `open(".")` (no `O_DIRECTORY`, measured), so a flag-derived value answers
   `false` for a descriptor that is very much a directory, and `fchdir` refused
   it `ENOTDIR`. Now derived from the object's kind.
2. The return-value convention (§1.3).
3. `observe_result`'s planned-vs-observed check, broken by the first attempt at
   (2) — which is what showed that translating the value downstream was wrong.
4. The descriptor floor had to cover `ReadDir`/`Fchdir`/`Close`, or every
   `close` in the process would be resolved against the namespace — and
   rewrite-backed runs, which have no floor, would have broken.
5. The test fixture's `fts` flags (§3).

---

## 5. Per-utility confidence

| utility / mode | status | basis | confidence |
|---|---|---|---|
| `/bin/ls <dir>` on **nfs-userspace** | **works** — lists the shadow's merged entries, exit 0, host untouched | observed listing (§4); matrix row `PASS userspace /bin/ls <workspace>/`; entries proven through the NFS client by the `fts` fixture | **high** |
| `ls` entry *correctness* on nfs-userspace | **proven through the client** | `a_directory_listing_through_fts_reaches_the_tracee_over_the_userspace_client` drives the same `libsystem_c` `fts` with the same flags plain `ls` passes, writes what it read into the routed workspace, and the test reads it back over NFSv4 with nothing mounted | **high** |
| `/bin/ls` on `--local-dev` and kernel-`nfs`, **plain** | **unregressed** | `run_fixtures`: `PASS local /bin/ls <workspace>`, `PASS local /bin/ls absent` | **high** |
| `/bin/ls -l`/`-t`/`-i`/`-p`/`-S`/`-F`/`-s`/`-n` on `--local-dev` | **was REGRESSED; now unregressed** | This row previously claimed all four new calls were "protected structurally" by the absent `descriptor_floor`. **That was false**, and round-1 review measured it: the attribute-set refusal lived in the ABI *decode*, which runs before the fence, so those eight flags went from exit 0 to a stopped run on registries that route nothing. Fixed by moving the refusal behind the fence, and re-measured side by side against a fresh `a85a8471` build: all twelve flag shapes now agree with master. Two rows (`-l`, `-t`) added to the rewrite-backed matrix, with a negative control confirming they fail when the ordering defect is reintroduced | **high** |
| `/bin/cat`, `/bin/mkdir`, `/usr/bin/touch` on nfs-userspace | **unregressed** | all matrix rows PASS; full `userspace_run` suite 22/22 (master: 20/20, +2 new) | **high** |
| `ls` on a **large** directory (multi-page) | **proven live** by round-1 review | 300 entries, 152-byte records, 45 600 bytes against a 32 768-byte buffer: two data pages plus the zero-return page, all 300 names returned, `md5` of the listing read out of the export identical to the host directory's | **high** (was medium) |
| `ls` on an **empty** directory | **proven live** by round-1 review | zero entries, no `fts` error, fixture and `/bin/ls` both exit 0 | **high** (was medium-high) |
| A **second** enumeration of one directory in one process | **was SILENTLY WRONG; now correct** | `Overlay::directories` was never evicted and descriptor numbers are reused, so a completed enumeration left an *empty remainder* under a key the next open reproduced exactly: three names, then nothing, exit 0. Exhibited before the fix and after (both passes now list correctly). The partial-read variant is fixed by the same eviction | **high** |
| `getattrlistbulk` on a descriptor umbra never issued | **was run-stopping; now `EBADF`** | `resolve_directory` resolved its own descriptor with a raw `StaleHandle`. Exhibited: `fstat`/`fchdir`/`close` answered `EBADF` and the run finished `Code(0)` while `getattrlistbulk` ended it. Now routed through `routed_binding` like its three siblings; all four answer `EBADF` and the run finishes | **high** |
| `ls -l`, `-la`, `-@`, and every metadata-requesting mode | **not claimed, fail-closed** | refused by name with the requested bitmap in the message, `ENOTSUP` bound to the tracee so the run survives; observed firing for the wide `fts_children` set | **high** that they are refused; they are *not* supported |
| `ls -R`, and recursive `fts` | **not claimed, and NOT fail-closed** | separated from the row above during round 3, because sharing its adjective was false. Recursion is neither implemented nor refused: when `fts` declines to descend it issues no syscall, so there is nothing to refuse it *with*. Measured, it appears to work and matches the host — and is still unclaimed and untested. The `nfs-userspace` README carries the full disposition | **high** that it is unclaimed; **not** a fail-closed claim |
| `chdir`(12), `getcwd`(326) | **declared and inert, documented as such** | zero measured callers; no decode | **high** |
| `fork`/`exec` cwd propagation | **out of scope, not needed** | `ls` does not fork; cwd does not move at all (§2) | **high** |

### What I measured versus what I inferred

**Measured (ran it, on this host, this session):** every `fts` syscall and its
arguments across four argv shapes; the `fchdir` operands and the resulting cwd
conclusion; the `getattrlistbulk` request bitmap and options; the kernel's reply
bytes, packing, alignment and the directory-record omission; `ls` end-to-end on
nfs-userspace including its actual printed listing; `ls` on the rewrite-backed
registry; the full `userspace_run` suite before and after; the `readdir` probe
with its negative control; the provider's `Stdio::null()` as the cause of the
empty stdout; the open flags `fts` uses for the dirfd and for `"."`.

**Inferred, not measured:** that `close`(6) is genuinely needed alongside
`__close_nocancel`(399)
— routing it is ratified and consistent, and the launch-time `svc` verification
confirms the symbol and number, but I did not observe a virtual descriptor being
closed through `close`(6) specifically; that no *other* program's wider
`getattrlistbulk` request will now reach a refusal it previously never got to
(the `TRACED_STUBS` doc records this hazard class — routing one member of a
refused set makes the next reachable — and I did not sweep programs beyond the
utility matrix).

**Corrected after round-1 review:** this section's "protected structurally"
claim, §1.3's "nothing was given up", §6's "the one edit", §7's master baseline,
and `dirents.rs`'s `PACKING` offset. Each is corrected in place above rather than
appended to, and `fix-r1.md` records what changed and why.

**Corrected in the audit:** §A.3's `fchdir` annotation and §B.1's
"byte-identical across argv shapes" (§2). §D's account of `resolve_directory`
omits the return-value conflict (§1.3). §G's G-ii is not constructible as
written (§1.1).

---

## 6. Scope, guarantees and the test-body edits

**Read `fix-r1.md`, `fix-r2.md` and `fix-r3.md` beside this section.** This document describes
the slice as first implemented; two review rounds have amended it since, and the
two fix reports are authoritative where they differ. §1 records **four**
deviation-grade items now, not three: the three below plus the
`SyscallAbi::directory_request` method that round 1's H1 fix added (`fix-r1.md`
§H1) — a new ABI method, which is worth a human's eye because the design gate
used that phrase for the option it rejected. This section's "Preserved"
paragraph is likewise incomplete on its own: since it was written a validation
**moved across the descriptor fence** (the most scope-relevant mechanism change
of either pass) and four more dispositions were added. `fix-r1.md` §H1, `fix-r2.md` §N1 and
`fix-r3.md` §1 carry all of it — the last of those holding the enumeration of
every tracee-supplied input of a directory read and where each one terminates,
which is the check this section's guarantees paragraph cannot express on its
own.

**In scope and done:** all eight ratified change items; G-ii production wiring;
the `ls` matrix row and an **entry-based** mutation probe; the three doc
corrections (waiver granted); the unsupported-modes documentation; tracking issue
[#125](https://github.com/invakid404/umbra/issues/125).

**Preserved:** every setup / recovery / cleanup / validation guarantee. No
validation was removed — one was corrected (§1.3) and three were added
(`count_records`'s exact walk, the encoder's self-check, the `io_buffer`
empty-buffer guard). Every #55–#120 mechanism is intact: `TRACED_STUBS`'s single
list with `Delivery` exhaustiveness (which caught an unhandled `RoutedEffect`
during this work, as designed), the descriptor fence and its C/Rust twin, the
`Err`-vs-`Emulate` rule at the routed-open refusals, `replay_must_poison`, the
probe discipline with negative controls. Audit records untouched.

**Three edits to existing `#[test]` bodies, all flagged.** This count has been
wrong twice. The first version said "the one edit"; round-1 scope review found a
second by sweeping. The corrected version said "two"; round-2 scope review found
a third the same way. Both times the edit itself was benign and both times the
*self-report* was the failure — in the one guardrail a human can enforce only
through it. The sweep that finds these is `#[test]`-body diff against
`master@origin`, and it is the check to run rather than this paragraph.

**The same discipline applies to every quantitative claim in these records, and
it had to be extended once more.** Four self-reported numbers in this slice have
failed to reconcile: two test-edit counts, the `822` master baseline (measured:
`813`), and round 3's `+4, no test removed` net test delta (measured: `+3`, one
superseded). Each was caught by someone re-deriving it rather than by the
paragraph claiming it. So: **counts are quoted from the command that produced
them, never from memory of what a pass did.** For the test delta that command is
`grep -rn '#\[test\]' crates/ --include='*.rs' | wc -l`, run at both revisions
— 908 here against 887 at `master@a85a8471`.

*First:* the new probe is added to
`every_mutation_probe_is_wired_into_the_userspace_job`'s list.
That test iterates a hardcoded `[(probe, package)]` set and exists precisely to
catch a probe that is written but never run in CI; adding a feature without
adding the row would leave the new probe unguarded while the test still passed —
the exact defect it was built for. The edit is additive and strengthens the
guard. I am naming it rather than relying on it being read as maintenance.
`ci.yml` gains the matching `Proof 7` step and the fixture build.

*Second:* `abi.rs`'s `process_control_stubs_are_not_delivered_to_the_namespace`
gained `461, 13, 399, 6` in its hardcoded array of numbers asserted
`Delivery::Namespace`. Same shape as the first -- four entries appended to an
assertion list, nothing removed, nothing loosened -- and *voluntary*: the test
iterates only the numbers it lists, so it would have passed unchanged. It is
strengthening rather than forced, which makes it easier to justify and does not
excuse having left it out of this list.

*Third:* `abi.rs`'s `the_interposers_descriptor_test_is_the_one_the_supervisor_applies`
— the C/Rust twin test — had its **failure message** changed, from naming
`FsOp::Fstat` to naming `Fstat | ReadDir | Fchdir | Close`. The `assert_eq!`'s
expected value is byte-identical and the guarantee is untouched; only the text a
failure would print changed. It exists solely because round-1 scope review asked
for it (S5 item 3), the message having become an under-description of what the
test protects once the fence covered four calls.


**Out of scope, not absorbed:** `ls -l`/`-la`, `-@`/ACLs, `-R`,
symlink-following in directory reads, xattrs in `getattrlistbulk`,
`getdirentriesattr`(222), `getdirentries`(196)/(344),
`opendir`/`readdir`/`closedir`, `chdir`(12), `getcwd`(326), any fork/exec cwd
propagation.

---

## 7. Gates

**Every total below is bound to the commit that produced it, and the canonical
table is §7.1.** A figure in this section describes the round-1 head and nothing
later; §7.1 is where to check the current one. This was the fifth
quantitative-reconciliation item in the slice and it is the one that named the
underlying property: *a count without its commit is a claim that cannot be
checked* — not wrong, just uncheckable, which is worse, because a reader cannot
tell a stale figure from a current one.

| gate | result, at `7bd1a95c` (fix round 1) |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace --all-targets` | **826 passed**, 3 ignored (master `a85a8471`: **813**, measured). **Superseded** — 832 at the current head; see §7.1 |
| `userspace_run` vs live Ganesha | **22 passed** (master: 20; +2 new) |
| `readdir` probe, mutated | passes |
| `readdir` probe, **negative control** (unmutated) | **fails**, as required |
| `run_fixtures` rewrite-backed matrix | `/bin/ls` both rows PASS |

### 7.1 Every reported total, bound to its commit

The canonical record. Each row is one head of change `psvqmvlmktvv`; the review
and CI rounds each saw the head named in "pass".

| commit | pass | `cargo test --workspace --all-targets` | `#[test]` attrs | fixture env set? |
|---|---|---|---|---|
| `a85a8471` | master (parent) | **813** passed, 3 ignored | 887 | n/a |
| `9f94b8f3` | implement | **825** passed | 901 | no |
| `7bd1a95c` | fix round 1 (`fix-r1.md`) | **826** passed | 902 | no |
| `20ccf7db` | fix round 2 (`fix-r2.md`) | **829** passed | 905 | no |
| `3480713b` | fix round 3 (`fix-r3.md`) | **832** passed | 908 | no |
| `e6618397` | PR #128 opened; CI round 1 **red** | 832 | 908 | no |
| `6a18b98f` | CI/CR fix round 1 (`ci-fix-r1.md`) | **832** passed | 908 | **yes** |
| `52fba1e1` | memoria re-ack; CI round 2 **green** | 832 | 908 | yes |
| **this head** | CI/CR fix round 2 (`ci-fix-r2.md`) | **832** passed, 3 ignored | 908 | **yes** |

**What is measured now versus reported then.** The `#[test]` attribute column is
re-derived at *this* head for every row, by diffing each commit against master
over Rust sources only:

```
$ jj diff --from a85a8471 --to <commit> --git 'glob:crates/**/*.rs'     | grep -cE '^\+\s*#\[test\]'
```

The passed totals are contemporaneous — they cannot be re-run at a hidden commit
without checking it out — but every one of them **reconciles independently**
against that re-derived count:

```
reported = 813 + (attrs - 887) - 2
  901 -> 825 ✓   902 -> 826 ✓   905 -> 829 ✓   908 -> 832 ✓
```

The `- 2` is the two tests in `userspace_run.rs`, which sits behind
`#![cfg(all(feature = "transport-raw", target_os = "macos", target_arch =
"aarch64"))]` and so compiles to nothing under a default-feature workspace run.

**Two rows carry a condition, not just a number.** Every total up to `3480713b`
was measured **without** `UMBRA_TEST_FIXTURE_PATH`, which means the eleven
`umbra-platform-macos` fixture cases reported `ok` without executing — the count
is identical either way, so nothing in the numbers is wrong, but the runs behind
them were weaker than they looked. CI round 1 is what exposed that; `ci-fix-r1.md`
§2 has the measurement. From `6a18b98f` onward the env is set, which is why the
same 832 takes 53s instead of 27s.

One clippy suggestion was **not** taken as offered: it proposed replacing
`length == 0 || length % 8 != 0` with `!length.is_multiple_of(8)`. Zero *is* a
multiple of eight, so that would have made `count_records` spin forever on a
zero-length record. The zero check is kept, with a comment saying why.

**Not run here:** CI, and the two reviews. `PAUSE BEFORE MERGING` is respected —
nothing is merged and no bookmark is pushed.
