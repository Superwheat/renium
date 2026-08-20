# Changelog

## Unreleased

## 0.2.4 - 2026-08-20

### Roblox Open Cloud

- Open Cloud commands work directly without Roblox Studio, a Renium connection, or a running daemon.
- Developer products can be listed, read, created, and updated from `rbx cloud product`.
- Any Open Cloud endpoint can be called with API-key or OAuth authentication, including JSON, form, file, and raw uploads and binary downloads.
- Newly saved Roblox credentials on Windows are available without restarting the editor.

### Renium Link

- Linked models can be renamed, moved, and rotated without losing their link.
- Updating a linked model preserves its local name, position, and orientation while refreshing its contents.

### Installation

- Windows updates keep `renium` and `rbx` pointed at one current installation instead of leaving stale executable copies behind.

## 0.2.3 - 2026-08-20

### Package editing

- Editing scripts, properties, attributes, or children inside a Roblox package no longer removes its `PackageLink`.
- Renium accepts Studio's package-edit warning without taking over keyboard or mouse input on Windows and macOS.

### Studio plugin

- The Studio toolbar shows the Renium icon again.

## 0.2.2 - 2026-08-20

### Agent guidance

- Agents now start with a compact Renium guide and open only the task-specific instructions they need, reducing context use without removing commands or safety rules.
- New and existing projects receive the same topic guides whenever Renium creates or refreshes their instructions.

## 0.2.1 - 2026-08-19

### Syncing

- Files-to-Studio pushes now stage the affected roots before editing and restore them if any batch or final verification fails, including package-backed trees.
- Package edits desync only the package relationships on changed paths; unrelated packages, services, and descendants remain untouched.
- Large editor transactions are streamed through the bridge in bounded chunks instead of failing at the WebSocket request limit.
- Cross-service moves use one project transaction and preserve scripts, references, sibling order, and destination package links.
- Script writes are verified against Studio's active script document, standalone deleted script files remove their instances, and deleted `init` scripts keep their children as folders.
- Filtered native imports still apply independent source and property changes outside the imported services.
- Property commands resolve Roblox aliases and case differences to the property name actually stored in project data.

### Studio automation

- GUI presses verify that the target is visible, unobstructed, and receives the expected Roblox button events before reporting success.
- Input coordinates follow the real rendered viewport, including scaled and simulated views, while the input shield yields only to Renium's own pointer events.
- Device simulation state is kept by the shared daemon and restored when a new Studio process connects; stopping simulation also closes Studio's remaining emulator toolbar on Windows and macOS.
- Play and Studio status report active device simulation so automation doesn't silently test with mobile controls.
- Ambiguous selectors and other already-reported command failures exit once without printing the same error twice.
- Installed launchers keep pointing at the selected Renium version after an update.

## 0.2.0 - 2026-08-18

### Syncing

- Added direct `rbx pull`, which starts or reuses the shared daemon, waits for the matching Studio place, and keeps temporary data under `.renium`.
- Pulling a place to files and pushing it back preserves classes, properties, attributes, references, scripts, hierarchy, and sibling order.
- Fixed full pushes swapping instances that share the same parent.
- External script `Source` reads and writes now use the exact source file instead of exposing `__SOURCE_EXTERNAL__` or creating a shadow property.
- Snapshot export and import reproduce every generated project file byte-for-byte.
- Model and store inspection use stable instance references that remain useful across commands.
- Removing the final imported tree from a service also removes its empty settings store and directory.
- File-backed commands and sourcemaps work from experience roots and individual place folders.
- Multi-place commands infer the current place when possible and list valid places when selection is ambiguous.
- Pull and push results are smaller and clearly identify their direction and affected services.

### Studio and automation

- Runtime listing no longer repeats the same Studio runtime under separate collections.
- Unpublished Play and local-server runtimes retain the name of their originating Edit place.
- Studio Auto-Recovery dialogs are dismissed automatically for Studio instances controlled by Renium.
- Minimized Studio windows can be restored without taking focus or moving ahead of other windows.
- Input commands recover from a minimized `1x1` Play viewport before interacting with it.
- Held-key results include the effective duration, and navigation results include the arrival radius and final distance.
- MP4 recording can be controlled with `record-start` and `record-end` without JSON, context IDs, or recording IDs.
- Device status reflects Studio's actual emulator selection, including devices selected outside Renium.
- `device stop` resets Studio to the normal desktop device, while disconnecting Renium no longer changes the selected device.
- Device changes fail clearly during Play instead of reporting an edit-side state that does not affect the running client.
- Fixed portrait resolution, native-versus-effective density, rounded viewport dimensions, duplicate device IDs, and detailed native-orientation output.
- Local-server tests no longer print repeated missing-plugin-icon errors.

### Direct agent commands

