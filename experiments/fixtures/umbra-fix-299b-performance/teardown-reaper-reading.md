# Routed teardown after provider reaper change

Three warmups followed by one diagnostic using commit 6ef21db1. All four invocations passed. This is not D6 acceptance; the earlier reading remains retained.

| Stage | Process | Offset ms |
|---|---|---:|
| first_root_exit_observed | 29848 | 0.000 |
| cleanup-start-requested | 29845 | 6.440 |
| provider-collection-begin | 29845 | 12.425 |
| provider-collection-received | 29845 | 12.537 |
| child-collected-CleanupOwner | 29844 | 12.892 |
| writer-finish-begin | 29845 | 13.446 |
| writer-finish-durable | 29845 | 64.783 |
| namespace-destroyed | 29845 | 65.330 |
| child-collected-Worker | 29844 | 66.459 |
| child-collected-Guardian | 29842 | 66.997 |
| caller-collected | 29842 | 70.259 |

Namespace destruction took 0.547 ms after durable writer completion, compared with 30.492 ms before the change. The following worker collection interval was 1.129 ms, compared with 31.537 ms. The journal interval increased in this single reading; no samples were discarded or thresholds changed.
