# umbra-test-child

Single-file C11 fixture for the umbra M0 Gate 2 tracer. Build with `./build.sh`,
then run `./smoke.sh`. The build prints its command and all compiler diagnostics;
it uses the exact flags requested in TASK.md. Explicit `-std=c11` syntax checking
also passes.

Bare-name commands take exactly one path; commands spelled as `--options` take
that path plus their own operands. Every command creates/truncates with mode
0644 (subject to umask), and exits zero on success or nonzero on failure.
Errors are individual stderr lines containing an operation, numeric errno, and
its description.
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
| `./umbra-test-child --argv0-check <path> <vendor-argv0>` | `argv0\n` | not covered |
| `./umbra-test-child --wnohang-wait <path>` | `wnohang\n` | PASS |
| `./umbra-test-child --dirfd-rename <root>` | `dirfd\n` at `<root>/db/c` | PASS |
| `./umbra-test-child --symlink-cycle <root>` | links under `<root>/sl` | PASS |

Verified on macOS 26.5.1 (25F80), native arm64, Apple clang 21.0.0 on
2026-09-07 (`--wnohang-wait`, `--dirfd-rename` and `--symlink-cycle` added
2026-09-09): warning-free build and all ten smoke cases PASS. Smoke uses a fresh
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
- `--argv0-check` is the one case smoke.sh does not run. It asserts the tracer's
  launch contract: `argv[0]` must equal the `<vendor-argv0>` operand byte for
  byte *and* differ from the image actually running (`_NSGetExecutablePath`,
  compared raw and through `realpath`). Only then does it write `argv0\n` and
  print `CAPTURED argv0-check` to stderr. Running the binary directly makes
  `argv[0]` the running image, so the second leg fails by construction; POSIX
  `sh` cannot set a different `argv[0]`. Its coverage is
  `tests/fixtures.rs::argv0_check` in `umbra-platform-macos`, which launches a
  signed twin with a decoy `argv[0]`.
- `--wnohang-wait` polls with `WNOHANG` — mask value 1, the least significant
  bit, not `1 << 1` — through five entry points: the public `wait4`, the
  `__wait4` and `__wait4_nocancel` stubs resolved with `dlsym(RTLD_DEFAULT, …)`
  exactly as the tracer resolves them, and raw `svc #0x80` naming syscall
  numbers 7 and 400 in `x16`. A sixth poll repeats number 7 with unspecified
  bits set in the upper half of `x2`, which the kernel's argument munger drops.
  Each poll must return zero and leave its `status` and `rusage` sentinels
  untouched. The child is held on a pipe rather than sleeping, so a poll that
  wrongly blocks deadlocks instead of passing on timing; the case then releases
  and reaps it, and checks that a repeated reap reports `ECHILD`. Untraced this
  exercises the kernel; the tracer coverage is
  `tests/fixtures.rs::wnohang_wait` in `umbra-platform-macos`.
- `--dirfd-rename` takes a directory rather than an output path and leaves its
  result at `<root>/db/c`, with `<root>/da` emptied. It creates two
  directories with `mkdirat(AT_FDCWD, …)`, opens each with
  `O_RDONLY | O_DIRECTORY`, creates and writes the source by relative name
  through the first descriptor, renames across the two descriptors with
  `renameat(a, "a", b, "b")`, reads the destination back through the second,
  confirms the source name is gone with `ENOENT`, then repeats with
  `renameatx_np(AT_FDCWD, "<root>/db/b", b, "c", 0)` — an absolute source and a
  descriptor-relative destination. Untraced it runs against the host, so
  smoke.sh passes a real directory; traced, the same argument is a logical
  namespace root that exists nowhere on the host, so an operand the tracer
  fails to rewrite lands somewhere absent and fails loudly. Its tracer coverage
  is `tests/fixtures.rs::dirfd_rename` in `umbra-platform-macos`.
- `--symlink-cycle` also takes a directory. It creates a link with `symlink(2)`
  whose target is deliberately not valid UTF-8, reads it back with
  `readlink(2)` — exact bytes, no appended NUL, and again into a three-byte
  buffer to check truncation — then repeats through `symlinkat(2)` and
  `readlinkat(2)` with an absolute target. It traverses a relative and an
  absolute link to the same file, requires `fstatat(…, AT_SYMLINK_NOFOLLOW)` to
  report `S_IFLNK` with the target length, and requires a two-link loop to fail
  `open(2)` with `ELOOP`. The non-UTF-8 byte stays in the *target* only:
  targets are opaque bytes, while APFS rejects the same byte in a file name, so
  traversal uses a plain name. Untraced this exercises real symlinks; traced,
  the same checks run against the overlay's logical links, which are stored as
  a placeholder object plus control metadata rather than as filesystem
  symlinks. Its tracer coverage is `tests/fixtures.rs::symlink_cycle` in
  `umbra-platform-macos`.
- These are untraced fixture checks. Descendant capture, signing/attach access,
  syscall redirection, and NFS behavior remain the Track A tracer's tests.
