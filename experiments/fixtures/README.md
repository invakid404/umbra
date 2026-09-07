# umbra-test-child

Single-file C11 fixture for the umbra M0 Gate 2 tracer. Build with `./build.sh`,
then run `./smoke.sh`. The build prints its command and all compiler diagnostics;
it uses the exact flags requested in TASK.md. Explicit `-std=c11` syntax checking
also passes. Source: 222 lines.

Every command takes exactly one path, creates/truncates with mode 0644 (subject
to umask), and exits zero on success or nonzero on failure. Errors are individual
stderr lines containing an operation, numeric errno, and its description.
Parents print `<subcommand>: child exit=0` on success; abnormal child status is
reported on stderr with ECHILD, following the child's original error if any.

| Argv | File bytes | smoke.sh |
| --- | --- | --- |
| `./umbra-test-child open-libc <path>` | `libc\n` | PASS |
| `./umbra-test-child open-svc <path>` | `libc\n` | PASS |
| `./umbra-test-child fork-write <path>` | `fork\n` | PASS |
| `./umbra-test-child posix-spawn-write <path>` | `libc\n` | PASS |
| `./umbra-test-child exec-write <path>` | `libc\n` | PASS |
| `./umbra-test-child grandchild-write <path>` | `grandchild\n` | PASS |
| `./umbra-test-child dup-inherit-write <path>` | `dup\n` | PASS |

Verified on macOS 26.5.1 (25F80), native arm64, Apple clang 21.0.0 on
2026-09-07: warning-free build and all seven smoke cases PASS. Smoke uses a fresh
temporary directory, compares exact bytes including newlines, and cleans up on
exit. Additional checks passed for missing-parent errors in every command, raw
open returning fd zero, truncation through both open paths, invalid argv, and
exec/spawn via PATH with relative paths containing spaces. Disassembly confirms
`svc #0x80` in `raw_open`, without a libc open call.

Limitations and integration notes:

- Native arm64 Darwin only; the private syscall ABI may change. Darwin returns
  positive errno with carry set, so the inline assembly negates errors before
  C checks the result. Descriptor zero is valid. See
  [Apple's syscall macros](https://github.com/apple-oss-distributions/xnu/blob/main/libsyscall/custom/SYS.h).
  Only `open-svc`'s open is raw; write and close use libc.
- The grandchild writes immediately; its middle parent waits for it, then calls
  `_exit` with its status. This makes command completion deterministic without
  sleeps or polling; it does not exercise an orphaned grandchild.
- Fork/grandchild/inherited-fd writers rely on `_exit` to close their descriptors;
  close-time failures are not observable there. No fsync/durability test is made.
- Exec/spawn resolve this executable before child creation using
  `_NSGetExecutablePath` and `realpath`, with a PATH_MAX buffer. The executable
  must remain accessible. Inherited-fd coverage uses fork inheritance, without
  an explicit dup syscall.
- These are untraced fixture checks. Descendant capture, signing/attach access,
  syscall redirection, and NFS behavior remain the Track A tracer's tests.
