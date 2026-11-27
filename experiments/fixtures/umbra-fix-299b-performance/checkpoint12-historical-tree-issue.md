# Historical routed tree failure: issue needed

At head 7441501a, the fifteenth measured registry_tree5 pair (zero-based index14, head order1) failed. The retained samples.jsonl reports exit status1, cancelled=false, forced=false. stderr reports shutdown owner-progress failure and failed managed provider collection. The reading remains a failure; no samples were removed.

Evidence: /tmp/graph-dg-zxzgdtjs/evidence/d6-registry_tree5/sample-14-1/stderr and ../samples.jsonl. A separate diagnostic of the same frozen binaries (evidence/checkpoint9-tree-original-diagnosis) succeeded. The full subsequent thirty-pair series at648dbda2 also had no terminal failure (evidence/checkpoint9-d6-registry_tree5). These successes do not establish a cause or invalidate the failed reading.

Per the visit10 anchor ruling, further reproduction is deferred to an issue. No root cause is claimed. The old404.6ms Retiring-to-Release interval is not evidence of a native destructor delay because measurement output occurs between those calls.

D7 remains fail-closed: historical failure exited1 with the owner-progress diagnostic. Current shutdown_drop_latch_envelope_fails_closed and r2_grace_exhaustion_is_loud both passed (evidence/checkpoint10-owner-failure.log), retaining owner failure and refusing restoration by late terminal data. CLI execute still maps errors to ExitCode::FAILURE. No threshold or failure classification changed.

Follow-up: investigate the retained owner-progress/disconnection event using the original frozen records and a bounded reproducer. Do not treat an isolated successful rerun as resolution.

WITHDRAWN 2026-11-27 (checkpoint11-anchor-ruling.md item 1): the failure recurred on the new code; PR 2 handles it. Do not file.

RESTORED 2026-11-27 (checkpoint12-anchor-ruling.md item2): the WITHDRAWN line above is cancelled. The historical after-root-exit owner-progress failure did not reproduce in100 head-only runs or the subsequent full series. The newer pre-GO preparation deadline failure has a different timeline and does not establish the historical cause. File this historical failure as a separate issue; retain all original failed evidence and the100-run result in checkpoint11-retained-analysis.md. No further broad reproduction is authorized here.
