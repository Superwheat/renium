<!-- renium-version: 0.5.5 -->
# Renium for agents

Use `rbx` from the place's project folder.

## Choose the smallest sufficient check

- Saved code/data: read files, use focused assertions, project checks or offline queries. Small edits do not require Play.
- Unsaved Studio state: make one bounded live query; do not scan for data already saved locally.
- Runtime behavior: use Play for a specific unanswered question about input, replication, physics or errors. Identify the expected result first.
- Visual behavior: capture the real client with real input (`rs`, `inp`, `re`), then inspect `rf` frames. Script measurements cannot establish what the player sees. A screenshot checks one state; use recordings for motion.

Check Luau syntax offline with `rbx ck FILE...`, never Studio `loadstring` or `LoadStringEnabled`. Project checks cover types, lint and behavior.

With healthy Live Sync, trust file edits. Do not push, poll, reread Studio or start Play after each save. Use one `lst --wait` after a reported problem or when the next operation needs synchronization.

Reuse suitable Play sessions, batch related checks with the fewest clients, and do not stop a user's session. Wait with `rbx wait EXPR -t N`, never a shell loop of `l` calls.

## Read the relevant guide

Read relevant guides once per session; reread after `rbx upd`.

| Task | Guide |
|---|---|
| Saved instances and scripts | `RENIUM/data.md` |
| Bulk and protected properties | `RENIUM/data-properties.md` |
| File queries and comparisons | `RENIUM/data-inspect.md` |
| Configuration and imports | `RENIUM/configuration.md` |
| Pull, push, Live Sync and conflicts | `RENIUM/sync.md` |
| Play sessions | `RENIUM/playtest.md` |
| Live Luau and consoles | `RENIUM/runtime.md` |
| Network simulation | `RENIUM/network.md` |
| UI, input and movement | `RENIUM/input.md` |
| Captures and device simulation | `RENIUM/capture-device.md` |
| Performance and heap snapshots | `RENIUM/performance.md` |
| MicroProfiler and resource limits | `RENIUM/performance-profiler.md` |
| Models, places and packages | `RENIUM/projects.md` |
| Links, Git and collaboration | `RENIUM/links.md` |
| Open Cloud setup and fetch | `RENIUM/opencloud.md` |
| Open Cloud resource operations | `RENIUM/opencloud-resources.md` |
| Assets, products and analytics | `RENIUM/opencloud-assets.md` |
| Studio lifecycle and audio | `RENIUM/advanced.md` |
| Save, publish and diagnostics | `RENIUM/publishing.md` |
| Installed plugins | `RENIUM/plugins.md` |
| Native Studio access and probes | `RENIUM/native-compatibility.md` |

## Targeting and edits

Single-place projects use `src`; experiences use `places/<alias>/src`. A place folder selects its target. At the experience root, add `--place <alias|placeId>` when needed. With two Studios on one project, put `--place <placeId|window name>` before the command (or set `RENIUM_PLACE`); `--runtime-id` does not choose.

Edit scripts as files; stores/places are compressed so grep fails. `f`/`bg`/`bb` read saved data; `q` searches a closed place; `v` inspects a model/place; `l` reads Studio; `oc fetch --version` gets older saved places. `cmp BEFORE --full` (optionally `--against AFTER`) compares fully.

Read targets once and reuse IDs; refresh after a pull. Run mutations sequentially and inspect results. After a failure, check state before retrying. A usage error (unknown flag, missing selector) is a malformed invocation, not a Renium failure: fix it and rerun. Quote JSON/code or pipe stdin. Keep queries bounded; `l` tables over 128 keys are cut. `co --grep` is regex (`-F` for text). Results are one JSON line on stdout, errors on stderr: skip `2>&1`; read it, don't grep it.

Fix and verify defects found in requested work. Renium marks affected linked packages Changed before edits; report `autoDesyncedPackages`, including in failures. Publishing needs authorization; it is not syncing. Say synced, not saved. `persistence: teamCreate` (status, edit results): Team Create synced it to Roblox, never ask for a save; `unsaved`: the user saves or publishes.

Before Open Cloud uploads, follow `RENIUM/publishing-validation.md`: verify API-limited instances are unchanged from the destination, else publish from Studio; local validation does not replace this.

## Tools and boundaries

- Use project tools; report unavailable tools without hunting for executables.
- Pass arguments or pipe JSON/code; do not create payload files.
- Launch, close or replace Studio only when needed; `rbx ro` reopens a closed bound place. Never take focus or global input.
- Do not back up the project: `rbx rev` restores affected files; Studio undo covers pushes.
- Update with `rbx upd` when asked or an update is reported, then reread the guides.
- On a Renium failure follow `RENIUM/bug-reports.md`: write, review every file for private data, upload, return the ID.
