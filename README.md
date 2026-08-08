# Renium

Renium is a high-fidelity two-way sync tool for Roblox Studio made in Rust, designed to sync large places quickly. Built with AI in mind: a token-efficient CLI lets agents read, edit, screenshot, and playtest your game. It ships with an automation API and a VS Code extension and is under active development. See the documented limitations before relying on a property type in release-critical workflows.

## Components

- [`tools/renium`](tools/renium) — the Rust CLI (`renium.exe`) and daemon. Full reference in its [README](tools/renium/README.md).
- [`tools/renium-vscode-extension`](tools/renium-vscode-extension) — VS Code extension with a virtualized explorer, live sync, and git integration.
- [`tools/plugin_ws_bridge`](tools/plugin_ws_bridge) — the Roblox Studio plugin (`Renium.rbxm`) that bridges Studio to the CLI over WebSockets.

## Getting started

1. Build the CLI: `cargo build --release --manifest-path tools/renium/Cargo.toml`
2. Install the Studio plugin: `renium setup` (or copy `tools/plugin_ws_bridge/Renium.rbxm` into your Roblox `Plugins` folder). On macOS, setup also prepares `~/Applications/Renium Studio.app`; open that app so protected properties can sync without a save dialog.
3. See [tools/renium/README.md](tools/renium/README.md) for the command reference, and [AGENTS.md](AGENTS.md) if you are pointing an AI agent at it.

## License

Licensed under [AGPL-3.0 with the Commons Clause](LICENSE): free for everyone, including commercial game development. Forks are welcome and must stay open source; selling the software (or paid hosting/support built on it) is not permitted.
