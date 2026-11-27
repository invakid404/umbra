# Visit10 acceptance report for the anchor

Runtime head b1fa03da is frozen. No thresholds changed and no failed reading was retried. Items1–5 produced committed stage tables and tested fixes before this D6 reading. Resource observation runs after readiness under the original two-second startup cutoff; the fixed nine-case D5 matrix passed.

## D6

Three warmup pairs and thirty alternating pairs were scheduled per workload. Native true, routed true and admission-only retained all six warmups and sixty measured invocations. The routed tree series retained six warmups, fifty successful measured invocations, then a failed head invocation at pair26 (zero-based25/order0); it stopped as required and has no completed distribution.

All durations below are milliseconds; pairs are median / p95.

| Workload | Admission | Helper startup | Baseline teardown | Head teardown | Result |
|---|---:|---:|---:|---:|---|
| native_true | 0.454 / 1.097 | 54.347 / 85.306 | 103.380 / 165.790 | 43.792 / 145.661 | helper startup misses |
| registry_true | 0.455 / 1.040 | 172.486 / 271.806 | 111.389 / 128.692 | 61.683 / 263.522 | helper startup and teardown p95 miss |
| admission_only | 0.757 / 2.680 | 8.767 / 27.948 | 46.639 / 103.964 | 72.811 / 153.471 | teardown median and p95 miss |

Admission limits remain1ms/5ms and pass in all three completed series. Helper startup limits remain25ms/50ms. Teardown limits remain max(5ms,10%) median increase and max(10ms,20%) p95 increase. Native teardown improves. The routed p95 and admission-only regressions remain failures; all outliers are retained.

The new tree failure is a startup failure before helpers_ready_go or a root-exit observation. stderr contains timeout, provider transport disconnection and managed.run protocol error. Guardian receipt confirms collected Worker and CleanupOwner status256, unresolved0, failed=true. Parent receipt records exit1, cancelled=false, forced=false. This is not asserted to be the same cause as the earlier7441501a owner-progress failure. No additional reproduction is planned under the current ruling.

## Stage evidence and fixes

- Startup tables were committed before changing acceptance wait behavior. Warmed diagnostic before readiness fix: native152.688ms/raw259.782ms; after:62.041ms/180.985ms. These single diagnostics are not D6 acceptance. Loader/runtime time remains combined before executable entry; no structural D8 lower-bound claim is made.
- Provider connection wait now wakes on socket readability while preserving the original deadline and5ms child-exit inspection cap. Failing-first queued connection median37.140ms became0.000875ms; expired-cutoff controls pass.
- Routed teardown before reaper change: root-to-return90.781ms, namespace destruction30.492ms, following worker collection31.537ms. After bounded actual-wait receipt:70.259ms,0.547ms and1.129ms. The journal interval increased. All four prescribed diagnostic invocations per reading remain retained.
- Admission fixture timer wait was replaced with production readiness. Its failing-first median22.456ms became0.205–0.330ms across thirty actual child/owner cycles; the final full admission-only series passes admission but still fails teardown.
- D5 after readiness: CLI3 extra helpers and21managed descriptors, library4/32, within22/33 ceilings. Overall descriptor deltas30/44 include9/12helper standard I/O descriptors. Caller return inventories match and no observed owned process or zombie survives. Startup maxima are not measured by this authorized method.

## Requested disposition

D6 remains BLOCKED pending the anchor's decision. No further startup/teardown redesign or bound change is taken. The stage and distribution records support a remaining miss, not a proven architectural lower bound. Other qualification and documentation work continues independently.

The historical tree issue note is checkpoint10-tree5-note.md. It preserves the earlier exit1/owner-progress failure and successful subsequent evidence without claiming a cause. Current owner failure controls pass. No external issue or PR was posted.
