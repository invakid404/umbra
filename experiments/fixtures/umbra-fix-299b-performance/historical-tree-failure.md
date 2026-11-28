# Historical routed tree failure: issue needed

At head 7441501a, the fifteenth measured registry_tree5 pair (zero-based index14, head order1) failed. The retained samples.jsonl reports exit status1, cancelled=false, forced=false. stderr reports shutdown owner-progress failure and failed managed provider collection. The reading remains a failure; no samples were removed.

The original stderr and sample record are retained in the qualification archive. A separate diagnostic of the same frozen binaries succeeded. The full subsequent thirty-pair series at648dbda2 also had no terminal failure. These successes do not establish a cause or invalidate the failed reading.

Per the visit10 anchor ruling, further reproduction is deferred to an issue. No root cause is claimed. The old404.6ms Retiring-to-Release interval is not evidence of a native destructor delay because measurement output occurs between those calls.

D7 remains fail-closed: historical failure exited1 with the owner-progress diagnostic. Current shutdown_drop_latch_envelope_fails_closed and r2_grace_exhaustion_is_loud both passed, retaining owner failure and refusing restoration by late terminal data. CLI execute still maps errors to ExitCode::FAILURE. No threshold or failure classification changed.

Follow-up: investigate the retained owner-progress/disconnection event using the original frozen records and a bounded reproducer. Do not treat an isolated successful rerun as resolution.
