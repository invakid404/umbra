# Reviewing a syscall-coverage section

A guide for sections that enumerate intercepted syscalls or explicitly
list what interception does not cover. It applies on top of project
guidance; follow project guidance where the two conflict.

## What to check

1. **Strict complement.** A syscall must appear in exactly one of the
   covered and does-not-cover sections of the same README. A syscall
   that appears in both, or in neither when it is one the backend
   handles, is a finding.
2. **Dirfd-relative variants.** For each intercepted syscall whose
   kernel surface has a dirfd-relative variant (`openat`, `renameat`,
   `linkat`, `unlinkat`, `symlinkat`, `fstatat`, `faccessat`,
   `readlinkat`, `fchmodat`, `fchownat`), the prose must say whether
   that variant is handled or explicitly list it as uncovered.
3. **Logical-symlink and byte-resolution subtleties.** Whenever a
   syscall's semantics change under umbra's logical symlinks or its
   byte-resolution path, call that out in the section. A syscall whose
   plain-filesystem semantics would mislead a reader without that note
   is a finding.
4. **ABI drift.** Register names, calling convention, and syscall
   numbers quoted in the prose must match the mapped code at HEAD. Prefer
   omitting a number to carrying a stale one.
