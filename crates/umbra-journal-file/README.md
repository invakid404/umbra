# umbra-journal-file

File journal scaffold implementing the Journal contract. All runtime methods return
NotImplemented without opening files, changing buffers or claiming persistence.
Future framed log records require bounded payloads, CRC32 checksums, sequence and writer
epoch validation; recovery must distinguish torn tails from interior corruption.
Checkpoint publication must follow durable data/log flushes. This provider accepts no
options; runtime control directories and writer authority arrive through Journal::open.
CRC32 integration will be added with actual file framing, not as an unused dependency.

The package supplies its own provider binary and `provider.json` installation template.
See [provider setup](../../docs/providers.md) and the corresponding trait walkthrough.

Check with `cargo check -p umbra-journal-file`.
