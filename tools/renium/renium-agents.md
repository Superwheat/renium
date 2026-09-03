<!-- renium-version: 0.3.4 -->
# Renium automation

Use `rbx`. If `PATH` is stale, use `%USERPROFILE%\.renium\bin\rbx.exe` on Windows or `~/.renium/bin/rbx` on macOS/Linux. Never search editor extension folders.

## Guide hierarchy

Before using a feature, read its guide under `RENIUM/`:

| Task | Required guide |
|---|---|
| Saved instances, properties, attributes, scripts | `RENIUM/data.md` |
| Settings, mappings, adapters, filters, imports, validation | `RENIUM/configuration.md` |
| Pull, push, Live Sync | `RENIUM/sync.md` |
| Playtests, Luau, consoles, clients | `RENIUM/playtest.md` |
| UI, input, movement, world interaction | `RENIUM/input.md` |
| Screenshots, recordings, device simulation | `RENIUM/capture-device.md` |
| Studio resource constraints and performance profiles | `RENIUM/performance.md` |
| Models, places, links, packages, Git | `RENIUM/projects.md` |
| Creator Store, Open Cloud, images, generation | `RENIUM/opencloud.md` |
| Places, Studio lifecycle, ordered input, multi-edit | `RENIUM/advanced.md` |

Read only relevant guides. Read several when categories overlap. Don't guess unread commands.

## Rules

- Use direct `rbx` commands. Renium handles the daemon, project, and Studio binding.
- Run `rbx upd` only when Renium reports an available version or the user asks. After an update, reread this file and the current task's guides.
- Don't start `rbx bd`, inspect daemon internals or help, or bind a context first.
- Don't create payload files. Use arguments; pipe larger `bb` queries through stdin.
- Read existing targets before editing. For unique temporary targets, create once and reuse returned IDs. Find IDs again after a pull.
- After deleting unique test instances, search once by their shared prefix. If `br` returns `storeRemoved: true`, don't query that removed store.
- Don't read or edit `.renium` or `sourcemap.json` by hand; use `rbx`.
- Ignore `.renium/editor-history`; it is local revert data.
- Launch, close, or replace Studio only when required. Renium handles confirmation.
- Run one mutation command at a time and inspect its result before the next. Never chain edits, deletes, pulls, pushes, Undo, Redo, package insertion, or recovery in one shell command.
- After a failed mutation, stop and verify affected live roots before any recovery or sync.
- Renium marks linked packages Changed before editing their descendants. If a result includes `autoDesyncedPackages`, tell the user which packages changed and ask before `pp`; never publish automatically. Use `pd` to mark one manually, `pu` to discard its changes and update it, and `upl` only to remove its PackageLink.

## Projects and targeting

Single-place projects use `src`; multi-place projects use `places/<alias>/src` and `renium.experience.json`. A place folder selects itself. At the experience root, Renium uses the sole matching Studio place; otherwise add `--place <alias|placeId>`. Studio commands also accept `gameId:placeId` or a place name. Ambiguity returns candidates.

Edit `.lua` and `.luau` directly. Use `rbx` for generated `.renium` data.

`f`, `bg`, and `bs` read saved files, not Studio. Use `rbx l` for live edits, then `rbx pl`; use `rbx ps` for files → Studio. Query the service named in `src/<Service>/...`.

Choose the data source once: `rbx f` searches saved project data, `rbx l` checks live Studio, and `rbx q FILE.rbxl` searches a closed place directly. Use `rbx cmp FILE.rbxl` to compare every projected script with the current project. Refine one ambiguous result instead of repeating the same check through other tools.
