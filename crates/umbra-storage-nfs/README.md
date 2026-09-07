# umbra-storage-nfs

Mounted NFS storage scaffold. Construction stores an existing mount root but neither
mounts nor validates an export. Required operations return NotImplemented and capability
reporting claims no supported behavior. Default Storage helpers retain their shared
bounds/authority preflight checks; they are not overridden to bypass validation.
Provider options are JSON encoding of the mount-root BytePath. Remote durability,
fencing and lifecycle semantics require a real implementation and NFS qualification.

The package supplies its own provider binary and `provider.json` installation template.
See [provider setup](../../docs/providers.md) and the corresponding trait walkthrough.

Check with `cargo check -p umbra-storage-nfs`.
