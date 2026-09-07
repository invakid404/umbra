Gate 2: NOT PASSED — debugger task control blocked in this session.

Environment: 2026-09-07; macOS 26.5.1 (25F80), arm64, SIP enabled; LLDB 2100.0.17.108; Python 3.9.6; local `/tmp/umbra-nfs-stub` root. NFS was unavailable.

- libc `open` interception + rewrite: PARTIAL — fixture available; launch timed out before initial debugger control; rewrite not exercised.
- raw-`svc` `open` interception: PARTIAL — fixture available and syscall site resolved offline; launch timed out before interception.
- `fork` + immediate child write: PARTIAL — fixture available; launch timed out before fork; child capture unverified.
- `posix_spawn` + child write: PARTIAL — fixture available; launch timed out before spawn; suspended-child attachment unverified.
- `exec` + child write: PARTIAL — fixture available; launch timed out before its fork/exec sequence; resign-on-exec unverified.
- grandchild write: PARTIAL — fixture available; launch timed out before child creation; grandchild capture unverified.

All six direct fixture controls passed. All six traced attempts exited 124 after 10 seconds; neither host nor shadow output existed. PARTIAL denotes completed setup, not successful capture. `demo.sh` failed after its 30-second launch timeout.

Verified: signing, cache hits, corrupted-cache recovery, source-hash invalidation, and offline placement of seven libsystem syscall breakpoints plus the fixture's raw syscall breakpoint.

Blocker: async LLDB launch and CLI fallback both hung. Direct debugserver logging ends immediately before `task_for_pid` after successfully launching the helper suspended. `DevToolsSecurity` reports developer mode disabled; its causal role is unconfirmed. No security settings changed.

Runtime memory/register rewriting, exec, and descendant handling remain unverified. Fork following alone does not establish supervision of the entire tree. Gate 2 provides no basis to start the Rust supervisor. Reproducers, limitations, and saved evidence are in `README.md` and `results/`.

Automatic approval review initially rejected numeric-PID cleanup because process identities were insufficiently established. Cleanup was subsequently approved and completed after exact command-line and parent verification.
