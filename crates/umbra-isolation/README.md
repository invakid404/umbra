# umbra-isolation

Run isolation for `umbra run` when it starts in launchd's session (session id 1),
issue #193. **Current state:** shipped on macOS and active only there, only in
session 1. In every other session, every terminal session included, it does
nothing, and on other platforms it compiles to no-ops. **Scope:** this crate's
`src/` and `tests/`. The `umbra-cli` launch path that calls it is described in
[the CLI README](../umbra-cli/README.md#exit-codes-and-streams), and how
debugserver kills a traced process on a lost connection in
[the platform README](../umbra-platform-macos/README.md).

This is the one umbra crate in the `umbra` binary's tree that permits `unsafe`
(third-party dependencies such as `libc` aside). Every
`unsafe` block carries a `// SAFETY:` comment, and `unsafe_op_in_unsafe_fn` is
denied. `umbra-cli` keeps `#![forbid(unsafe_code)]` and calls two safe functions:
`guardian_entry()` first in `main`, and `isolate_run()` before a run.

## The problem

A launchd job, or CI's self-hosted runner, starts `umbra` in session 1. The run's
processes then share a session with the caller's process group. Some of their
exits are not counted against that group's job-control count (XNU's uncounted
reparent to launchd, #186). A count that reaches 0 with a stopped member in the
group makes the kernel send SIGHUP and SIGCONT to the whole group. Measured on
master (7f551ba0), each of the following happened in session 1:

- P1: a stopped tracer at the run's deadline SIGHUPed `umbra`, its providers and
  the caller's group;
- P2: a tracer killed while its tracee's exit was pending did the same;
- P3: a killed platform provider drove the caller's group count to 0, an
  orphaned group;
- P4: a stopped provider, then the supervisor killed, SIGHUPed the providers and
  the caller's group.

## Topology

In session 1, `isolate_run()` does not return. The process the caller started
stays where it is, in the caller's group, as the **relay**. It spawns a
**guardian** into a new session S (`POSIX_SPAWN_SETSID`, so S is the guardian's
pid). The guardian spawns the **supervisor**, which is the same `umbra` binary
with the same arguments and the caller's environment (less the guardian's own
descriptors variable). Everything the supervisor starts (providers,
debugservers, the tracee) is in S. No exit in S can count against a group in
session 1.

```mermaid
flowchart LR
    caller["caller's group (session 1)"] --> relay
    relay -- "POSIX_SPAWN_SETSID" --> guardian["guardian (leads session S)"]
    guardian --> supervisor["supervisor: umbra run, same argv"]
    supervisor --> providers["providers, debugservers, tracee (all in S)"]
```

The relay and the guardian talk over three pipes: guardian to relay, relay to
guardian, and a life pipe the relay never writes. The life pipe's EOF tells the
guardian the relay is gone. Messages are newline-terminated ASCII frames of at
most 1024 bytes, parsed strictly (`src/frame.rs`):

1. `ISOLATED <S>`: the guardian leads session S (its own pid; checked by the relay).
2. `GO <hex path>`: the relay has renamed the record to carry S (below).
3. `STARTED <pid>`: the supervisor was spawned.
4. `OUTCOME <wait status> <COMPLETE|INCOMPLETE|ERROR|UNVERIFIABLE> <record errno>`.
5. `FAIL <reason>`: startup failed.

A malformed or oversized frame, EOF, or a read error while the guardian is
still running is a lost peer, and the relay fails closed (below). A frame split
across read timeouts is kept and completed.

## Cleanup and ownership

A sweep kills every process of S by **audit token**, never by pid
(`proc_signal_with_audittoken`, `src/census.rs`). Each token is read from a task
name port before and after `getsid`. A process that exits and has its pid reused
fails the kernel's pid-version check, so the kill is refused as stale.

Who sweeps:

- **Supervisor exit:** the guardian is the only process that waits on the
  supervisor. It sweeps S and then sends `OUTCOME`. While the guardian runs, S
  exists (its leader is a member), so the guardian needs no further proof.
- **Relay gone** (life-pipe EOF): the guardian sweeps S, reaps the supervisor
  within 1 s, records `relay-death` and exits.
