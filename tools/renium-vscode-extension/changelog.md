# Changelog

## 0.3.5 - 2026-09-07

### Sync and Studio connections

- More reliable two-way Live Sync through rapid edits, reconnects, and Play transitions, including when several places are open.
- Resolving a conflict releases the previous sync connection promptly. Stopping Live Sync succeeds even when unresolved changes still need review.
- Deleted temporary instances no longer leave phantom changes waiting to sync. Create, rename, move, and delete bursts preserve the final state and instance references.
- Sync handles Studio's linked corner-radius properties without reporting false conflicts or overwriting genuine edits.
- Duplicate-named instances and cross-service references reconcile correctly without rebuilding unrelated place content. Cross-service moves still recreate the moved Studio object; its saved data and references are preserved.
- Cached exports stay current when properties outside normal Live Sync tracking change.
- Mesh collision-fidelity changes preserve cooked geometry, mass, and inertia in both sync directions, including non-Archivable meshes. Sync waits for Studio to finish applying the change before checking it.
- Live Sync completion no longer waits out its timeout after a successful reconciliation has already cleared the queue.
- Large-place sync avoids repeated project lookups, redundant comparisons, and unnecessary per-instance property listeners without dropping saved properties.
- Restarting Renium reconnects Studio windows opened by an earlier daemon instead of leaving stale connections alive.
- Closing and immediately reopening a local place no longer targets its retired Studio connection.
- Place-name targeting uses the Studio window name instead of the unreliable DataModel name. Commands stay attached to the selected place and reject ambiguous targets.
- Busy connections remain distinguishable from disconnected ones; one slow place no longer holds up unrelated places.
- Commands apply one timeout budget while waiting for busy Studio connections, including their initial retries.
- Play clients and servers follow the correct session through starts, stops, reconnects, and delayed replies.
- macOS automatically ignores Auto-Recovery prompts while preserving recovery files, and explains missing Accessibility permission instead of silently failing.

### Performance monitoring

- New `rbx perf` commands capture frame timing, memory, network traffic, and object counters for Edit mode, servers, or individual clients.
- Inspect slow frames, timing percentiles, and memory categories; export complete recordings when detailed analysis is needed. Unavailable measurements are clearly identified.
- Capture and analyze MicroProfiler data without opening its UI or asking users to save a dump manually. Reports highlight slow frames and relevant scopes, with filters for deeper investigation.
- Built-in profiling is a trusted, authenticated workflow and does not require enabling unrestricted property access or starting a new Play session.

### Network simulation

- New `rbx net` commands change latency, jitter, and packet loss during an existing playtest, including different settings for separate clients.
- Start with `normal`, `mid`, `high`, or `poor` connection presets, or set each direction separately.
- Restore previous settings after testing. Renium rejects configurations that cannot be isolated to the requested client.

### Protected Studio properties

- New `rbx access` commands read and edit supported engine-protected properties directly on Windows and macOS, without restarting Studio.
- Ask mode is the default. Individual approvals apply only to the exact instance and operation; read-only and explicitly enabled read-write modes are also available.
- CollisionFidelity is available by default with validated values. Package edits still mark the containing package Changed and preserve its PackageLink.
- Native entry points are rediscovered and validated after Studio updates. Unsupported layouts return a bounded, actionable error instead of using stale addresses.
- Package auto-desync on macOS no longer mistakes adjacent memory for a second package layout and rejects a valid package.

### Recording and saved-place inspection

- Recording completion includes a timestamped frame overview. Review every captured frame through paginated contact sheets or open an individual full-resolution frame with `rbx rf`.
- Full comparisons now support RBXL and RBXLX files against another file or the local project, covering saved instances, scripts, properties, attributes, references, packages, and Terrain data.
- Comparison output stays compact by default, with optional complete differences and before/after values.
- Temporary import folders no longer appear as game instances in the extension's Explorer.

### Workflow plugins

- Add custom commands without modifying Renium. `rbx plugin new` creates a small starter with a command manifest, Rust handler, SDK, and guide.
- Plugin commands reuse Renium's project targeting and APIs. Persistent exclusive leases protect shared testing resources across tasks and interruptions.
- A configurable sandbox workflow is included as source only; it is not built or installed automatically, and `rbx sandbox` is unavailable until that plugin is installed.

