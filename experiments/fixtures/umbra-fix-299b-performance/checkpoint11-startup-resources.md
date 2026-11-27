# Startup resource observations

The explicit startup instrument counts descriptors inside each emitting process before opening its output file. It also queries current descriptors of event-discovered participants, checking birth identities and recording each query interval. It launches no observer, waits for no acknowledgment and pauses no role. Calibration verifies exact original inventory, two added descriptors, closure return and rejection of a stale birth. Original two-second startup cutoff is unchanged. Both CLI and registered-library readings passed existing collection and guard checks.

| Role | CLI processes / observed descriptors | Library processes / observed descriptors |
|---|---:|---:|
| Caller | part of Relay | 1 / 27 |
| Relay | 1 / 19 | 1 / 24 |
| Guardian | 1 / 18 | 1 / 21 |
| Worker | 1 / 13 | 1 / 14 |
| CleanupOwner | 1 / 14 | 1 / 15 |
| Provider | 1 / 12 | 1 / 12 |

Each listed process has three standard-I/O descriptors. These maxima occur at different stages and are not summed. The largest direct pre-GO census observed 54 descriptors across five CLI participants and 67 across six library participants. Query spans were 27.667 and 102.750 microseconds; neither reading had an incomplete startup census. At those observations the role counts were CLI Relay7, Guardian9, Worker14, CleanupOwner12, Provider12; library Caller9, Relay9, Guardian10, Worker14, CleanupOwner13, Provider12.

The count comparison already fails conservatively. CLI Relay/Guardian/CleanupOwner have 4+6+9=19 non-stdio descriptors, all introduced by managed execution. Add only the Worker's three topology endpoints and the Provider's admission/cancellation endpoints: at least24 managed descriptors, above22. For library, Caller/Relay/Guardian/CleanupOwner have 6+6+7+10=29 non-stdio descriptors. Add only four Worker topology endpoints and two Provider endpoints: at least35, above33. The topology endpoints follow `managed/topology.rs`; this lower-bound accounting intentionally leaves out additional Worker/Provider bootstrap and queue descriptors, along with existing RPC and tracer resources. Helper standard I/O remains separately nine for CLI and twelve for library, matching the three/four added helpers. No limit is raised and no passing startup claim is made.

Coverage limits: these are observed sequential kernel censuses, not atomic snapshots or a proof of an absolute maximum. A newly spawned child may not yet have published its first record, and a descriptor can change between queries. Storage/journal/debugger processes are outside the discovered managed-role census. Their legacy resources are not credited as savings. Parent-held inherited copies during executable startup may create higher brief peaks. Therefore54/67 are observed totals for the listed participants, not complete whole-product upper bounds. These limitations cannot turn the demonstrated overage into a pass. The earlier readiness21/32 remains a separate, passing steady-state reading.

Raw inventories, identities, stage records, query intervals, manifests and before/after host censuses remain under checkpoint11-census-cli and checkpoint11-census-library in the retained evidence directory. Earlier separate-process readings remain preserved. No timing acceptance claim follows from instrumentation readings.
