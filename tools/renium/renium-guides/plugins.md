# Workflow plugins

Plugins add commands without changing Renium's source. Install only trusted code: native plugins have your account's filesystem/network access. Manifest permissions describe capabilities; they are not an OS security sandbox.

```powershell
rbx plugin list
rbx plugin info NAME
rbx <PLUGIN> --help
```

Read `info` for the plugin's workflow, permissions and instructions before using it. Listing, help and info do not execute plugin code. Put global Renium options (`--project`, `--place`, `--output-mode`) before the plugin name. The plugin's `--session ID` identifies a task; `RENIUM_SESSION_ID` or `CODEX_THREAD_ID` can supply it.

Choose a plugin only when it helps the requested task. Access to a test environment is not a reason to launch Play. Keep offline checks offline, and follow the normal Renium guides for sync, input and capture.

## Author a plugin

```powershell
rbx plugin new my-workflow
rbx plugin check ./my-workflow
rbx plugin install ./my-workflow --dev
rbx plugin remove my-workflow
```

The starter contains a manifest, Rust handler, guide and local SDK. Edit the handler and define commands/arguments once in `renium-plugin.json`; Renium generates help and validates input. Build from that folder with `cargo build --release` before installing. Install never builds or runs code. `check` validates source metadata without needing a binary.

`--dev` allows local edits/rebuilds without reinstalling. Normal installs pin the manifest and entry executable; reinstall after changing either. Dependencies loaded by that executable are still trusted code, not a sealed package. Removal unregisters the plugin but preserves its source, state and leases.

The SDK passes the exact Renium executable, workspace/project/target, task ID and state directory. `ctx.renium(...)` calls existing commands with structured results. `ctx.snapshot(path)` creates an isolated flattened project using Renium's projection engine. Resource leases persist through interruptions; cleanup must succeed before releasing one. Studio place leases are checked on daemon binding and subsequent operations.

Commands run in separate processes, default to a 20-second deadline and return JSON. Split long workflows into durable, resumable steps. Keep diagnostics on stderr; stdout belongs to the protocol. Do not spawn detached helpers to escape timeouts or let unfinished cloud cleanup masquerade as success.
