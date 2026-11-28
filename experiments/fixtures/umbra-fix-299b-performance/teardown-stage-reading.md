# Routed teardown stage reading

Three warmups followed by one diagnostic. All four invocations succeeded; no D6 acceptance claim. Offsets start at observed root exit. Actual collection events include parent and child identities in retained raw records.

| Stage | Process | Offset ms |
|---|---|---:|
| first_root_exit_observed | 17659 | 0.000 |
| cleanup-start-requested | 17657 | 1.008 |
| provider-collection-begin | 17657 | 7.456 |
| provider-collection-received | 17657 | 7.841 |
| child-collected-CleanupOwner | 17656 | 8.193 |
| writer-finish-begin | 17657 | 9.128 |
| writer-finish-durable | 17657 | 25.828 |
| namespace-destroyed | 17657 | 56.320 |
| child-collected-Worker | 17656 | 87.857 |
| child-collected-Guardian | 17655 | 88.526 |
| caller-collected | 17655 | 90.781 |
