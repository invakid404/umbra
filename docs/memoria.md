# Memoria

## What Memoria is here

Memoria keeps READMEs connected to the source they own, with a check enforced in
CI. The nearest `README.md` above a file owns it; a nested README starts a new
ownership boundary. Memoria detects changes that need review. Contributors check
whether the prose still matches the code. This guide covers repository policy;
the [Memoria skill](../.claude/skills/memoria/SKILL.md) covers the procedure.

## When you interact with it

Use Memoria after any code change in a README's ownership boundary and before
opening a pull request. When reviewing someone else's pull request that touches
source, check the affected README claims and the documentation gate too.

## The five-stage per-session flow

1. Start with `memoria status` to see ownership coverage and pending reviews.
2. Follow the review plan's `next_action`: render outdated imports or prepare the
   next ready README for review. Imports are generated excerpts from other READMEs.
3. Cross-check the README's prose against its owned source and the project writing
   rules. Edit claims that have drifted; refresh the review packet after edits.
4. Acknowledge the review with a note that states what you verified.
5. Repeat until the plan is empty and `memoria check` passes.

Use the [skill's procedure](../.claude/skills/memoria/SKILL.md#procedure-after-code-changes)
for exact commands, review order, and packet/token rules. Review packets belong
outside the repository so they do not become documentation inputs.

## CI gate

The existing `rust` job in [CI](../.github/workflows/ci.yml) installs Memoria and
runs `memoria --root . check` after the contract doctests on both `macos-14` and
`ubuntu-latest`. A non-zero exit fails the job. A pending-review diagnostic means
a README has not been reviewed against its current inputs; it does not by itself
prove the prose is wrong. Read the diagnostics for outdated imports or structural
errors too. Passing the gate validates recorded reviews, not proof of prose accuracy.

## How to invalidate for policy changes

Invalidation requests a semantic review even when source files have not changed.
Any session may invalidate when documentation guidance has shifted; prefer the
narrowest scope that fits so unrelated reviews are not forced. Routine drift
from ordinary code changes flows through the review procedure instead of
invalidation.

For a writing-rule change requiring repository-wide review, use
`memoria invalidate all --reason "..."`. For a narrower audit, use
`memoria invalidate subtree:<dir> --reason "..."`. Replace the reason with the
policy change reviewers must address, then follow the skill's review procedure.

## Boundaries this pilot establishes

The persistent documentation-inventory session runs `invalidate all` audits
after policy changes. Routine file changes and their README reviews belong to
every session that touches code, before opening a pull request.

[memoria.toml](../memoria.toml) holds Umbra's writing rules. Its ignore rules exclude
scratch experiment files, build output, macOS debug bundles, jj internal state,
and `Cargo.lock` from owned inputs. Lockfile-only dependency updates do not require
a README review; `Cargo.toml` remains a review input.
The CLI still discovers READMEs under ignored experiment paths, with no owned files.
On first initialization, discovered READMEs need an initial review. Later sessions
must finish their affected reviews before the CI gate passes.
