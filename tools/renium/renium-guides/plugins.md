# Plugins

Plugins add commands to Renium without changing its source: `rbx <PLUGIN> <COMMAND>`. Install only trusted code: native plugins have your account's filesystem/network access. Manifest permissions describe capabilities; they are not an OS security sandbox.

```powershell
rbx plugin list
rbx plugin info NAME
rbx <PLUGIN> --help
```

Read `info` for a plugin's guide, permissions and commands before using it. Listing, help and info do not execute plugin code. Put global Renium options (`--project`, `--place`, `--output-mode`) before the plugin name. A plugin's `--session ID` identifies a task; `RENIUM_SESSION_ID` or `CODEX_THREAD_ID` can supply it.

Use a plugin only when it helps the requested task, and follow the normal Renium guides for sync, input and capture. Access to an extra Studio or test environment is not a reason to launch Play. A failed plugin command reports what to do next; do not work around its leases or journals.

## Author a plugin

```powershell
rbx plugin new my-plugin
rbx plugin check ./my-plugin
rbx plugin install ./my-plugin --dev
rbx plugin remove my-plugin
```

The starter contains a manifest, Rust handler, guide and local SDK. Edit the handler and define commands/arguments once in `renium-plugin.json`; Renium generates help and validates input. Build from that folder with `cargo build --release` before installing. Install never builds or runs code. `check` validates source metadata without needing a binary.

`--dev` allows local edits/rebuilds without reinstalling. Normal installs pin the manifest and entry executable; reinstall after changing either. Dependencies loaded by that executable are still trusted code, not a sealed package. Removal unregisters the plugin but preserves its source, state and leases.

The SDK passes the exact Renium executable, workspace/project/target, task ID and state directory. `ctx.renium(...)` calls existing commands with structured results and a 20-second deadline; `ctx.renium_timeout(...)` takes a longer one for pushes, launches or captures. `ctx.snapshot(path)` creates an isolated flattened project using Renium's projection engine. Resource leases persist through interruptions; cleanup must succeed before releasing one. Studio place leases are checked on daemon binding and subsequent operations.

Commands run in separate processes and return JSON. Each command declares `timeoutSeconds` (default 20, up to 3600), and the invocation carries that budget; start a long step only when the remaining time covers its deadline, otherwise return at a saved step. Split long work into durable, resumable steps. Keep diagnostics on stderr; stdout belongs to the protocol. Do not spawn detached helpers to escape timeouts or let unfinished cleanup masquerade as success.
