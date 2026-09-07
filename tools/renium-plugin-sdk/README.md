# Renium plugin SDK

Use `rbx plugin new NAME` for a standalone Rust starter with this SDK included. No package registry or Renium source edits are required.

The handler receives `Invocation { command, arguments, context }` and returns a JSON value through `serve`. Define commands, typed flags, defaults and help in `renium-plugin.json`; the host validates them before launching your executable. stdout is a single protocol response; diagnostics go to stderr.

`context` provides the exact host executable, workspace, optional project/place/task ID, plugin directory and persistent state directory. `context.renium(&[...])` calls existing commands with JSON output and a deadline. `context.snapshot(destination)` creates a new flattened project with separate sync metadata. No Studio changes are involved in snapshotting.

For reusable resources, `lease::Registry` provides exclusive claims and durable journals. Keep an operation-level file lock when a workflow spans multiple calls. Store progress before external writes. Release a claim only after verified cleanup; never use a TTL to make dirty resources available. Attach a Studio place claim to a cloned context's `resource_lease` to use the protected target. Tokens must not appear in user output or logs.

This is process isolation, not a security boundary. A trusted native plugin can access the user's files and network directly; listed permissions do not constrain arbitrary native code. Version 1 supports Windows, macOS and Linux hosts, with each plugin declaring the platforms it supports. Studio requires Windows/macOS.
