# Historical routed tree failure: Refs #314

PR #313 retains a historical after-root-exit failure at head 7441501a: measured registry_tree5 pair 15 (zero-based index 14, head order 1) exited with status 1, cancelled=false and forced=false. The diagnostic reported shutdown owner-progress failure and failed managed provider collection. No sample was discarded.

A diagnostic using the same frozen binaries succeeded, as did the subsequent thirty-pair series at 648dbda2. The historical failure also did not recur in 100 later head-only runs. These successes neither establish its cause nor invalidate the original failed reading. The retained analysis is summarized in checkpoint11-retained-analysis.md beside this file.

The newer pre-GO preparation deadline failure has a different timeline and was repaired separately. It does not establish the historical cause. Issue #314 tracks the historical failure; further broad reproduction is outside this change.

The failure remains loud: status 1 with an owner-progress diagnostic. The shutdown_drop_latch_envelope_fails_closed and r2_grace_exhaustion_is_loud controls retain owner failure and reject restoration by late terminal data. No threshold or failure classification changed. The historical 404.6 ms Retiring-to-Release interval includes measurement output and cannot be attributed to native destruction alone.