- **Relay fallback** (the guardian stopped, lost, malformed or slow): the relay
  SIGKILLs the guardian by pid, but only while it is the relay's own unreaped
  child. It then sweeps S, writes its `relay-fallback` line, reaps the
  guardian once, records the reap in a `guardian-reaped` line and exits 1.

The relay's sweep rests on the **zombie anchor**. The relay reaps the guardian
only after its sweep and that sweep's `relay-fallback` (or, on the normal path,
`outcome`) record line; the `guardian-reaped` line follows the reap. While the
guardian is
unreaped, its pid cannot be allocated again. A session id is its leader's pid.
So no other process can create a session with id S, and any process with
session id S belongs to this run. The XNU sources (xnu-12377.121.6), also
recorded in the #193 arc's T1 protocol (an external arc record, graph audit
`dg-56gcxqyw`, not in this repository):

- pid allocation skips pids still hashed, zombies included
  (`kern_fork.c:971-974`);
- `setsid` makes the caller a session leader (`kern_prot.c:498`,
  `setsid_internal` → `enterpgrp`), and `session_alloc` sets the session id
  to the leader's pid (`kern_proc.c:2801-2808`);
- the pid is released only at reap (`kern_exit.c:2981-2984`).

As defence in depth (`src/sweep.rs`):

- a process found in S is enrolled only in a census pass that both begins and
  ends with the anchor verified;
- from the first failed check, the owned set is frozen;
- any other process with session id S is reported `UNVERIFIABLE` and never
  signalled.

After the single reap, the relay performs no operation on a numeric pid or
session id (R-10): only the `guardian-reaped` record line, stderr, and its own
exit or mirrored signal follow it (`relay.rs`, `Relay::fallback`, end of
`run_with`).

The isolation does not reach a process that leaves S with its own `setsid()`. A
traced process that does so is still killed by its debugserver if the
connection to the platform is lost while it runs: `umbra-platform-macos` sends
`QSetDetachOnError:0` on every connection before attaching, and fails the
connection unless debugserver answers `OK` (platform README). If its tracer
dies, the kernel kills it (`kern_exit.c:2556-2608`). An untraced process that
escapes S is not cleaned up.

## Exit status

