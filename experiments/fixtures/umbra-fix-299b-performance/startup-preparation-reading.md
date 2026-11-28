# Expanded startup preparation reading

One invocation per workload on the same frozen instrumented binaries, native first. No warmup, D6 result or causal attribution is claimed. Program load/runtime remain combined before executable entry. Intervals from different processes overlap.

## native

Invocation to worker GO: 1092.763 ms.

| PID | Stage | Target role | Offset ms | Since preceding record ms |
|---|---|---|---:|---:|
| 94305 | executable-entry | entry | 302.082 | 302.082 |
| 94305 | caller-validation-begin | Caller | 304.188 | 2.107 |
| 94305 | caller-validation-end | Caller | 304.332 | 0.144 |
| 94305 | launch-configuration-ready | Caller | 304.868 | 0.536 |
| 94305 | root-relay-entry | Relay | 304.950 | 0.082 |
| 94305 | spawn-begin | Guardian | 306.397 | 1.447 |
| 94305 | spawn-return | Guardian | 306.724 | 0.327 |
| 94305 | announcement-received | Guardian | 555.045 | 248.321 |
| 94305 | inventory-queued | Guardian | 555.163 | 0.118 |
| 94305 | readiness-received | Guardian | 556.322 | 1.159 |
| 94305 | readiness-queued | Relay | 556.392 | 0.070 |
| 94305 | go-queued | Guardian | 1083.762 | 527.369 |
| 94307 | executable-entry | entry | 308.543 | 308.543 |
| 94307 | configuration-validated | Guardian | 309.729 | 1.186 |
| 94307 | entry-adopted | Guardian | 309.810 | 0.081 |
| 94307 | configuration-decoded | Guardian | 310.291 | 0.481 |
| 94307 | dispatch-registered | Guardian | 310.343 | 0.052 |
| 94307 | spawn-begin | Worker | 311.379 | 1.036 |
| 94307 | spawn-return | Worker | 311.656 | 0.277 |
| 94307 | spawn-begin | CleanupOwner | 312.678 | 1.022 |
| 94307 | spawn-return | CleanupOwner | 314.666 | 1.988 |
| 94307 | announcement-received | Worker | 318.712 | 4.046 |
| 94307 | announcement-received | CleanupOwner | 554.815 | 236.103 |
| 94307 | announcement-queued | Guardian | 554.936 | 0.120 |
| 94307 | inventory-received | Guardian | 555.252 | 0.317 |
| 94307 | inventory-queued | Worker | 555.336 | 0.083 |
| 94307 | inventory-queued | CleanupOwner | 555.409 | 0.073 |
| 94307 | readiness-received | Worker | 555.621 | 0.212 |
| 94307 | readiness-received | CleanupOwner | 556.094 | 0.473 |
| 94307 | readiness-queued | Guardian | 556.195 | 0.102 |
| 94307 | go-received | Guardian | 1083.903 | 527.707 |
| 94307 | go-queued | Worker | 1084.000 | 0.097 |
| 94307 | go-queued | CleanupOwner | 1084.085 | 0.086 |
| 94308 | executable-entry | entry | 316.780 | 316.780 |
| 94308 | configuration-validated | Worker | 317.885 | 1.105 |
| 94308 | entry-adopted | Worker | 317.950 | 0.065 |
| 94308 | configuration-decoded | Worker | 318.418 | 0.468 |
| 94308 | dispatch-registered | Worker | 318.467 | 0.049 |
| 94308 | announcement-queued | Worker | 318.549 | 0.082 |
| 94308 | inventory-received | Worker | 555.451 | 236.903 |
| 94308 | readiness-queued | Worker | 555.543 | 0.092 |
| 94308 | worker-setup-begin | Worker | 556.122 | 0.579 |
| 94308 | worker-validation-end | Worker | 556.188 | 0.066 |
| 94308 | workspace-inventory-end | Worker | 556.391 | 0.203 |
| 94308 | storage-connect-end | Worker | 788.758 | 232.367 |
| 94308 | namespace-prepared | Worker | 1066.975 | 278.217 |
| 94308 | go-received | Worker | 1092.693 | 25.718 |
| 94309 | executable-entry | entry | 318.231 | 318.231 |
| 94309 | configuration-validated | CleanupOwner | 319.668 | 1.438 |
| 94309 | entry-adopted | CleanupOwner | 319.721 | 0.053 |
| 94309 | configuration-decoded | CleanupOwner | 320.151 | 0.430 |
| 94309 | dispatch-registered | CleanupOwner | 320.204 | 0.052 |
| 94309 | provider-launch-begin | Provider | 321.230 | 1.026 |
| 94309 | provider-launch-end | Provider | 554.488 | 233.258 |
| 94309 | announcement-received | Provider | 554.636 | 0.148 |
| 94309 | announcement-queued | CleanupOwner | 554.726 | 0.091 |
| 94309 | inventory-received | CleanupOwner | 555.486 | 0.760 |
| 94309 | inventory-queued | Provider | 555.573 | 0.087 |
| 94309 | readiness-received | Provider | 555.902 | 0.329 |
| 94309 | readiness-queued | CleanupOwner | 555.985 | 0.082 |
| 94309 | go-received | CleanupOwner | 1084.216 | 528.232 |
| 94309 | go-queued | Provider | 1084.307 | 0.091 |
| 94310 | executable-entry | entry | 551.711 | 551.711 |
| 94310 | configuration-validated | Provider | 553.350 | 1.639 |
| 94310 | provider-accept-begin | Provider | 553.494 | 0.144 |
| 94310 | provider-accept-end | Provider | 554.304 | 0.811 |
| 94310 | announcement-queued | Provider | 554.484 | 0.180 |
| 94310 | inventory-received | Provider | 555.768 | 1.284 |
| 94310 | readiness-queued | Provider | 555.837 | 0.070 |
| 94310 | go-received | Provider | 1084.448 | 528.611 |

