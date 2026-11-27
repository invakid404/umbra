# Warmed startup stages

Declared sequence: three warmups and one diagnostic per workload; all eight invocations succeeded and are retained. This is not a D6 distribution or a structural lower bound. Same frozen binaries throughout.

## native_true

Total to GO: 152.688 ms.

| PID | Stage | Offset ms | Since prior record ms |
|---|---|---:|---:|
| 2679 | executable-entry | 2.396 | 2.396 |
| 2679 | caller-validation-begin | 2.898 | 0.502 |
| 2679 | caller-validation-end | 2.952 | 0.054 |
| 2679 | launch-configuration-ready | 3.370 | 0.418 |
| 2679 | root-relay-entry | 3.413 | 0.043 |
| 2679 | spawn-begin | 4.415 | 1.002 |
| 2679 | spawn-return | 4.619 | 0.205 |
| 2679 | announcement-received | 36.749 | 32.130 |
| 2679 | inventory-queued | 36.860 | 0.111 |
| 2679 | readiness-received | 37.910 | 1.050 |
| 2679 | readiness-queued | 37.971 | 0.060 |
| 2679 | go-queued | 143.682 | 105.711 |
| 2680 | executable-entry | 6.036 | 6.036 |
| 2680 | configuration-validated | 6.997 | 0.960 |
| 2680 | entry-adopted | 7.052 | 0.056 |
| 2680 | configuration-decoded | 7.469 | 0.417 |
| 2680 | dispatch-registered | 7.513 | 0.043 |
| 2680 | spawn-begin | 8.424 | 0.911 |
| 2680 | spawn-return | 8.610 | 0.186 |
| 2680 | spawn-begin | 9.535 | 0.925 |
| 2680 | spawn-return | 9.709 | 0.174 |
| 2680 | announcement-received | 11.660 | 1.951 |
| 2680 | announcement-received | 36.546 | 24.886 |
| 2680 | announcement-queued | 36.646 | 0.099 |
| 2680 | inventory-received | 36.949 | 0.303 |
| 2680 | inventory-queued | 37.028 | 0.079 |
| 2680 | inventory-queued | 37.105 | 0.076 |
| 2680 | readiness-received | 37.265 | 0.160 |
| 2680 | readiness-received | 37.695 | 0.431 |
| 2680 | readiness-queued | 37.791 | 0.095 |
| 2680 | go-received | 143.873 | 106.082 |
| 2680 | go-queued | 144.006 | 0.133 |
| 2680 | go-queued | 144.109 | 0.103 |
| 2681 | executable-entry | 9.978 | 9.978 |
| 2681 | configuration-validated | 10.893 | 0.915 |
| 2681 | entry-adopted | 10.944 | 0.051 |
| 2681 | configuration-decoded | 11.356 | 0.412 |
| 2681 | dispatch-registered | 11.424 | 0.068 |
| 2681 | announcement-queued | 11.560 | 0.136 |
| 2681 | inventory-received | 37.124 | 25.564 |
| 2681 | readiness-queued | 37.198 | 0.074 |
| 2681 | worker-setup-begin | 37.700 | 0.502 |
| 2681 | worker-validation-end | 37.753 | 0.053 |
| 2681 | workspace-inventory-end | 37.874 | 0.122 |
| 2681 | storage-connect-end | 77.535 | 39.660 |
| 2681 | namespace-prepared | 139.541 | 62.006 |
| 2681 | go-received | 152.581 | 13.040 |
| 2682 | executable-entry | 11.113 | 11.113 |
| 2682 | configuration-validated | 12.088 | 0.975 |
| 2682 | entry-adopted | 12.138 | 0.050 |
| 2682 | configuration-decoded | 12.528 | 0.389 |
| 2682 | dispatch-registered | 12.566 | 0.038 |
| 2682 | provider-launch-begin | 13.506 | 0.940 |
| 2682 | provider-launch-end | 36.272 | 22.766 |
| 2682 | announcement-received | 36.391 | 0.119 |
| 2682 | announcement-queued | 36.464 | 0.073 |
| 2682 | inventory-received | 37.187 | 0.723 |
| 2682 | inventory-queued | 37.269 | 0.082 |
| 2682 | readiness-received | 37.521 | 0.252 |
| 2682 | readiness-queued | 37.596 | 0.075 |
| 2682 | go-received | 144.275 | 106.680 |
| 2682 | go-queued | 144.423 | 0.147 |
| 2683 | executable-entry | 15.871 | 15.871 |
| 2683 | configuration-validated | 16.819 | 0.948 |
| 2683 | provider-accept-begin | 16.886 | 0.068 |
| 2683 | provider-accept-end | 35.918 | 19.032 |
| 2683 | announcement-queued | 36.040 | 0.122 |
| 2683 | inventory-received | 37.381 | 1.341 |
| 2683 | readiness-queued | 37.451 | 0.070 |
| 2683 | go-received | 144.589 | 107.138 |

| Provider | Spawn ms | Accept ms | Welcome ms |
|---|---:|---:|---:|
| platform | 0.815 | 22.072 | 22.308 |
| storage | 0.203 | 38.737 | 39.008 |
| journal | 0.212 | 42.375 | 42.478 |

