# Reviewing a provider-binary section

A guide for sections that document a provider crate's shipped
executable — its `src/main.rs`, its config/registration surface, and the
JSON descriptor callers pass to `umbra providers`. It applies on top of
project guidance; follow project guidance where the two conflict.

## What to check

1. **Binary name convention.** For a provider crate (not the CLI, which
   ships `umbra`), the executable's name matches the crate name. Drift
   is a finding. A section that cites this guide is implicitly a
   provider section, so the convention applies.
2. **End-to-end lifecycle.** The prose describes install → register →
   run → cleanup, or links to the companion in `docs/providers.md`. A
   lifecycle step that is only reachable by reading the code is a
   finding.
3. **Enumerated env vars.** Every environment variable the binary
   consults — including those read by shared library code the binary
   links in — is named in the section with its default and effect.
4. **Config surface stays honest.** Where the prose names a config
   field, option key, or JSON descriptor key, verify it against the
   type definitions in the mapped code; a renamed or removed field is
   a finding.
