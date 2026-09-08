# Track C — Seatbelt and LLDB, M0

The profile enforces the tested local-write denial, but **Gate 3 is not passed**:
no test reached a breakpoint in the resigned `ls` twin. The unsandboxed control
also stalled in task-port acquisition. These results do not establish that
Seatbelt composition is impossible on this OS; they do prevent claiming it works.

**2026-09-09 update.** Composition was subsequently established by a different
route and is not contradicted by the LLDB results above, which stand as recorded.
The Rust tracer drives `debugserver` over RSP rather than LLDB, and
`crates/umbra-platform-macos/tests/sandbox_launch.rs` measures a traced
`sandbox-exec` bootstrap handing off at the target's exec stop with the policy in
force: an unrewritten write outside the run root fails `EPERM`, while the same
fixture with interception active captures into the shadow. The profile below is
now a template that the supervisor embeds and renders per run, so the scripts
here take an explicitly rendered profile.

## Environment and reproduction

Tested 2026-09-07 on macOS 26.5.1 (25F80), Apple Silicon, SIP enabled,
LLDB 2100.0.17.108 / Apple Swift 6.3.2 from the existing Command Line Tools.
`/bin/ls` selects the arm64e slice. Full environment, mount table, signatures,
and applied entitlements are in [results/setup.log](results/setup.log).

`umbra.sb` is now a **template**, not a runnable profile: it carries one token,
`{{UMBRA_RUN_ROOT}}`, standing for a complete quoted Seatbelt string literal that
names one run's own root. The supervisor embeds this same file and renders it at
runtime; these scripts must be given an already-rendered profile so they test the
policy that actually runs. There is no built-in mount path and no fallback, and a
missing profile is an error rather than a skip.

Run from an ordinary terminal outside an existing Seatbelt sandbox:

```sh
./render-profile.py /absolute/existing/run/root /tmp/umbra-rendered.sb
UMBRA_PROFILE=/tmp/umbra-rendered.sb UMBRA_WRITE_ROOT=/absolute/existing/run/root \
    ./verify-deny.sh
UMBRA_PROFILE=/tmp/umbra-rendered.sb ./verify-lldb-composition.sh
```

**Pass the resolved path.** Measured on macOS 26.5.1: Seatbelt matches `subpath`
against the path the kernel resolves, so a profile granting `/tmp/<run>/root`
denies writes there, because `/tmp` is a symlink to `/private/tmp`. Rendering
`/private/tmp/<run>/root` passes. The supervisor resolves the run root during
preparation for this reason; `verify-deny.sh` checks that the profile it was
handed actually grants the root it was told about, so a mismatch fails as a
configuration error instead of looking like a denial.

`verify-deny.sh` reports four verdicts: the sandbox starts and can read, a `/tmp`
write is denied, a write inside the granted run root succeeds, and a write to a
*sibling* directory next to that root is denied — granting one run's root must
never grant the store containing it.

The agent's enclosing workspace sandbox initially produced
`sandbox-exec: sandbox_apply: Operation not permitted`. The recorded verification
runs were performed outside that enclosing sandbox. A sandbox-launch failure
does not count as a successful write denial.

No software was installed, system security settings were changed, or files in
`~/Coding/umbra` were modified. The historical run used the then-pinned
`/mnt/umbra-nfs` literal, which was absent throughout verification; no local
stand-in or sudo mkdir was used, so its allowed-write check was skipped rather
than passed. That literal is gone: the allowed-write check now targets whichever
run root the caller renders, so it is required input and its absence is a FAIL. If the directory exists
but is not writable, the script reports FAIL, rather than hiding that failure as
a skip. If a local stand-in is supplied, success proves only path policy, not NFS
operation or durability; inspect the recorded mount table.

## Profile

[umbra.sb](umbra.sb) starts with `(version 1)` and `(deny default)`:

| Operation | Allowance |
| --- | --- |
| File reads | `file-read*` globally, with no exclusions; host Unix permissions and other security policies still apply |
| Persistent filesystem writes | `file-write*` only under the single rendered run root |
| Runtime device writes | `file-write-data` only to literal `/dev/null` |
| Process launch | `process-fork`, `process-exec` |
| Signals | Self and children |
| System information | `sysctl-read` |
| Mach lookups | Seven named basic system services, listed explicitly in the profile |
| Debugger task ports | `mach-priv-task-port` only for `target same-sandbox` |
| Network | `network*`, permissive for M0 agent APIs, downloads, and debugger transport |
| Everything else | Denied by default |

