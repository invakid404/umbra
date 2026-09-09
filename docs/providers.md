# Runtime providers

Build installed executables with `cargo build --workspace --bins`. Each of the eight
implementation packages supplies a `provider.json` installation template. Copy the
selected descriptors into a JSON registry keyed by contract role, set each executable
to an explicit absolute path, and configure opaque options. No PATH scan or Rust backend
import is used by the CLI. IDs are open strings; roles name the five contracts.

For example, create a registry for the local development storage provider:

```python
import json
from pathlib import Path
root = Path.cwd()
descriptor = json.loads((root / "crates/umbra-storage-local/provider.json").read_text())
descriptor["executable"] = list(bytes(root / "target/debug/umbra-storage-local"))
# Options are bytes containing provider-specific JSON, opaque to the CLI.
descriptor["options"] = list(json.dumps(list(b"/tmp/umbra-local-dev")).encode())
Path("/tmp/umbra-providers.json").write_text(json.dumps({
    "timeout_ms": 5000, "providers": {"storage": descriptor}
}))
```

Run `target/debug/umbra providers --registry /tmp/umbra-providers.json --role storage`.
The command uses the real contract proxy and handshake, then closes the provider.
It starts no tracee or run; constructing local storage creates its configured directory.
Without `--role`, the same command calls supervisor assembly and requires platform
and agent descriptors. Standard overlay assembly also requires storage and journal;
a configured namespace provider replaces that branch and owns those connections.
Alternative namespace providers can decode the core
`NamespaceConnections` DTO from their options, connect its storage/journal descriptors
through the contracts re-exported by overlay, and inject their own engine. This avoids
opening duplicate role sessions in CLI assembly.

Storage-local and storage-nfs options encode a physical-root BytePath as JSON byte arrays.
Storage-tar options encode an absolute archive filename BytePath in the same format.
Its parent directory must exist; no mount, service, or environment variables are needed.

| Storage provider | Options (JSON BytePath) | Persistence |
| --- | --- | --- |
| `local` | Local directory | Local filesystem |
| `nfs` | Existing NFSv4 mount root | Client fsync; remote durability unqualified |
| `tar` | Absolute tar archive filename | Indexed run with persistent staging; flush publishes a locally fsynced tar. Retry budget allows ~3 MiB cumulative written bytes per archive for three-digit byte values (capacity varies with payload/metadata); not reclaimed by flush or reopen. |

For tar, use `crates/umbra-storage-tar/provider.json` and the
`target/debug/umbra-storage-tar` executable in the registration example above, and
encode an archive filename such as `/tmp/umbra-run.tar` as options. See the
[tar provider README](../crates/umbra-storage-tar/README.md) for its format and limits.

Codex options encode `CodexConfig`; the template's `UNQUALIFIED` version is a placeholder
requiring replacement and qualification before runtime use. Claude, file journal and
the Linux platform stub require empty options. The macOS provider accepts JSON
`Options { debugserver, twin_cache, timeout_ms }`, or empty options for defaults;
it implements experimental arm64 tracing and advertises `darwin-arm64-abi-v1`.
Descriptor capabilities are required open
capability names, verified against the provider handshake. Stubs advertise no runtime
support. An empty capability requirement permits connection testing, never authorization
to run a tracee. The supervisor's operational methods still return NotImplemented.

Each role's `provider` module owns Request/Response types, a generic `serve_provider`
harness and a proxy (`connect` returns paired platform proxies). A new backend package
implements its existing contract, calls that harness from main, and installs a descriptor.
No established trait, CLI branch, backend dependency or feature must be edited.
Common registration and framing are implemented in `umbra-core::provider`, independent
of role implementations. All implementation packages keep only core and their own
contract as direct Umbra dependencies.

The transport uses a Unix socket in a private temporary directory, a version/identity/
role/capability handshake, monotonically increasing request IDs, owned JSON DTOs and
big-endian four-byte length prefixes. Frames are capped at 32 MiB before allocation;
individual storage writes and trace-memory transfers are capped at 1 MiB. The frame
limit includes JSON's byte-array expansion and transport envelope overhead. Only one
ordinary request is outstanding per connection, providing backpressure. Configured I/O
deadlines are bounded to 1–60000 ms; malformed, disconnected or timed-out connections
are invalidated. No failed call is retried automatically or redirected to host writes.
Private IPC descriptors use Rust's non-inheritable Unix descriptors. Provider processes
are trusted components, not tracees; future launch implementations must still sanitize
all inherited resources and independently enforce controller/provider-death behavior.

Platform decode supports at most 256 memory callbacks per request. No client borrow or
connection lock is held while TraceMemory runs. A callback may issue nested read-only
control requests on the paired proxy; the server services them while awaiting the
callback result. Mutating nested requests are rejected. Journal replay uses an exclusive
session cursor and one bounded record per page, propagates streaming errors and releases
the cursor on early iterator drop. Records never accumulate unboundedly in the proxy.

CI checks Rust targets and doctests on Linux and macOS. These checks do not qualify
production tracing coverage, kernel enforcement, remote NFS durability or vendor
agent behavior. The Rust macOS tracer and NFS/local storage are implemented;
the Linux tracer, file journal and agent adapters remain stubs. Environment-gated
fixture and mounted-NFS tests require explicit executed verdicts for qualification.

The common transport is version 2. Installation descriptors must set
`protocol_version` to 2; version 1 peers are refused during handshake rather than
at decoding the required sandbox launch policy or rewrite messages.