### Agent workflow and editor reliability

- Rewritten documentation teaches shorter, offline-first workflows. Agents use Play only for a specific runtime question, not after every small edit.
- `rbx ck` checks Luau syntax without executing code or enabling `loadstring` in Studio.
- More reliable extension startup, message handling, selection updates, and rapid property edits. Installation and update checks keep the CLI, plugin, and bundled guides aligned.

## 0.3.4 - 2026-09-03

### Roblox packages

- Editing inside a linked package automatically marks it Changed and reports the package path, while keeping its PackageLink intact.
- `rbx pd` marks a package Changed, `rbx pp` publishes its changes, and `rbx pu` discards local changes and updates to the latest published version.
- Package actions work directly on Windows and macOS without selecting instances, opening dialogs, moving windows, or taking input focus.
- Package roots remain protected during sync, including duplicate names, nested packages, rapid edits, retries, and full or filtered updates.
- Discarding package changes restores the published contents instead of only clearing the Changed marker.
- Failed package edits restore the original properties and script source before returning the package to Up To Date.
- Renaming or repositioning a package root remains a normal local override instead of being mistaken for changed package contents.
- Removing a PackageLink is now consistently named **Unlink Package** and remains an explicit `rbx upl` action.

### Live Sync and Studio targeting

- Live Sync reports packages it marked Changed so publishing remains an explicit choice.
- Daemon and Studio restarts reconnect to the exact saved local file or published place, even when several Studio windows are open.
- Direct editor commands return compact results while preserving package-change warnings and actionable errors.
- Script verification accepts Studio's normal line-ending conversion without hiding real source differences.
- Rapid file replacements wait through brief Windows file locks and sync the final saved revision.
- Expected command failures stay in Renium's result and logs instead of filling Studio's Output.
- Studio status lists the available sessions when a project can't select one unambiguously.

## 0.3.3 - 2026-09-01

### Studio sync

- Rapid Studio edit bursts settle into one complete pull instead of saving an intermediate state.
- Live Sync status and pull acknowledgements stay attached to the Edit session while Play starts, runs, or disconnects.
- Entire package roots can be replaced or removed normally while direct `PackageLink` edits remain protected.
- Sync retries when Studio changes during an update, while conflicting project-file edits remain untouched.

### Playtests

- Play status follows the real session in the selected Studio window, including delayed starts and sessions whose server connects before the client.
- Stop waits for the actual client and server to finish without reporting a timeout after Studio has returned to Edit mode.
- Repeated start and stop commands finish or reuse the current session instead of creating overlapping tests.

### Place inspection

- Closed RBXL and RBXLX files can be searched directly with `rbx q` without opening Studio.
- `rbx cmp` compares every projected script with the current project and reports changed, missing, and extra scripts.
- Script comparisons ignore line-ending-only changes and duplicate sibling order.

### Agent workflow

- Live Sync status stays compact unless detailed diagnostics are needed.
- Renium updates run only from an update notice or an explicit request.
- PowerShell examples now cover Luau strings containing apostrophes.

## 0.3.2 - 2026-08-31

### Sync safety

- Reconciliation updates only content that actually changed, including after Studio or the daemon restarts; one edit no longer replaces unrelated place content.
- Independent Studio and project-file edits are combined, while true overlaps remain untouched until a side is chosen.
- Package links and package-owned trees stay protected during ordinary pulls, pushes, moves, renames, deletes, retries, and interrupted syncs.
- Duplicate-named instances, cross-service moves, new and deleted scripts, properties, attributes, tags, and references keep the correct identity in both directions.
- Full pushes finish without hanging or repeating the same work, and commands that require approval are rejected before Studio is contacted.
- A completed sync no longer returns as a false pending Studio change after Studio reconnects.
- Live Sync stays attached to its exact project and Studio place after restarts, and a second project cannot take ownership of the same place.
- Fast create, move, edit, and delete bursts no longer leave stale Studio tracking or false reconciliation differences.
- Cross-service object references resolve correctly when their target is unique and report a clear conflict when it is ambiguous.

