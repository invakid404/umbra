# umbra-overlay

Storage-independent overlay policy and M0 namespace scaffolding. Direct Umbra
dependencies are `umbra-core`, `umbra-storage`, and `umbra-journal`; no backend
implementation is selected or imported here.

`NamespaceResolver::resolve(&mut self, &ProcessContext, &FsOp)` returns
`umbra_core::Result<ResolvedAction>`. Construct `Overlay::new(Box<dyn Storage>,
Box<dyn Journal>)` with injected contracts. `Storage` and `Journal` are re-exported
for alternative namespace implementations.

`dispatch` classifies read-through, materialisation, whiteouts, merged enumeration,
and process state. Writes, create/append/truncate opens, metadata changes, rename,
hard links, and shared writable mappings require shadow preparation. Read-through
must prefer the shadow, honor whiteouts, then consult the immutable base.
`components` preserves raw path bytes, dot/parent components, and separators;
tokenization alone does not establish containment.

`NamespaceSession: NamespaceResolver` exposes prepare, observe_result, commit, abort,
and checkpoint using shared core DTOs. `standard_namespace(storage, journal)` returns
`Box<dyn NamespaceSession + Send>`. All standard-engine execution methods currently
return `ErrorKind::NotImplemented`; they perform no filesystem mutations or base syscalls.
Anchored lookup, symlink traversal, descriptor provenance, transactions, directory
merging and recovery remain unimplemented. Unimplemented low-level copy-up/whiteout
helpers are no longer exposed as a competing concrete-only public API.

`provider` owns the namespace protocol, generic server harness, and client proxy.
An alternative provider may decode `NamespaceConnections` from its opaque options
and connect the supplied storage/journal descriptors, then inject those contracts.

## Adding a new namespace backend

1. Create `crates/umbra-overlay-<name>` and add its workspace member. Inherit version,
   authors, license, and edition. Its only direct Umbra dependencies are
   `umbra-overlay` and `umbra-core`.
2. Implement `NamespaceResolver` and `NamespaceSession` prepare/observe-result/commit/abort/
   checkpoint extension. Resolve without mutation, durably prepare intent before
   irreversible effects, and commit namespace changes only after observed success.
   Abort must reconcile executed writes instead of claiming they were undone.
3. Inject `umbra_overlay::Storage` and `umbra_overlay::Journal`, or connect their
   role endpoints. Validate that they refer to the same run and fenced writer.
   Walk byte components from anchored root/cwd/dirfd handles, bound symlink
   traversal, retain logical symlink targets, and confine parents to the logical
   root. Preserve hard-link identity. Never fall back to host writes.
4. Export a provider binary using `provider::serve_provider` and
   register its versioned descriptor and executable in runtime role configuration.
   Do not add backend branches or dependencies to the supervisor or CLI. Use `umbra providers --registry PATH --role namespace` to check it.
5. Start with `cargo test -p umbra-overlay` for flag classification and byte
   preservation. Before enabling execution, add conformance coverage for copy-up
   metadata, shadow/read-through precedence, whiteouts, cwd/dirfds, non-UTF-8 paths,
   symlink escapes and loops, hard links, stable merged enumeration, pathless writes,
   concurrent process state, mount-root changes on resume, and journal crash
   boundaries. Unsupported operations must fail before mutation. M0 unit tests
   are not backend qualification or a complete conformance harness.
