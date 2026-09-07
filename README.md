# Renium

Two-way Roblox Studio sync and automation. Edit code in VS Code or Cursor,
build in Studio, and keep both in the same project.

## Install

Download the Windows installer or your platform's ZIP from
[Releases](https://github.com/Superwheat/renium/releases/latest).
Install, then restart your editor and Studio.

[Get started and browse commands](tools/renium/README.md).
Agents use the generated `RENIUM.md` in their project.

## Components

- [CLI and daemon](tools/renium): project tools, sync, and automation.
- [VS Code/Cursor extension](tools/renium-vscode-extension): Explorer, Live Sync, and Git.
- [Studio plugin](tools/plugin_ws_bridge): the Studio connection.

Build from source:

```powershell
cargo build --locked --release --manifest-path tools/renium/Cargo.toml
```

Builds and release bundles are not stored in Git.
For contributor guidance, read [AGENTS.md](AGENTS.md).

[Report a bug](https://github.com/Superwheat/renium/issues) ·
[License](LICENSE)