### Performance

- Small Live Sync edits update only their affected instances instead of exporting an entire service or rebuilding the place.
- Clean daemon restarts resume from the saved common state without rereading unchanged Studio content.
- Full pulls, no-change pushes, and source-only pushes skip redundant decoding, transfer, and verification work.
- Large services reuse decoded project data and resolve script paths directly, keeping create, delete, and source edits responsive.

### Performance testing

- Studio performance profiles can constrain CPU, processor cores, memory headroom, and priority without changing FPS or taking input.
- Built-in device tiers provide quick degraded-performance tests and are offered only when the current computer can enforce them.
- Custom profiles can be saved, inspected, replaced, and removed with short `rbx pf` commands.
- Active profiles follow replacement Studio processes and survive daemon restarts; turning them off restores Studio's original processor and priority settings.
- Windows applies and verifies the limits directly. macOS and Linux report the feature as unavailable instead of claiming unsupported controls are active.

### macOS

- Renium now uses the installed Roblox Studio app directly instead of creating a separate Renium Studio copy.
- Studio updates are detected and the Renium helper is reapplied to the official app when needed.
- Pulling and exporting no longer opens the macOS Export Place save panel or repeatedly asks for file access.
- Background Studio automation stays behind the user's other apps and avoids activating, resizing, or taking over global keyboard and mouse input.
- Local-place reopening, recovery prompts, and package-change dialogs are handled without changing the selected project or losing the connected session.

### CLI and installation

- Studio, Live Sync, pull, and push commands consistently use the same connected runtime after reconnects.
- macOS setup and updates install a matching CLI, Studio plugin, helper, guides, and editor bundle.
- Command output remains compact and stable, including consistent spacing for scripts and filters that parse it.

## 0.3.1 - 2026-08-29

### Sync safety

- Small edits update only the affected Studio instances instead of rebuilding unrelated place content.
- Packages and their links remain intact during ordinary sync, reconciliation, moves, renames, deletes, and failed updates.
- References continue pointing to the same instances after cross-service moves and renames.
- Duplicate-named instances and reused internal IDs remain distinct instead of being merged, replaced, or duplicated.
- A failed or interrupted transaction leaves the previous Studio state intact and can be retried safely.

### Reconciliation and Live Sync

- Independent Studio and project-file edits are combined automatically; only overlapping changes require a choice.
- Creates, deletes, moves, renames, properties, attributes, tags, and script edits sync reliably in both directions.
- Pulls no longer turn unchanged place content into thousands of false Live Sync changes.
- Live Sync uses the selected project consistently and resumes cleanly after Studio or the daemon reconnects.
- Missing script files, `init` scripts, plugin scripts, and parent-sensitive instance classes keep their intended Studio structure.

### Performance

- Large-place pulls, cross-service moves, reference repair, and file publishing avoid reprocessing unchanged data.
- Bridge payloads are compressed and reused when safe, reducing repeated transfer and decode work.
- Small Live Sync edits no longer fall back to full-place reconstruction.

### Fidelity

- Round trips preserve package IDs, unknown properties, special Roblox value types, MeshPart sizing, Lighting settings, MaterialService settings, hierarchy, attributes, tags, scripts, and references.
- Editor property names map to the correct Studio properties instead of creating transport-only differences.

### Studio and editor reliability

- Background Studio automation no longer brings Studio to the front, resizes it, or takes over keyboard and mouse input.
- Every project command uses the same active Studio connection, avoiding false “no Studio connected” results.
- Custom and isolated daemon ports start and connect to the same intended session.
- Windows installs a native `rbx` launcher while keeping the command-file fallback available.
- Console output keeps stable spacing after labels so existing scripts and filters continue to match it.

## 0.3.0 - 2026-08-26

### Agent commands

- Commands return compact one-line results by default. Use `--output-mode pretty` only when expanded JSON is useful.
- Command help shows short runnable examples without daemon, bridge, or protocol options.
- Saved-data edits rely on Live Sync when it's active instead of repeating the same edit with a manual push.
- Update notices use the short `rbx upd` command.
- Luau can be piped through standard input, and sourcemap reads can use the existing cached map without rebuilding it.
- Explorer searches stay inside the selected service or subtree instead of returning unrelated project instances.

