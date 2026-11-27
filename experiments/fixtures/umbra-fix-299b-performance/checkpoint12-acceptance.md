# Final performance qualification under the owner disposition

Frozen source80e8b9c9729bceb2ea02dfd2b1981840331bf5ac was compared with the unchanged original baseline. Every workload completed three warmup pairs and thirty alternating measured pairs. Across all132 head invocations, guard, protocol and deadline failure counters were zero. Admission passed its unchanged1ms median /5ms p95 bounds. Every attempted record, including outliers, is retained.

The November27 owner optionA accepts command whole-invocation median no slower than base by more than max(5ms,10% of base median). Admission_only fixed cost is reported separately. The original helper-start and teardown distributions and misses remain visible; they are not erased by this acceptance policy. All four final series passed the owner criteria.

All table values below are milliseconds.

| Workload | Whole median base / head | Teardown median base / head | Teardown p95 base / head | Helper median / p95 | Admission median / p95 |
|---|---:|---:|---:|---:|---:|
| native_true | 2064.537 / 2013.619 | 101.675 / 36.137 | 132.325 / 89.997 | 52.520 / 67.723 | 0.410 / 0.851 |
| registry_true | 1984.507 / 1946.255 | 104.457 / 45.967 | 148.476 / 75.073 | 162.330 / 179.389 | 0.390 / 0.698 |
| registry_tree5 | 3948.046 / 4263.257 | 92.891 / 50.599 | 135.431 / 217.193 | 147.948 / 171.091 | 0.412 / 0.924 |
| admission_only | 15.087 / 24.348 | 5.377 / 11.879 | 9.033 / 16.025 | 5.874 / 8.494 | 0.304 / 0.416 |

Native and routed-true whole medians improved. Tree5 increased315.211ms (7.984%), within its394.805ms margin. The original helper-start limits are still missed for all three command workloads. Tree5 teardown p95 is217.193ms versus135.431ms base, an81.761ms tail increase. Admission_only adds9.261ms to whole-invocation median, with teardown median11.879ms versus5.377ms base. These remain PR3 follow-up costs. Native admission maximum6.842ms and tree admission maximum11.669ms are retained outliers; the original admission policy constrains median and p95, not maximum.

The first routed attempts failed a registry path guard before their head invocations: descriptors used /tmp paths while the loader canonicalized to /private/tmp. Each retains its successful baseline warmup and failed head configuration record with no head sample. Only registry paths were corrected; binaries, baseline, workload bytes and acceptance criteria were unchanged. The complete routed series use fresh -configured directories. These invalid configuration attempts are not presented as successful product runs.

Evidence: checkpoint12-d6-native_true, checkpoint12-d6-registry_true-configured, checkpoint12-d6-registry_tree5-configured and checkpoint12-d6-admission_only under the retained evidence directory. Each holds samples.jsonl, summary.json, all invocation events, diagnostics and parent receipts, plus before/after host censuses. checkpoint12-d6-results.json collects the final metrics and independently checks all head failure counters, including warmups. Original routed configuration failures remain in checkpoint12-d6-registry_true and checkpoint12-d6-registry_tree5. Immutable artifact manifests are in checkpoint12-final-artifacts.

D5 readiness remains21/32 within22/33. The fresh startup census reports24/35 by the conservative visit-11 method; the owner accepts these startup-only counts and its explicit discovery/sequential-query limits. See checkpoint12-startup-resources.md. No absolute descriptor maximum is claimed. SID1, native_tree5, mounted NFS and the known#303 residual provider-loss shapes remain unqualified locally.
