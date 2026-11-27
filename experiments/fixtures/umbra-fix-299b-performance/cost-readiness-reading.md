# Resource inventories after readiness

The observer no longer pauses startup allocation or executable entry. Both sides use the same run-ready boundary before launching the command; managed builds also record worker GO. These counts are not startup maxima or latency readings.

The fixed schedule completed four warmups, four measured invocations, and pre-GO library rollback. All nine passed. Birth-identity exclusion, exact caller descriptor return and final host process census checks passed; no observed owned process or zombie survived.

| Measured invocation | Product processes | Product descriptors | Extra helpers | Extra descriptors beyond helper standard I/O |
|---|---:|---:|---:|---:|
| Baseline CLI | 4 | 26 | 0 | 0 |
| Managed CLI | 7 | 56 | 3 | 21 |
| Baseline library | 4 | 26 | 0 | 0 |
| Managed library | 8 | 70 | 4 | 32 |

The external measurement driver and identified observer processes are excluded. Managed descriptor deltas are 30 and 44 overall; subtracting the additional helpers' nine and twelve standard I/O descriptors gives 21 and 32, within the owner-approved 22 and 33 ceilings. Full inventories remain retained. The library caller had three descriptors both before preparation and after return, including rollback. Every managed CLI role returned with three descriptors.

Head b6669726 and baseline c85722bf use matching observation changes. Production startup remains bounded at two seconds. The earlier startup timeout remains failed evidence. No startup peak count is claimed from the new method.
