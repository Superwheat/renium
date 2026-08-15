# Changelog

## Unreleased

## 0.1.8 - 2026-08-15

- Replaced untyped daemon command forwarding with a versioned operation registry, bound project/place contexts, stable errors, and review receipts.
- Reused one Studio daemon across editor windows and direct `rbx` commands, and waited for the matching Studio runtime before runtime-bound operations.
- Fixed pulls when Roblox omits engine-generated descendants while cloning and serializing a Studio tree.
- Made signed updates component-aware so the CLI, extension, and Studio plugin stay on compatible versions during installation and rollback.
- Added read-only `.rbxm` and `.rbxmx` inspection through `rbx view` and stdin support for batched project reads.
- Shortened the generated `RENIUM.md`, removed temporary command-payload files, and clarified ordinary Play, local-server, multiplayer, and Luau runtime selection.
- Split the daemon and automation implementation into focused modules while removing duplicated parsing and dispatch code.
- Removed the missing-property-database warning from installed builds that use the plugin's bundled schema.

## 0.1.7 - 2026-08-15

- Changed exact-window recordings from animated WebP images to H.264 MP4 clips on every supported platform.
- Kept normal pulls on Studio's plugin serializer and ignored Roblox-reserved service attributes that third-party plugins cannot recreate.
- Kept automatic plugin reconnection active across Edit, Play server, and Play client states with bounded retry delays.
- Fixed repeated Play start and stop cycles, multi-client runtime selection, stale runtime pins, and false duplicate-launch errors.
- Added a targeted cross-platform input shield with an orange viewport outline and the current Renium version while preserving system window switching.
- Added shielded plugin virtual input for Linux and retained exact-window native input on Windows and macOS.
- Fixed typed automation requests for plural instance properties and attributes, settings-ID editor mutations, live status, bound project validation, and Revert history.
- Returned useful Revert results, normalized Windows script paths, and bounded documentation snippets from minified Roblox pages.
- Prevented Play clients from creating edit-session locks and fixed console filtering results.
- Kept editor update prompts open until the user chooses **Update**, **Later**, or closes the prompt.

## 0.1.6 - 2026-08-14

- Restored a minimal `renium.project.jsonc` marker in every place root and create missing place markers during binding and place setup.
- Included every serialized instance in sourcemaps while retaining source-file paths for scripts.
- Fixed cached update checks when GitHub returns `304 Not Modified` and added **Check for Updates** to the editor menu.
- Moved the generated agent guide to `RENIUM.md` and kept project-owned instructions in `AGENTS.md`.
- Added one Unicode-marked guide instruction to `AGENTS.md` and to `CLAUDE.md` when Claude's file doesn't already refer to `AGENTS.md`.
- Fixed generated GitHub release notes so the change list appears directly below **What's Changed**.

## 0.1.5 - 2026-08-14

- Moved Renium's agent instructions into one packaged Markdown file instead of embedding the full guide in Rust and JavaScript source.
- Kept generated project instructions named `AGENTS.md` and the Claude pointer named `CLAUDE.md`.
- Included the agent guide in platform archives, editor extensions, installers, repairs, and signed updates.
- Updated generated instructions to use the installed `renium` and `rbx` commands from `PATH`, with stable fallback paths for shells opened before installation.
- Removed the unrelated snapshot-importer tagline from the CLI help header.

## 0.1.4 - 2026-08-13

- Added normal platform ZIP installers with short installation instructions and direct launchers.
- Added editor selection to the Windows and macOS installers, with an explicit exit option.
- Made the Windows launcher install directly from an open ZIP and put `renium` on the user PATH.
- Centralized signed update checks in Rust with a shared five-minute cache and ETag revalidation.
- Made every new Studio runtime receive the cached update result without repeating GitHub requests.
- Removed direct plugin downloads from the editor extension and kept matching CLI, extension, and plugin versions together.

## 0.1.3 - 2026-08-13

- Added batched Roblox Open Cloud requests with bound universe and place expansion, escaped path parameters, conditional requests, retry metadata, and Data Store support.
- Added Creator Store and user-inventory search, supported asset insertion, model generation jobs, local image validation, image upload, and official Roblox documentation reads.
- Added script search, saved-source reads, literal grep, exact multi-edit operations, temporary camera capture, and adjustable pathfinding speed to the agent API.
- Added ordered keyboard and mouse sequences sent only to the selected Studio window without moving the system cursor or taking focus.
- Added exact-window Studio and play-client recording as animated WebP clips.
- Reorganized the Rust crate into feature-focused modules and updated release validation for the new layout.
- Removed generated Studio plugin bundles from source control; release and extension builds now create them from the Luau source.
- Added startup update notifications and one-click signed updates for the installed editor extension and Studio plugin.

## 0.1.2 - 2026-07-10

- Added experience projects with separate place roots, exact GameId/PlaceId routing, active-place switching, and renameable aliases.
- Bundled the matching Renium CLI and `rbx` launcher with the extension so projects no longer need local executable copies.
- Pinned every multi-channel command to one Studio runtime so two windows on the same place cannot be mixed during chunked or parallel sync.
- Added Studio undo recordings, Explorer selection preservation, CollectionService tag tracking, and first-error batch stopping for filesystem-to-Studio changes.
- Hardened imports and uploads with recoverable stale-file backups, bounded/expiring sessions, explicit cancellation, and fail-closed place-guard reloads.
- Ignored line-ending-only script differences while preserving the filesystem's CRLF/LF convention.
- Made ordinary compile, test, package, and release builds use checked-in generated metadata without launching Studio; API and icon refreshes are now explicit maintenance commands.
- Expanded release verification with Rust formatting/Clippy, Linux/macOS tests, generated plugin parsing, binary round-trip fixtures, and Rojo builds for both plugin formats.

- Added the **Renium Store Viewer**: a new tab in the Renium sidebar where you drag any `.renium` file (or double-click one in the Explorer) to see its full instance tree, class icons, properties, attributes, and script source. Decoding goes through the `renium view` CLI so what you see matches exactly what syncs.
- Added **renium-link**: control one script's source from a single place and mirror it into multiple targets. Sources can be local, git (public/private), or Wally. Targets are read-only mirrors (with an `L` Explorer badge and native read-only editor) until broken. New commands: `Apply Links`, `Add Link`, `Link Status`, `Break Link`, `Reveal Link Source`, plus a `renium-link.json` manifest watcher and `renium.link.*` settings.
- Hardened **Wally** sync: dropped the Rojo dependency (packages are imported directly), added `wally.lock`-aware no-op detection, and added multi-realm import (`shared`/`server`/`dev`) via `renium.wallySync.realms`.
- Added a locked, version-checked release build that regenerates the VSIX and both Studio plugin artifacts with a checksum/provenance manifest.

## 0.1.1

- Added Renium GitHub sync in the main Renium panel with a normal tab layout, src-only default scope, git status/fetch/pull/commit/push flows, and optional Studio re-apply after pull.
- Added explicit `Renium: Export Game File` support for writing the current `src` tree to `.rbxl` / `.rbxlx` place files without changing GitHub sync's src-only default.
- Added git parsing/redaction helpers plus focused tests for GitHub sync parsing and commit-message utilities.
- Reduced Studio live-sync polling pressure by using a 250ms base interval with adaptive idle/error backoff.
- Wait for the Properties webview to signal readiness before sending the first property payload.
- Updated live-sync documentation and setting descriptions to match current two-way sync behavior.

## 0.1.0

- Initial VS Code/Cursor extension scaffold for Renium with native executables.
- Added command palette actions, status bar entry, output logging, live sync process management, and auto-sync-on-save support.