The `/dev/null` carve-out is a character-device data sink, not persistent storage.
It is required by LLDB's `target.disable-stdio` launch path on this machine:
without it, launch fails with `Operation not permitted` and a corresponding
`file-write-data /dev/null` denial. The rule grants no create, unlink, or metadata
mutation rights there. No `/tmp`, `/private/var/folders`, home-directory, cache,
PTY, or `/dev/dtracehelper` write allowance was added. Tools that need writable
runtime directories must use paths under the rendered run root; broad tool
usability is not established by these probes.

The task-port rule addresses an observed
`mach-priv-task-port same-sandbox [ls-c-test(...)]` denial. LLDB launches the twin
and `debugserver` as siblings, so a children-only rule is insufficient. This
rule grants no task-port access to host processes outside the sandbox and does
not replace signing/AMFI checks. Apple documents the separate relationship
between debugger and get-task-allow entitlements in its
[debugging entitlement reference](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.cs.debugger).

Network access and the named system-service IPC mean this is an M0 filesystem
policy, not a complete boundary against exfiltration or service-mediated effects.
The eventual trusted supervisor should live outside the tracee sandbox. The
same-sandbox debugger rule is for testing the requested opposite nesting order
and should be revisited for that architecture.

## Results

| Script | Expectation/case | Result |
| --- | --- | --- |
| `verify-deny.sh` | Sandbox starts; `/bin/ls` is readable | PASS |
| `verify-deny.sh` | `touch /tmp/umbra-should-be-denied` denied | PASS — `Operation not permitted` |
| `verify-deny.sh` | `touch /mnt/umbra-nfs/umbra-should-be-allowed` allowed | SKIP — mount not up; path absent |
| `verify-lldb-composition.sh` | Unsandboxed twin, LC_MAIN breakpoint | FAIL — 20-second timeout |
| `verify-lldb-composition.sh` | LLDB outside, original sandbox launcher, `b main; run` | FAIL — attach not allowed |
| `verify-lldb-composition.sh` | Sandbox outside, LLDB inside, `b main; run` | FAIL — shell expansion not permitted |
| `verify-lldb-composition.sh` | LLDB outside, resigned sandbox launcher, LC_MAIN breakpoint | FAIL — 20-second timeout |
| `verify-lldb-composition.sh` | Sandbox outside, LLDB inside, LC_MAIN breakpoint | FAIL — 20-second timeout |

The final script matrices are [verify-deny.log](results/verify-deny.log) and
[verify-lldb-composition.log](results/verify-lldb-composition.log). Each LLDB case
has its complete command, output, exit status, timeout status, and process IDs in
`results/<case>.log`. Earlier `*-raw` and `*-diagnostic` logs are exploratory
results; the final matrix uses the five cases listed above.

The deny test checks startup independently, establishes that the exact `/tmp`
fixture is writable without this policy, and requires an in-sandbox marker plus
a permission error and absence of the file. It refuses preexisting fixtures.
The positive test checks creation, existence, and cleanup. Script exit status is
0 when all executed expectations pass; the NFS skip remains visible in output.

The composition shell script invokes [lldb_composition.py](lldb_composition.py),
which copies `/bin/ls` to `/tmp/umbra-m0/twins/ls-c-test`, resigns it using the
Gate 1 entitlement plist and exact signing flags, and verifies the signature.
It also creates `/tmp/umbra-m0/twins/sandbox-exec-c-test` for the adapted order.
These are test copies; original Apple binaries are untouched. The helper assumes
this machine's arm64e `ls` slice and reads its Mach-O metadata with `otool`.

