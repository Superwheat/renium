# Renium

Renium is a fast, two-way Roblox Studio sync and automation tool written mostly
in Rust. Bug reports and suggestions are welcome.

## Components

- [`tools/renium`](tools/renium) — the Rust CLI and daemon. Full reference in its [README](tools/renium/README.md).
- [`tools/renium-vscode-extension`](tools/renium-vscode-extension) — the VS Code/Cursor extension with a virtualized explorer, live sync, and Git integration.
- [`tools/plugin_ws_bridge`](tools/plugin_ws_bridge) — the Roblox Studio plugin that bridges Studio to the CLI over WebSockets.

## Getting started

1. Download **Install-Renium.cmd** on Windows, or the matching platform ZIP on
   macOS/Linux, from [GitHub Releases](https://github.com/Superwheat/renium/releases/latest).
2. Restart the selected editor and Roblox Studio.
3. See [tools/renium/README.md](tools/renium/README.md) for the command reference, and [AGENTS.md](AGENTS.md) if you are pointing an AI agent at it.

Signed updates install matching CLI, extension, and Studio plugin versions.

To build from source, run `cargo build --release --manifest-path tools/renium/Cargo.toml`.
Built executables, VSIX files, and plugin models aren't stored in Git.

## License

Licensed under [AGPL-3.0 with the Commons Clause](LICENSE). Commercial game
development is allowed. Forks must stay open source; selling Renium or paid
hosting/support isn't permitted.
