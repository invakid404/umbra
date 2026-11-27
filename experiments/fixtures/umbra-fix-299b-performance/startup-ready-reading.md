# Startup after listener readiness

Fixed three warmups and one diagnostic for each workload; all eight attempts passed. Frozen source5b1f9648. Single readings do not qualify D6. Bounds unchanged.

| Workload | Prior warmed ms | Readiness wait ms |
|---|---:|---:|
| native_true | 152.688 | 62.041 |

## native_true, process 8106

| Stage | Offset ms | Since prior record ms |
|---|---:|---:|
| executable-entry | 2.475 | 2.475 |
| caller-validation-begin | 2.971 | 0.496 |
| caller-validation-end | 3.030 | 0.058 |
| launch-configuration-ready | 3.465 | 0.435 |
| root-relay-entry | 3.509 | 0.045 |
| spawn-begin | 4.514 | 1.005 |
| spawn-return | 4.739 | 0.224 |
| announcement-received | 18.180 | 13.442 |
| inventory-queued | 18.276 | 0.095 |
| readiness-received | 19.354 | 1.078 |
| readiness-queued | 19.411 | 0.058 |
| go-queued | 55.604 | 36.193 |

## native_true, process 8107

| Stage | Offset ms | Since prior record ms |
|---|---:|---:|
| executable-entry | 6.167 | 6.167 |
| configuration-validated | 7.129 | 0.962 |
| entry-adopted | 7.193 | 0.064 |
| configuration-decoded | 7.648 | 0.455 |
| dispatch-registered | 7.693 | 0.045 |
| spawn-begin | 8.618 | 0.924 |
| spawn-return | 8.824 | 0.207 |
| spawn-begin | 9.790 | 0.966 |
| spawn-return | 9.973 | 0.183 |
| announcement-received | 11.910 | 1.937 |
| announcement-received | 17.978 | 6.068 |
| announcement-queued | 18.061 | 0.083 |
| inventory-received | 18.371 | 0.310 |
| inventory-queued | 18.448 | 0.077 |
| inventory-queued | 18.534 | 0.086 |
| readiness-received | 18.727 | 0.193 |
| readiness-received | 19.142 | 0.415 |
| readiness-queued | 19.230 | 0.088 |
| go-received | 55.708 | 36.478 |
| go-queued | 55.798 | 0.090 |
| go-queued | 55.868 | 0.070 |

## native_true, process 8108

| Stage | Offset ms | Since prior record ms |
|---|---:|---:|
| executable-entry | 10.268 | 10.268 |
| configuration-validated | 11.221 | 0.953 |
| entry-adopted | 11.286 | 0.064 |
| configuration-decoded | 11.700 | 0.415 |
| dispatch-registered | 11.754 | 0.054 |
| announcement-queued | 11.827 | 0.073 |
| inventory-received | 18.549 | 6.723 |
| readiness-queued | 18.646 | 0.097 |
| worker-setup-begin | 19.147 | 0.501 |
| worker-validation-end | 19.197 | 0.049 |
| workspace-inventory-end | 19.314 | 0.118 |
| storage-connect-end | 21.331 | 2.017 |
| namespace-prepared | 44.605 | 23.274 |
| go-received | 61.979 | 17.374 |

## native_true, process 8109

| Stage | Offset ms | Since prior record ms |
|---|---:|---:|
| executable-entry | 11.450 | 11.450 |
| configuration-validated | 12.406 | 0.957 |
| entry-adopted | 12.454 | 0.048 |
| configuration-decoded | 12.853 | 0.399 |
| dispatch-registered | 12.893 | 0.040 |
| provider-launch-begin | 13.789 | 0.896 |
| provider-launch-end | 17.705 | 3.916 |
| announcement-received | 17.795 | 0.090 |
| announcement-queued | 17.875 | 0.080 |
| inventory-received | 18.657 | 0.782 |
| inventory-queued | 18.742 | 0.085 |
| readiness-received | 18.977 | 0.235 |
| readiness-queued | 19.050 | 0.073 |
| go-received | 55.947 | 36.898 |
| go-queued | 56.024 | 0.077 |

## native_true, process 8110

| Stage | Offset ms | Since prior record ms |
|---|---:|---:|
| executable-entry | 16.207 | 16.207 |
| configuration-validated | 17.140 | 0.933 |
| provider-accept-begin | 17.198 | 0.058 |
| provider-accept-end | 17.354 | 0.156 |
| announcement-queued | 17.446 | 0.092 |
| inventory-received | 18.843 | 1.397 |
| readiness-queued | 18.912 | 0.069 |
| go-received | 56.122 | 37.210 |

