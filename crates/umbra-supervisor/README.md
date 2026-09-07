# umbra-supervisor

Synchronous supervisor assembly depending only on core and the five contracts.
`Supervisor::new(run_id, platform, storage, journal, agent)` accepts a `RunId`, a paired
`PlatformSession`, boxed `Storage`, boxed `Journal`, and boxed `Agent`. It delegates
namespace construction to `umbra_overlay::standard_namespace`.

`Supervisor::with_namespace(run_id, platform, namespace, agent)` accepts an alternative
`Box<dyn NamespaceSession + Send>`. `SupervisorParts` is returned by `into_parts()`;
it contains platform, namespace, agent and runtime state, including the supplied run ID.
Construction performs no I/O or backend operations and starts in `RunLifecycle::Created`.

The namespace contract includes prepare, observed results, commit, abort and checkpoint.
Operational supervisor methods still return `NotImplemented`: execution, quiescence,
recovery and handoff ordering are documented obligations, not implemented behavior.
Check with `cargo check -p umbra-supervisor`.
