# Reviewing a provider op surface

A guide for sections that wrap a provider's op layer — the file(s) that
implement the per-operation handlers a backend ships. It applies on top
of project guidance; follow project guidance where the two conflict.

## What to check

1. **Disposition stays in sync.** Each shipped-vs-stub claim in the
   section's prose must match what the code actually returns at HEAD. A
   section that promises coverage the code stubs out is a finding, and
   so is prose that still names an op the file no longer implements.
2. **Errno matches code where the prose names it.** Where the README
   states an op's errno behaviour — `EINVAL`, `EPERM`, `EBADF`, `ENOENT`,
   `ELOOP`, `ENOTDIR`, `EEXIST` — the mapping must match the code. This
   does not require enumerating every errno for every op; it enforces
   the ones the prose already claims.
3. **Authority is unambiguous.** For each op the section describes, the
   prose must say who may call it and which capability or lease the call
   demands. A reader who only has this section should not need to open
   the code to find that out.
4. **I/O limits and offsets.** Where the section states a per-request
   cap or offset bound, verify it against the constants in the mapped
   code. A stale number is a finding; omit the number instead when it
   drifts.