Neither Apple executable exposes a `main` symbol here. The literal commands are
still attempted, but a pending `b main` cannot establish success. Adapted cases
derive `ls`'s LC_MAIN file address (`0x100000960` on this build) and set a
module-scoped address breakpoint that can resolve after exec and ASLR. They
disable shell expansion, terminal allocation, and stopping merely on exec.
LLDB's local `help breakpoint set` documents the pending module/address behavior;
[LLDB's command reference](https://lldb.llvm.org/man/lldb.html) describes batch
command execution and initialization handling. User `.lldbinit` files are disabled.

PASS requires LLDB to report a real breakpoint stop, the frame's module to be
the exact twin, and its file PC to equal LC_MAIN. A live debugserver, launch stop,
wrapper entry point, missing error, or timeout never passes. Each invocation is
bounded to 20 seconds and the harness tracks and kills its own descendants,
including debugserver's separate session. It does not kill other tracks' debuggers.
Exit status is 1 unless both adapted nesting orders pass; literal diagnostic
failures remain in the matrix even if future adapted tests succeed.

## Exact failures and what needs to change

The original launcher fails before it can execute the resigned twin:

```text
error: process exited with status -1 (attach failed (Not allowed to attach to process.  Look in the console messages (Console.app), near the debugserver entries, when the attach failed.  The subsystem that denied the attach permission will likely have logged an informative message about why it was denied.))
```

Resigning only `ls` cannot fix attachment to the first executable,
`/usr/bin/sandbox-exec`. The adapted test resigns that launcher too, using
`codesign -f -s - --entitlements <Gate-1-ent.plist>
--preserve-metadata=identifier,flags,runtime <copy>`. The immediate attach error
then becomes a timeout, which is not proof of successful attachment.

The literal sandbox-outside order reports:

```text
error: shell expansion failed (reason: Operation not permitted). consider launching with 'process launch'.
```

`process launch -X false` avoids that dependency. With terminal allocation
disabled, the next measured blocker was `/dev/null`; allowing its data writes
exposed the same-sandbox task-port denial. The final profile includes both
measured rules. Evidence before and after is in
[sandbox-outside-violations.log](results/sandbox-outside-violations.log),
[sandbox-outside-null-violations.log](results/sandbox-outside-null-violations.log),
and [sandbox-outside-task-port-violations.log](results/sandbox-outside-task-port-violations.log).
The last capture no longer reports the task-port denial, but the launch still
times out. Other denials (Open Directory, logging, notification shared memory,
and `/dev/dtracehelper`) are preserved there; their causal relevance to the
remaining stall is unproven, so they were not broadly allowed.

The unsandboxed diagnostic's last debugserver line is:

```text
[LaunchAttach] (83450) about to task_for_pid(83449)
```

[debugserver-baseline.log](results/debugserver-baseline.log) ends there;
[gdb-packets.log](results/gdb-packets.log) shows LLDB waiting after `vAttach`.
This is a timed-out task-port request, not an observed successful acquisition.
The Gate 1 note's live-debugserver heuristic therefore cannot resolve this gate.
Next, establish an actual LC_MAIN breakpoint in the unsandboxed control from an
interactive, debugger-authorized terminal and inspect taskgated/Developer Tools
authorization if it still stalls. The cause of this host/session stall has not
been established; no entitlement change or OS security toggle is claimed to fix
it. Then rerun both adapted orders and inspect any remaining per-process Seatbelt
denials. Prefer a trusted supervisor outside the sandbox once that control works.

## M3 TODOs

- Pin an OS/toolchain matrix and establish a supported deployment mechanism for
  the deprecated/private Seatbelt interface. Fail startup if policy application
  fails; never silently run unsandboxed.
- Require a verified live NFS mount, restrict policy to one session's run root,
  and fail closed on missing mounts, replacement, disconnection, and remount.
  A path-only allow rule cannot attest that the directory is remote storage.
- Audit mutation coverage: rename, unlink, metadata, xattrs, hard links, symlinks,
  directory descriptors, writable mappings, inherited writable descriptors,
  descriptor passing, and writes through devices or IPC services. Add an
  independent leakage oracle and adversarial path/race tests.
- Validate fork/spawn/exec/grandchildren and crash paths with Gate 2; remove
  unnecessary debug privileges and prevent a tracee from controlling the trusted
  supervisor. Arrange safe inherited stdio and NFS-backed runtime endpoints.
- Exercise real Codex/Claude, shells, Git, toolchains, SDKs, Unix sockets, and
  their own nested sandboxes. Global reads do not prove complete usability.
- Reduce network/Mach permissions using measured workloads; account for
  service-mediated writes and authentication without persisting host credentials.
- Test debugger ownership conflicts with crash reporters, profilers, other
  debuggers, and anti-debugging behavior. Unknown or untraceable children must
  remain stopped or be terminated.

Gate 3 looks plausible as an M0 path but remains unverified on this OS/session.
Actual task control and a live-NFS allowed-write result are still required.
