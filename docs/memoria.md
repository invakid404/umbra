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

To ask why a README is pending without preparing a packet, run
`memoria explain <README.md>`. It is read-only, records no review, and reports the
changed paths, their hashes, the selection policy, the guidance state, and the Git
hunks it could verify. A hunk it cannot verify carries an explicit reason, such as
a file added since the last review. Use it to triage the gate; use the review
packet to perform the review.

## CI gate

The existing `rust` job in [CI](../.github/workflows/ci.yml) installs Memoria and
runs `memoria --root . check` after the contract doctests on both `macos-14` and
`ubuntu-latest`. A non-zero exit fails the job. A pending-review diagnostic means
a README has not been reviewed against its current inputs; it does not by itself
prove the prose is wrong. Read the diagnostics for outdated imports or structural
errors too. Passing the gate validates recorded reviews, not proof of prose accuracy.

The job pins the CLI to a release tag. Bump that pin and your local install
together so CI diagnostics match what you see locally. From 0.3.0 the human output
groups repeated diagnostics under a counted header, wraps to the terminal width,
and ends with a `Next action:` block. From 0.4.0 the human `review` and `explain`
output starts with changes and evidence; pass `--full` to restore the previous
detailed layout. Existing `review` and `explain` JSON contracts and exit codes
remain compatible across both releases; the new `packet view` command carries
its own versioned JSON view separate from those contracts.

To read one section of a saved packet without preparing a new review, run
`memoria packet view <PACKET> --section <SECTION>` (0.4.0+). `--file <FILE>`
selects one exact saved input instead of a named section. The command
validates the packet's canonical JSON v2 envelope and prints the requested
piece; it does not record a review and does not consult the working tree.

## How to invalidate for policy changes

Invalidation requests a semantic review even when source files have not changed.
Invalidation is initiated on an authorized review request when documentation
guidance has shifted; prefer the narrowest scope that fits so unrelated reviews
are not forced. Routine drift from ordinary code changes flows through the
review procedure instead of invalidation.

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