- Added offline `project-validate`, `script-search`, `script-grep`, and ranged `script-read` commands that need no Studio connection, daemon, context, or JSON payload.
- Limited script searches report complete totals, deterministic ordering, and whether results were truncated.
- Creator Store search and Roblox documentation reads work directly without context binding.
- Creator Store results are compact by default and identify the requested asset type.
- Public Creator Store models use Roblox's plugin-accessible loader when ownership-only loading APIs reject them.
- AI model generation, creator-job polling, and local image validation have direct commands.
- Roblox documentation results contain readable text and signatures instead of minified page markup.
- Created, cloned, renamed, and moved instances return their resulting stable identity for immediate reuse.
- Batched `prop:Source` reads return exact external script text, and requested field filters no longer leak unrequested internal paths and IDs.
- Property and source values beginning with hyphens are accepted normally.
- Repeating an edit that changes nothing does not rewrite files or report false changes.
- Misspelled properties are rejected, while explicit property scope remains available for hidden or newly introduced Roblox properties.
- Typed instance references survive cloning and are remapped to cloned targets.

### Projects and configuration

- Single-place projects can be converted safely into multi-place experience layouts.
- Place addition validates IDs, aliases, destinations, and game identity before moving project files.
- Failed place conversions and renames restore the original project instead of leaving a partial migration.
- Place add, rename, and reorder results state when the project must be rebound.
- Project initialization previews every file and directory it will create or update and rejects wrong-type collisions before changing anything.
- Validation accepts absent source roots for mount-only projects but rejects existing source roots that are not directories.
- Rojo imports preserve JSONC comments, no longer mistake URLs for comments, and omit redundant default, empty, and null fields.
- Generated adapter modules remain visible through `find`, `tree`, `inspect`, and `bb` without invalidating later validation, builds, path explanations, or syncback.
- Adapter syncback counts only source files changed by the user.
- Writable mounts update their backing files, read-only mounts reject edits, and mount-only projects export through their projected content.
- `explain-path` identifies transformed sources, excluded rules, matching selectors, sync direction, and property or attribute decisions.
- Configuration paths are normalized across Windows, macOS, and Linux, and removing the final scoped value cleans up its empty file and directory.
- File and directory imports identify each file as `create`, `overwrite`, or `unchanged`; unchanged files are not rewritten or included in later push work.
- Newly imported scripts can be inspected before their service has a `.renium` settings store.
- `doctor` reports complete parser errors, normalized paths, correct repair instructions, and deterministic diagnostic bundles.

### Packages and version control

- Link application can initialize a missing service store without requiring a Studio pull first.
- Links can be removed permanently while keeping their instances as editable project content, and empty manifests and lock files are removed automatically.
- Link results distinguish total and active targets, return stable root IDs, and report no changes for unchanged refreshes.
- Fixed link path forms, ordinals, hierarchy counts, exact source reads, and manifest and lock cleanup.
- Wally directory `init.lua` files retain their children, lock versions are correct, unchanged normalized package trees are detected, and forced refresh is supported.
- Detaching a reusable package writes embedded scripts back as exact editable source files.
- Package operations return stable IDs for the complete materialized subtree, and repacking includes local packages referenced by the project.
- Unchanged packages, stores, and formatted project files retain their bytes and timestamps.
- Git initialization adds missing Renium rules without replacing existing user rules.
- Renium policy files and JSONC configuration retain LF line endings on Windows.
- Binary-store merges support independent field edits and clearly reject conflicting edits to the same field.
- Merge-driver paths containing spaces work correctly.

### Editor and installation

- Completed editor updates close their progress notification before asking for a reload.
- **Check for Updates** appears at the bottom of the main Renium menu.
- Windows installs directly from the selected platform ZIP instead of downloading the CLI again.
- Windows and macOS verify the Studio plugin against the signed release manifest before installation.
- macOS and Linux show editor choices and **Exit** before downloading release files.
- Normal command output no longer includes bridge startup lines, channel-ready messages, internal build timings, or per-service import progress.
- Generated `RENIUM.md` uses direct commands and avoids unnecessary daemon checks, help calls, recursive searches, temporary JSON files, repeated reads, and local-server tests.
- The Windows `rbx.cmd` fallback correctly handles complex inline Luau containing `cmd.exe`-sensitive syntax.

## 0.1.9 - 2026-08-17

- Preserved non-Archivable instances and current script documents during full Studio pulls without cloning the live DataModel.
- Kept package roots and cross-service references intact during full pushes while avoiding unnecessary package snapshot work.
- Reduced full-push time with direct project builds, filtered native exports, and faster retained-package matching.
- Verified a 95,691-instance pull-to-push round trip with no class, property, attribute, source, or reference differences.
- Sent only plugin settings edited while disconnected when the matching editor reconnects.
- Started automatic connections immediately in every Studio state and made the plugin show connection progress without moving its controls.
- Kept bound automation contexts valid when editor and direct `rbx` commands share one daemon.
- Improved source projection, duplicate-name mapping, sourcemap generation, model pivot restoration, and native export mutation checks.

## 0.1.8 - 2026-08-15

- Added one Windows installer that selects x64 or ARM64 automatically and recovers when the wrong platform ZIP was downloaded.
- Fixed standalone installers to verify downloads through the release update manifest instead of a removed checksum file.
- Kept the editor extension active without a workspace and initialized project features when a folder becomes available.
- Removed the redundant SPDX, checksum-list, and XML plugin files from GitHub release assets.
- Reorganized the editor menu around sync, project, and tool groups with shorter result-focused descriptions.
- Prevented full pushes from exposing an empty place while replacing Studio's service trees.
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