## registry

Invocation to worker GO: 531.842 ms.

| PID | Stage | Target role | Offset ms | Since preceding record ms |
|---|---|---|---:|---:|
| 94781 | executable-entry | entry | 5.125 | 5.125 |
| 94781 | caller-validation-begin | Caller | 5.622 | 0.497 |
| 94781 | caller-validation-end | Caller | 5.677 | 0.055 |
| 94781 | launch-configuration-ready | Caller | 6.102 | 0.425 |
| 94781 | root-relay-entry | Relay | 6.157 | 0.055 |
| 94781 | spawn-begin | Guardian | 7.204 | 1.047 |
| 94781 | spawn-return | Guardian | 7.420 | 0.216 |
| 94781 | announcement-received | Guardian | 26.646 | 19.227 |
| 94781 | inventory-queued | Guardian | 26.749 | 0.103 |
| 94781 | readiness-received | Guardian | 27.773 | 1.024 |
| 94781 | readiness-queued | Relay | 27.833 | 0.060 |
| 94781 | go-queued | Guardian | 524.891 | 497.058 |
| 94782 | executable-entry | entry | 9.465 | 9.465 |
| 94782 | configuration-validated | Guardian | 10.523 | 1.058 |
| 94782 | entry-adopted | Guardian | 10.582 | 0.059 |
| 94782 | configuration-decoded | Guardian | 10.991 | 0.410 |
| 94782 | dispatch-registered | Guardian | 11.030 | 0.038 |
| 94782 | spawn-begin | Worker | 11.987 | 0.957 |
| 94782 | spawn-return | Worker | 12.192 | 0.205 |
| 94782 | spawn-begin | CleanupOwner | 13.218 | 1.026 |
| 94782 | spawn-return | CleanupOwner | 13.402 | 0.184 |
| 94782 | announcement-received | Worker | 16.056 | 2.654 |
| 94782 | announcement-received | CleanupOwner | 26.429 | 10.373 |
| 94782 | announcement-queued | Guardian | 26.537 | 0.108 |
| 94782 | inventory-received | Guardian | 26.824 | 0.287 |
| 94782 | inventory-queued | Worker | 26.902 | 0.078 |
| 94782 | inventory-queued | CleanupOwner | 26.974 | 0.072 |
| 94782 | readiness-received | Worker | 27.139 | 0.166 |
| 94782 | readiness-received | CleanupOwner | 27.552 | 0.413 |
| 94782 | readiness-queued | Guardian | 27.648 | 0.096 |
| 94782 | go-received | Guardian | 525.011 | 497.363 |
| 94782 | go-queued | Worker | 525.114 | 0.104 |
| 94782 | go-queued | CleanupOwner | 525.197 | 0.083 |
| 94783 | executable-entry | entry | 14.313 | 14.313 |
| 94783 | configuration-validated | Worker | 15.373 | 1.060 |
| 94783 | entry-adopted | Worker | 15.428 | 0.056 |
| 94783 | configuration-decoded | Worker | 15.845 | 0.417 |
| 94783 | dispatch-registered | Worker | 15.899 | 0.053 |
| 94783 | announcement-queued | Worker | 15.968 | 0.070 |
| 94783 | inventory-received | Worker | 26.990 | 11.022 |
| 94783 | readiness-queued | Worker | 27.067 | 0.077 |
| 94783 | worker-setup-begin | Worker | 27.559 | 0.492 |
| 94783 | worker-validation-end | Worker | 27.613 | 0.054 |
| 94783 | workspace-inventory-end | Worker | 27.725 | 0.112 |
| 94783 | storage-connect-end | Worker | 425.532 | 397.807 |
| 94783 | namespace-prepared | Worker | 520.034 | 94.502 |
| 94783 | go-received | Worker | 531.772 | 11.738 |
| 94784 | executable-entry | entry | 16.232 | 16.232 |
| 94784 | configuration-validated | CleanupOwner | 17.293 | 1.061 |
| 94784 | entry-adopted | CleanupOwner | 17.349 | 0.055 |
| 94784 | configuration-decoded | CleanupOwner | 17.751 | 0.402 |
| 94784 | dispatch-registered | CleanupOwner | 17.793 | 0.042 |
| 94784 | provider-launch-begin | Provider | 18.761 | 0.968 |
| 94784 | provider-launch-end | Provider | 26.188 | 7.427 |
| 94784 | announcement-received | Provider | 26.287 | 0.099 |
| 94784 | announcement-queued | CleanupOwner | 26.349 | 0.062 |
| 94784 | inventory-received | CleanupOwner | 27.064 | 0.715 |
| 94784 | inventory-queued | Provider | 27.150 | 0.086 |
| 94784 | readiness-received | Provider | 27.370 | 0.221 |
| 94784 | readiness-queued | CleanupOwner | 27.452 | 0.082 |
| 94784 | go-received | CleanupOwner | 525.323 | 497.871 |
| 94784 | go-queued | Provider | 525.416 | 0.093 |
| 94785 | executable-entry | entry | 21.261 | 21.261 |
| 94785 | configuration-validated | Provider | 22.270 | 1.008 |
| 94785 | provider-accept-begin | Provider | 22.328 | 0.058 |
| 94785 | provider-accept-end | Provider | 26.044 | 3.716 |
| 94785 | announcement-queued | Provider | 26.149 | 0.104 |
| 94785 | inventory-received | Provider | 27.261 | 1.112 |
| 94785 | readiness-queued | Provider | 27.316 | 0.055 |
| 94785 | go-received | Provider | 525.544 | 498.228 |
