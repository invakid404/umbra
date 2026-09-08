# umbra-agent-claude

Claude adapter scaffold. Declares methods for launch/resume/stop plans and observations
through the Agent contract; current fallible methods return NotImplemented and capabilities
are empty. It never launches processes or invents session IDs. Logical configuration,
project and temporary paths must remain stable across handoff; credentials remain external.
The provider accepts no options. Agent invocation, pinned versions, session discovery,
history retention and restart behavior still need implementation and qualification.

The package supplies its own provider binary and `provider.json` installation template.
See [provider setup](../../docs/providers.md) and the corresponding trait walkthrough.

Check with `cargo check -p umbra-agent-claude`.
