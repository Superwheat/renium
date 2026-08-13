# About the project

Renium is a Roblox daemon sync tool, written mostly in rust. It was built to solve multiple issues, long sync times in other similar tools like Argon/Rojo and the inefficiencies of Roblox MCP, such as its token hungry nature and slow commands.
It is still in active development and any suggestions/bug reports are greatly appreciated.

## Components

- [`tools/renium`](tools/renium) — the Rust CLI and daemon. Full reference in its [README](tools/renium/README.md).
- [`tools/renium-vscode-extension`](tools/renium-vscode-extension) — the VS Code/Cursor extension with a virtualized explorer, live sync, and Git integration.
- [`tools/plugin_ws_bridge`](tools/plugin_ws_bridge) — the Roblox Studio plugin that bridges Studio to the CLI over WebSockets.

## Getting started

1. Download the VSIX for your platform from [GitHub Releases](https://github.com/Superwheat/renium/releases/latest) and install it in VS Code or Cursor.
2. Run **Renium: Install Studio Plugin** from the command palette.
3. See [tools/renium/README.md](tools/renium/README.md) for the command reference, and [AGENTS.md](AGENTS.md) if you are pointing an AI agent at it.

Renium's Rust updater checks the signed GitHub Release manifest when the editor
opens. The daemon also checks when a Studio process first connects and suppresses
its reconnects for five minutes. The update notification installs the matching
extension and Studio plugin together.

To build from source, run `cargo build --release --manifest-path tools/renium/Cargo.toml`.
Generated binaries, VSIX packages, and plugin models are release artifacts and
are not stored in the source repository.

## License

Licensed under [AGPL-3.0 with the Commons Clause](LICENSE): free for everyone, including commercial game development. Forks are welcome and must stay open source; selling the software (or paid hosting/support built on it) is not permitted.
