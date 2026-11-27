# Startup resource observations after trimming

Source14108c96 releases the provider's three child-only parent copies immediately after spawn, before RPC acceptance or handshake. The provider releases its temporary readiness queue after timely readiness publication, before waiting through namespace preparation. The managed transport controls pass; both new CLI and registered-library census runs passed their original success, callback, custody and collection checks. The22/33 managed-descriptor bounds remain unchanged and are still exceeded.

| Role | CLI own observed maximum | Library own observed maximum |
|---|---:|---:|
| Caller | part of Relay |27|
| Relay |19|24|
| Guardian |18|21|
| Worker |13|14|
| CleanupOwner |13|14|
| Provider |12|12|

These maxima include each process's three standard descriptors and occur at different times; do not sum them. The largest complete sequential census before Worker GO observed52 descriptors across five CLI participants and67 across six library participants. Query spans were33.958 and36.375 microseconds, with no invalid identity queries in either startup reading. At those observations the counts were CLI Relay7/Guardian9/Worker13/CleanupOwner12/Provider11, and library Caller9/Relay9/Guardian10/Worker15/CleanupOwner13/Provider11. Concurrent queries can observe a descriptor absent from a process's earlier self-observation.

The conservative managed lower bound is still24 for CLI: Relay4+Guardian6+CleanupOwner9 sockets/queues, plus only the Worker's three topology endpoints and Provider's two topology endpoints. Library is still35: Caller6+Relay6+Guardian7+CleanupOwner10 sockets/queues, plus only four Worker topology endpoints and two Provider topology endpoints. Both exceed22/33. This intentionally excludes other Worker/Provider bootstrap and queue descriptors; their legacy RPC, storage and tracer resources are not claimed as savings. Helper standard I/O remains separately nine for CLI and twelve for library.

The instrument opens no observer process and pauses no role. It records each process's descriptors before opening its output file, then queries discovered participants sequentially with birth checks. A simultaneous output file in another participant can appear in total counts; socket/queue-only accounting for the guard and owner roles avoids crediting those output files as managed sockets. The inventories are observations, not an atomic snapshot or an absolute upper bound. A newly spawned child may not yet have emitted a discoverable record. Legacy storage/journal/debugger processes are outside the managed-role census. None of these limitations permits a passing startup claim from this result.

Evidence is retained under checkpoint12-census-cli and checkpoint12-census-library, with summaries checkpoint12-census-{cli,library}-summary.json, original inventories, stage records, identities and before/after host censuses. CLI used the frozen checkpoint12-census-artifacts manifest. Cargo selected a separately compiled library driver for its reading; checkpoint12-census-library-artifact-receipt.json records that actual executable and its retained copy, and verifies all three actual provider hashes against the original manifest. The originally frozen library driver is also retained and is not claimed to have executed. This metadata distinction does not change the observed descriptor records. No D6 timing acceptance is claimed.