| registry_true | 259.782 | 180.985 |

## registry_true, process 8360

| Stage | Offset ms | Since prior record ms |
|---|---:|---:|
| executable-entry | 2.527 | 2.527 |
| caller-validation-begin | 3.037 | 0.510 |
| caller-validation-end | 3.097 | 0.060 |
| launch-configuration-ready | 3.557 | 0.460 |
| root-relay-entry | 3.606 | 0.049 |
| spawn-begin | 4.632 | 1.026 |
| spawn-return | 4.866 | 0.234 |
| announcement-received | 18.720 | 13.854 |
| inventory-queued | 18.827 | 0.106 |
| readiness-received | 19.865 | 1.038 |
| readiness-queued | 19.933 | 0.068 |
| go-queued | 171.947 | 152.014 |

## registry_true, process 8361

| Stage | Offset ms | Since prior record ms |
|---|---:|---:|
| executable-entry | 6.349 | 6.349 |
| configuration-validated | 7.349 | 1.000 |
| entry-adopted | 7.410 | 0.061 |
| configuration-decoded | 7.837 | 0.427 |
| dispatch-registered | 7.878 | 0.042 |
| spawn-begin | 8.881 | 1.003 |
| spawn-return | 9.112 | 0.231 |
| spawn-begin | 10.120 | 1.008 |
| spawn-return | 10.317 | 0.197 |
| announcement-received | 12.347 | 2.030 |
| announcement-received | 18.534 | 6.186 |
| announcement-queued | 18.619 | 0.086 |
| inventory-received | 18.907 | 0.288 |
| inventory-queued | 18.982 | 0.074 |
| inventory-queued | 19.055 | 0.074 |
| readiness-received | 19.207 | 0.151 |
| readiness-received | 19.632 | 0.425 |
| readiness-queued | 19.739 | 0.107 |
| go-received | 172.050 | 152.311 |
| go-queued | 172.132 | 0.082 |
| go-queued | 172.190 | 0.058 |

## registry_true, process 8362

| Stage | Offset ms | Since prior record ms |
|---|---:|---:|
| executable-entry | 10.645 | 10.645 |
| configuration-validated | 11.648 | 1.003 |
| entry-adopted | 11.707 | 0.059 |
| configuration-decoded | 12.145 | 0.438 |
| dispatch-registered | 12.196 | 0.051 |
| announcement-queued | 12.266 | 0.070 |
| inventory-received | 19.071 | 6.805 |
| readiness-queued | 19.140 | 0.069 |
| worker-setup-begin | 19.640 | 0.500 |
| worker-validation-end | 19.703 | 0.063 |
| workspace-inventory-end | 19.856 | 0.154 |
| storage-connect-end | 69.373 | 49.517 |
| namespace-prepared | 162.532 | 93.159 |
| go-received | 180.915 | 18.383 |

## registry_true, process 8363

| Stage | Offset ms | Since prior record ms |
|---|---:|---:|
| executable-entry | 11.808 | 11.808 |
| configuration-validated | 12.780 | 0.972 |
| entry-adopted | 12.835 | 0.055 |
| configuration-decoded | 13.246 | 0.411 |
| dispatch-registered | 13.287 | 0.041 |
| provider-launch-begin | 14.247 | 0.960 |
| provider-launch-end | 18.301 | 4.054 |
| announcement-received | 18.398 | 0.098 |
| announcement-queued | 18.464 | 0.066 |
| inventory-received | 19.130 | 0.666 |
| inventory-queued | 19.206 | 0.076 |
| readiness-received | 19.439 | 0.234 |
| readiness-queued | 19.523 | 0.084 |
| go-received | 172.301 | 152.778 |
| go-queued | 172.382 | 0.081 |

## registry_true, process 8364

| Stage | Offset ms | Since prior record ms |
|---|---:|---:|
| executable-entry | 16.777 | 16.777 |
| configuration-validated | 17.752 | 0.975 |
| provider-accept-begin | 17.808 | 0.057 |
| provider-accept-end | 17.970 | 0.162 |
| announcement-queued | 18.067 | 0.097 |
| inventory-received | 19.308 | 1.241 |
| readiness-queued | 19.371 | 0.063 |
| go-received | 172.500 | 153.129 |

