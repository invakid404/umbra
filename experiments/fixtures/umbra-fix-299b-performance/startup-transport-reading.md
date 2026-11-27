# Provider transport startup stages

Single diagnostic invocations, native first, no warmup. Times are cumulative from immediately before command spawn. The interval before accept includes executable loading, runtime startup and listener scheduling; it is not solely polling cost. All prior readings retained.

| Workload | Role | Spawn ms | Accept ms | Welcome ms |
|---|---|---:|---:|---:|
| native_true | storage | 0.290 | 292.976 | 293.537 |
| native_true | journal | 0.313 | 302.642 | 302.916 |
| native_true | platform | 1.149 | 281.590 | 282.115 |
| registry_true | storage | 0.272 | 258.329 | 327.023 |
| registry_true | journal | 0.320 | 37.252 | 37.566 |
| registry_true | platform | 1.170 | 22.158 | 22.625 |