### Live Sync

- Reconcile starts without a direction choice when edits are independent. If both sides changed the same content, choose the version to keep once and Live Sync finishes starting automatically.
- A conflict choice applies only to that overlap; the saved reconciliation policy remains unchanged.
- CLI conflicts return both resolution commands directly; checking Studio's UI isn't required.
- Verify mode compares Studio and project files without changing either side or starting live writes.
- Existing `studio`, `editor`, and `none` startup settings migrate to the matching reconcile or verify behavior.
- Independent script edits from Studio and project files merge against their last common version. Overlapping edits stay pending, and CRLF/LF differences don't create conflicts.
- Studio source, hierarchy, duplicate-name, and sibling-order changes wake Live Sync without waiting for a full rescan.
- Settings changed in the Studio plugin while disconnected are delivered after reconnect; untouched plugin settings don't replace editor settings.

### Explorer

- Large searches show results quickly and keep loading ahead of fast scrolling instead of leaving blank gaps.
- Expanding and collapsing large trees responds faster without reloading work that's already available.
- Clearing a search no longer expands unrelated instances or moves the scroll position backward.
- Search help opens only when starting a search, and search navigation icons display consistently.

### Packages and sync safety

- Package contents can sync without replacing the existing `PackageLink` or losing the package relationship.
- `PackageLink` instances are read-only during normal add, copy, move, rename, property, import, and delete operations. Package desync remains an explicit command.
- Package changes are confirmed after a successful push on Windows and macOS without taking over keyboard or mouse input.
- A failed native import restores the original Studio tree; failed rollback data remains available instead of being discarded.
- Settings files changed during a prepared Studio update are detected before the update can overwrite newer edits.
- The Studio plugin no longer shows the extra editor-sync undo notification.

### Configuration

- Renium settings can be listed, read, and changed through the short `rbx cfg` command.
- The settings list includes current and valid values.

### Open Cloud

- Open Cloud can query analytics, manage game events and experiments, and configure personalized thumbnails.
- Multi-image thumbnail uploads and repeated array parameters are sent intact, and experiments can be started without request-body failures.

## 0.2.9 - 2026-08-22

### Agent commands

- Every automation operation is available as a direct top-level `rbx` command. Context IDs, the `rbx a` layer, command payload files, and manual daemon setup are no longer part of normal agent workflows.
- Short command names are the canonical agent interface, while descriptive names remain available for people using the CLI directly.
- Command help includes short examples for every operation and keeps descriptive aliases pointed at the same examples.
- Studio commands start or reuse the shared connection, select the project and place, and handle required workflow confirmation themselves.
- Image uploads with an explicit user or group owner can use Open Cloud directly without opening Studio.

### Updates

- Every agent command checks for a newer Renium version through one shared five-minute cache and prints a short update command when one is available.
- Failed update checks also respect the five-minute cache instead of retrying on every command.
- Generated agent instructions recommend keeping Renium current and rereading the instructions after an update.

### Open Cloud

- Native Open Cloud commands use Roblox's current request formats for assets, passes, badges, localization, servers, thumbnails, memory stores, Creator Store, advertising, experiments, events, and configuration.
- Pagination uses the correct token and limit fields for each Roblox endpoint.
- Creator Store search supports current page tokens, verified-creator filtering, price limits, and audio-duration filters.
- Multipart uploads and asset-version rollbacks send files and form fields in the formats Roblox expects.
- Public read operations can use `--anonymous`; authenticated permission failures remain visible instead of silently retrying under a different identity.

### Live Sync and agent commands

- Live Sync remains enabled when the shared daemon is replaced and resumes when the project is used again.
- Agents can rely on Live Sync after an edit instead of sending the same change with a separate push command.
- One-off file pushes accept paths directly, such as `rbx ps src/StarterGui/Menu.client.luau`.
- Successful push commands return only useful results instead of repeating paths, targets, direction, and selection details from the command.
- Project agent guides refresh when their contents change, including changes published under the same Renium version.

