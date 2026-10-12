# Reviewing a test-coverage section

A guide for sections that document the tests under a crate — their
fixtures, their gating env vars, and the acceptance bar each test
enforces. It applies on top of project guidance; follow project
guidance where the two conflict.

## What to check

1. **Reproducible fixture setup.** The section states, or links to, the
   steps a reader without project context needs to re-run the suite:
   fixture paths, feature flags, and the commands that build any
   support artefacts. "Set up the fixture" is not enough.
2. **Gating flagged.** Every CI-only or environment-gated test — one
   that requires network, a specific OS, root privileges, a mounted
   external filesystem, or an env var — is explicitly flagged. A test
   that silently skips without the gate stated is a finding.
3. **Concrete pass/fail bar.** Each test the section names has a
   concrete acceptance bar: exit codes, substring markers in stderr or
   captured output, or named assertion counts. "Passes" with no observable
   criterion is a finding.
4. **Opt-in fault tests called out.** Where the suite ships `#[ignore]`
   or feature-gated fault tests, the section lists them separately and
   says how to run them. A silently-skipped fault test is a finding.