When the run completes, the relay mirrors the supervisor's wait status: the same
exit code, or the same signal, re-raised. `umbra`'s own exit-code rules are
unchanged ([CLI README](../umbra-cli/README.md#exit-codes-and-streams)). The
relay exits 1 instead, with a line on stderr, in these cases:

1. the sweep ended other than `COMPLETE`:
   `umbra: run isolation cleanup <KIND> (session S); supervisor status <N>`;
2. any record write or `fsync` failed, in the relay or the guardian:
   `umbra: isolation record write failed (guardian errno …, relay errno …); supervisor status <N>`;
3. the relay failed closed: `umbra: run isolation failed: <reason> (session S, relay sweep <KIND>)`;
4. the isolation could not start, and nothing was launched:
   `umbra: run isolation failed: <reason>`.

A terminating signal sent to the relay (a `launchctl remove`, for example) is
not a graceful stop. The relay installs no signal handlers, so it dies of the
signal, and its status is that signal. The guardian sees the relay gone, sweeps
S with SIGKILL and records it. Neither the relay nor the guardian handles SIGHUP.

## The isolation record

Every isolated run writes a JSON-lines record to
`~/Library/Logs/umbra/isolation/`. The home directory comes from the password
database (`getpwuid_r(getuid())`), not `$HOME`, so launchd jobs without `HOME`
are covered.

- **R-1:** the relay creates the record, as
  `isolation-<32 hex>.jsonl` with a random id from `getentropy`, and writes and
  `fsync`s its first line before anything is spawned. If it cannot, `umbra`
  exits 1 and launches nothing. Once the guardian reports S, the relay renames
  the record to `isolation-<32 hex>-s<S>.jsonl`. Session ids recur across runs,
  so S alone never names a record.
- **R-2:** each line is one checked JSON object, written with one `write` whose
  full length is verified, then `fsync` (not `F_FULLFSYNC`).
- **R-3:** a failed write or `fsync` is sticky in the process that saw it. The
  guardian forwards its errno in `OUTCOME`. The run itself is not killed for a
  reporting failure; only its reported status becomes 1.
- **R-4:** if the relay is dead **and** the record is unwritable, the guardian's
  inherited stderr is the only report:
  `umbra: run guardian: relay gone, cleanup <KIND>, isolation record write failed (errno N)`.
  This is a limit of the design, not something it hides.
- **Retention:** at relay start, only the newest 50 records are kept. Only
  regular files in that directory whose names match the record pattern exactly
  are deleted.

The record is not linked to the run's journal.

## Bounds

The bounds are operational targets on the monotonic clock, not hard real-time
guarantees (#193 O7). Each wait below is a `WNOHANG` or `poll` loop against a
deadline:

| Wait | Bound |
|---|---|
| each startup handshake phase (`ISOLATED`, then `STARTED`) | 2 s |
| a sweep (census passes, kills, pauses) | 3 s, plus the overrun below |
| the guardian's reap of the supervisor after relay death | 1 s |
| the guardian's exit after its `OUTCOME` | 1 s |
| the relay's wait for a SIGKILLed guardian to exit | 1 s |
| the relay's reap of the guardian | 1 s |

A sweep checks its deadline in two places: at the top of each pass, and before
each process of a census (`sweep.rs`, `census.rs`). Each of these steps runs to
completion once it has started:

- **the two anchor checks** of each pass (`anchor_ok`), one before the census
  and one after it. In the relay's fallback sweep each is one
  `waitid(…WNOWAIT…)` call on the guardian; the guardian's own sweep answers
  them without a call;
- **the process listing** at the start of each census: up to eight
  `proc_listallpids` calls (`all_pids`);
- **one process's census step:** `getsid`, `proc_pidinfo`, then either
  `task_name_for_pid`, two `task_info` reads around a second `getsid`,
  `mach_port_deallocate` and sometimes a third `getsid` (`verified_token`), or,
  when `proc_pidinfo` gives no answer, one more `getsid` (`MacHost::census`);
- **the kill loop:** one `proc_signal_with_audittoken` per owned process still
  alive, with no deadline check inside it;
- **the 20 ms pause** after each pass, including a pass whose census failed.

A sweep can therefore end later than its 3 s. At most, the overrun is the
composition of: the step in progress at the deadline, then the post-census
anchor check, then that pass's kill loop, then one 20 ms pause. A census cut at
the deadline counts as failed, never as empty, and so does a process whose
`proc_pidinfo` gives no answer but whose `getsid` still names S: it is reported
as a claimant without a token, which is never signalled. Only an empty census
ends a sweep `COMPLETE`; otherwise the result is `UNVERIFIABLE` or
`INCOMPLETE` from the last complete census, or `ERROR` if none completed.

The relay's reap of the guardian is bounded too. If a SIGKILLed guardian has
not been reaped within 1 s, for example because it is still inside an
uninterruptible call, the relay exits 1 without reaping it, saying so on stderr
and in the record's `guardian-reaped` line (`"reaped": false`), and launchd
inherits it. The reap still comes after the sweep and its `relay-fallback` or
`outcome` line, and nothing after it signals or censuses by number.

Outside every bound:

- **Record I/O.** Each record line is a blocking `write` plus `fsync` to a local
  disk. It happens outside the sweep: after it in the guardian and in the
  relay's fallback, and around the handshake in the relay. A stalled disk
  delays the report and the exit, never a sweep's own deadline checks.
- **Scheduling delay** of the relay or the guardian.
- **Pipe writes.** Over a whole run, the guardian writes at most three frames
  (`ISOLATED`, `STARTED`, then `OUTCOME` or `FAIL`), and the relay writes one
  (`GO`). Each frame is at most 1025 bytes, well under a pipe's 16 KiB buffer,
  so no write can block even if the reader never reads.
- **The run's own duration**, unbounded as before: the relay waits for the
  guardian for as long as the supervisor runs.

## Spawning and signals

No process here ever calls `fork()`. The relay and the guardian start their
children with `posix_spawn` of the `umbra` executable itself (`src/spawn.rs`),
so no child ever runs in a forked copy of a multi-threaded process.

- **Descriptors:** the three pipe ends a child needs are passed with
  `posix_spawn_file_actions_addinherit_np`, which `libc` does not bind. Measured
  on this host, `adddup2(fd, fd)` does not clear close-on-exec. Every other
  close-on-exec descriptor stays closed in the child.
- **Signals:** the Rust runtime ignores SIGPIPE in the relay and the guardian.
  `POSIX_SPAWN_SETSIGDEF` resets it to the default in the child, so the
  supervisor and everything it starts never inherit that suppression. Every
  other disposition and the signal mask are inherited exactly as `execve` would
  inherit them.

## Residuals

Stated, not closed here:

- **The relay stays in the caller's group.** If that group is orphaned by
  something outside the run, the relay is signalled as `umbra` was before. Its
  death then cancels the run through the guardian's sweep.
- **Double failure** (#199): if both the relay and the guardian are killed,
  nothing sweeps S, and its processes can outlive the run. They are in a session
  other than 1, so #193's SIGHUP cannot reach them. The platform also keeps such
  a run alive until its own deadline (#203).
- **Other debugserver detach routes** (#200): only the RSP-loss route is
  governed, by the verified `QSetDetachOnError:0`; any other route by which
  debugserver might detach with that setting has not been audited.
- **No caller plus an unwritable record:** R-4 above.
- **Session continuity in the relay's fallback** (the design's R-9, as amended
  by the T1 resolution): continuity of S is proved by the zombie anchor, not by
  a registry of members. A process found in S without a readable audit token
  is reported `UNVERIFIABLE` and never signalled, so it can outlive the run.
- **Only `umbra run` is isolated.** `umbra resume` and `umbra providers` also
  start provider processes, with no tracee, and stay in the caller's session.
- **A tracee that leaves S** with its own `setsid()` is cleaned up through its
  debugserver (the verified `QSetDetachOnError:0`; platform README). That
  route was measured in the #193 design readings, where umbra already sent the
  packet; no test in this repository exercises it on the shipped binary.
- **A rejected `QSetDetachOnError:0` fails the connection**, but no test injects
  a rejection: the suite has no scripted debugserver. The success path runs on
  every connection to the real one (every `umbra run`, and
  `umbra-platform-macos`'s handshake test), and the handler that answers `OK` is
  `RNBRemote.cpp:2429-2447` (swift-6.3.2).

## Tests

- `src/*` unit tests:
  - frame parsing, including partial, malformed and oversized frames;
  - record lines, short writes, `fsync` failure and retention;
  - the sweep's interleavings, scripted: the last witness lost mid-sweep, a
    registry refresh after the loss, and a reused pid or unknown claimant never
    signalled;
  - spawn: no SIGPIPE inheritance, a new session, exact descriptor passing.
- `tests/t1_kernel.rs`, on the real kernel:
  - with the anchor forced off, claimants are reported `UNVERIFIABLE` and not
    signalled, and with it held they are killed (the frozen-set half of T1);
  - a stale token is refused;
  - a sanity check that a zombie leader stays a zombie while new sessions are
    created. It attempts no pid reuse, so it does not show the anchor.
- The anchor-held half of T1 (no process can obtain a zombie leader's pid or
  session id) is not a CI test. It rests on the XNU source cited under
  Cleanup and ownership, and on one recorded reading of the #193 arc that
  wrapped the whole pid space past a held zombie.
- `tests/relay_process.rs`: the real relay against a scripted guardian. It
  covers:
  - the mirrored statuses and O5's exit 1 cases;
  - the fail-closed transitions that a scripted guardian can produce (EOF,
    malformed, oversized and unknown frames, a stopped, silent, hung or
    exited guardian, a wrong session claim);
  - the bounds.

  Every case that starts a guardian also checks that the single reap is the
  record's last line, that it reaped, and that no signal follows it. Permanent
  `waitid` and `poll` errors also fail closed in code, but they are not
  injected.
- `tests/guardian_process.rs`: the real guardian against a fake relay. It covers
  the sweep on supervisor exit and on relay death, a mid-run record failure in
  `OUTCOME`, and R-4's stderr line.

The production-path tests in session 1 (P1-P4, topology, cleanup, and
debugserver's arguments left as on master) and the terminal tests are `umbra-cli`'s
`tests/isolation.rs`. CI's self-hosted runner, whose jobs run in session 1, runs
both suites with `UMBRA_REQUIRE_SID1=1`.

```sh
cargo test -p umbra-isolation
```