## registry_true

Total to GO: 259.782 ms.

| PID | Stage | Offset ms | Since prior record ms |
|---|---|---:|---:|
| 2938 | executable-entry | 2.480 | 2.480 |
| 2938 | caller-validation-begin | 2.990 | 0.511 |
| 2938 | caller-validation-end | 3.052 | 0.062 |
| 2938 | launch-configuration-ready | 3.506 | 0.454 |
| 2938 | root-relay-entry | 3.557 | 0.051 |
| 2938 | spawn-begin | 4.618 | 1.061 |
| 2938 | spawn-return | 4.877 | 0.259 |
| 2938 | announcement-received | 52.754 | 47.877 |
| 2938 | inventory-queued | 52.879 | 0.126 |
| 2938 | readiness-received | 54.095 | 1.216 |
| 2938 | readiness-queued | 54.161 | 0.066 |
| 2938 | go-queued | 252.205 | 198.043 |
| 2940 | executable-entry | 6.780 | 6.780 |
| 2940 | configuration-validated | 7.914 | 1.134 |
| 2940 | entry-adopted | 7.986 | 0.072 |
| 2940 | configuration-decoded | 8.413 | 0.427 |
| 2940 | dispatch-registered | 8.453 | 0.040 |
| 2940 | spawn-begin | 9.417 | 0.965 |
| 2940 | spawn-return | 9.704 | 0.287 |
| 2940 | spawn-begin | 10.710 | 1.006 |
| 2940 | spawn-return | 10.934 | 0.225 |
| 2940 | announcement-received | 13.255 | 2.321 |
| 2940 | announcement-received | 52.471 | 39.216 |
| 2940 | announcement-queued | 52.640 | 0.169 |
| 2940 | inventory-received | 52.979 | 0.339 |
| 2940 | inventory-queued | 53.071 | 0.092 |
| 2940 | inventory-queued | 53.155 | 0.084 |
| 2940 | readiness-received | 53.329 | 0.173 |
| 2940 | readiness-received | 53.870 | 0.541 |
| 2940 | readiness-queued | 53.971 | 0.101 |
| 2940 | go-received | 252.410 | 198.439 |
| 2940 | go-queued | 252.637 | 0.227 |
| 2940 | go-queued | 252.754 | 0.117 |
| 2941 | executable-entry | 11.471 | 11.471 |
| 2941 | configuration-validated | 12.567 | 1.096 |
| 2941 | entry-adopted | 12.633 | 0.066 |
| 2941 | configuration-decoded | 13.067 | 0.433 |
| 2941 | dispatch-registered | 13.111 | 0.045 |
| 2941 | announcement-queued | 13.176 | 0.065 |
| 2941 | inventory-received | 53.175 | 39.999 |
| 2941 | readiness-queued | 53.263 | 0.088 |
| 2941 | worker-setup-begin | 53.872 | 0.609 |
| 2941 | worker-validation-end | 53.932 | 0.060 |
| 2941 | workspace-inventory-end | 54.068 | 0.136 |
| 2941 | storage-connect-end | 96.646 | 42.578 |
| 2941 | namespace-prepared | 247.064 | 150.419 |
| 2941 | go-received | 259.682 | 12.618 |
| 2942 | executable-entry | 12.578 | 12.578 |
| 2942 | configuration-validated | 13.562 | 0.984 |
| 2942 | entry-adopted | 13.611 | 0.049 |
| 2942 | configuration-decoded | 14.019 | 0.408 |
| 2942 | dispatch-registered | 14.058 | 0.038 |
| 2942 | provider-launch-begin | 14.967 | 0.909 |
| 2942 | provider-launch-end | 52.050 | 37.084 |
| 2942 | announcement-received | 52.300 | 0.250 |
| 2942 | announcement-queued | 52.386 | 0.086 |
| 2942 | inventory-received | 53.248 | 0.862 |
| 2942 | inventory-queued | 53.328 | 0.080 |
| 2942 | readiness-received | 53.666 | 0.339 |
| 2942 | readiness-queued | 53.756 | 0.090 |
| 2942 | go-received | 252.942 | 199.186 |
| 2942 | go-queued | 253.106 | 0.164 |
| 2943 | executable-entry | 17.594 | 17.594 |
| 2943 | configuration-validated | 18.529 | 0.935 |
| 2943 | provider-accept-begin | 18.583 | 0.054 |
| 2943 | provider-accept-end | 51.525 | 32.942 |
| 2943 | announcement-queued | 51.680 | 0.155 |
| 2943 | inventory-received | 53.512 | 1.832 |
| 2943 | readiness-queued | 53.593 | 0.082 |
| 2943 | go-received | 253.340 | 199.747 |

| Provider | Spawn ms | Accept ms | Welcome ms |
|---|---:|---:|---:|
| platform | 1.010 | 36.285 | 36.452 |
| storage | 0.226 | 33.055 | 41.855 |
| journal | 0.392 | 30.730 | 30.879 |