## 0.2.8 - 2026-08-22

### Live sync

- Studio edits wake live sync immediately instead of waiting for a polling interval.
- Files-to-Studio creates and deletions reach Studio sooner without scanning unrelated instances.
- Long Studio change waits leave the other bridge channel available, so Explorer edits and automation commands can run while live sync is idle.
- Pulls and pushes wait for the complete Studio connection and cancel stale change waits when needed instead of failing during a partial reconnect.
- Auto-connect keeps using its fast retry period after an unexpected disconnect.

### Explorer

- Creating, editing, or deleting an instance from the editor starts or reuses the Studio connection and reports an error if Studio did not apply the edit.
- Empty service roots accept their first child, and deleting the last child keeps the service ready for later edits.
- Multiple instances with the same default name remain separate and sync in creation order.
- Opening the add-instance menu no longer changes the selected node's expanded state.

### Project state

- Importing snapshots rebases the live watcher to the imported files instead of treating the import as new editor work.

## 0.2.7 - 2026-08-21

### Updates

- Updating the Studio plugin now asks how to handle each connected local place: leave it open, save and close it, or terminate it without saving. The choice can be remembered for later updates.
- Places closed for an update reopen at the same local file or published universe and place after the update succeeds or rolls back.
- Saving a local place before an update includes its live unsaved changes without opening Studio's save dialog or taking keyboard and mouse input.
- Direct Studio close commands require an explicit save or terminate choice instead of guessing what to do with local work.

### Studio automation

- Luau commands, waits, and movement return their results through Renium without adding automation messages to Studio's Output.
- Project instructions refresh when the installed Renium version changes, and agent commands stop once to request a reread before continuing with changed instructions.

## 0.2.6 - 2026-08-21

### Live sync

- Live sync now watches project files in the shared Renium daemon, so editor commands and direct `rbx` use the same pending work, retries, and Studio connection.
- File edits made while Studio or the editor is disconnected remain pending across daemon restarts and are sent when live sync starts against the matching place again.
- Nearby saves, directory changes, renames, deletions, new source roots, nested projects, and project configuration changes are collected into complete pushes instead of partial or repeated syncs.
- Transient connection failures retry with a bounded delay, while permanent failures keep the affected files pending until they are edited or explicitly retried.
- Starting live sync clears a pause left by an interrupted editor session, and stopping it also stops file watching when Studio's live-mode cleanup fails.
- Sync on save sends the saved files to Studio even when continuous live sync is off.
- Pulls, manual pushes, Git changes, package updates, link updates, history restores, and generated file writes coordinate with the watcher so their own writes aren't pushed back as new edits.
- Live-sync state files stay under `.renium` and are ignored by version control in new and existing projects.

### Script syncing

- Script batches that span multiple services are checked against Studio's current Script Editor content, and only scripts that didn't apply are retried.
- A script that still differs remains pending with its exact verification error instead of making the rest of the batch repeat or silently reporting success.
- Files edited during a Studio pull are kept as local pending changes instead of being consumed by the pull.

### Editor

- Git branch changes pause file mirroring and resume from the final worktree without stopping and rebuilding the whole live-sync session.
- Live-sync status shows daemon-side pending files and failures without running a second editor-side filesystem watcher.
- The obsolete **Run Import** setting is gone; pulling from Studio always updates the project files required by the operation.

## 0.2.5 - 2026-08-20

### Updates

- Agent commands report when a newer Renium version is available and include the command that installs it.

### Roblox Open Cloud

- Persistent and ordered data, memory stores, universes, places, messages, servers, restrictions, secrets, notifications, users, inventories, groups, social interactions, Team Create, assets, passes, Creator Store, localization, configs, analytics, ads, experiments, events, matchmaking, thumbnails, and speech generation have direct `rbx cloud` commands.
- Open Cloud work no longer requires endpoint paths or temporary JSON payload files for supported operations.
- API-key scopes and resource limits can be checked without exposing the key, including user-owned, dedicated group-automation, and resource-restricted keys.

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
