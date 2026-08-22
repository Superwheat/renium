<!-- renium-version: 0.2.9 -->
# Renium automation

Use `rbx`; the installer adds `rbx` and `renium` to `PATH`. If this process has an old `PATH`, use `%USERPROFILE%\.renium\bin\rbx.cmd` on Windows or `~/.renium/bin/rbx` on macOS and Linux. Never search editor extension folders.

## Guide hierarchy

This file contains the rules and targeting behavior required for every Renium task. Before using a feature, read its matching guide under `RENIUM/`:

| Task | Required guide |
|---|---|
| Find, inspect, or edit saved instances, properties, attributes, and scripts | `RENIUM/data.md` |
| Configure mappings, adapters, filters, imports, or project validation | `RENIUM/configuration.md` |
| Pull, push, or inspect Live Sync | `RENIUM/sync.md` |
| Start tests, run Luau, read consoles, or target several clients | `RENIUM/playtest.md` |
| Discover UI, press, type, click, move, wait, or interact with the world | `RENIUM/input.md` |
| Take screenshots, record clips, or control device simulation | `RENIUM/capture-device.md` |
| Import/export models or places, manage links/packages, or configure version control | `RENIUM/projects.md` |
| Use Creator Store, Open Cloud, images, or generated models | `RENIUM/opencloud.md` |
| Manage places, reopen or close Studio, run ordered input, or edit several scripts | `RENIUM/advanced.md` |

Read only the relevant guides. Read several when a task crosses categories. Don't guess a command from memory when its guide hasn't been read. A broad Renium audit may require every guide; ordinary work shouldn't.

## Rules

- Every operation is a direct `rbx` command. Renium reuses the editor daemon, selects the project, and binds Studio itself.
- If `rbx` reports that Renium instructions were updated, reread this file and the guides required for the current task before retrying the command.
- Keep Renium current. When `rbx` reports an available update, run `rbx update`, then reread this file before continuing.
- Don't start `rbx bd`, inspect daemon state, inspect CLI usage, or bind a context before a command.
- Don't create command payload files. Use positional values and flags. `bb` may read a batch query from stdin when several saved-data reads belong together.
- Read an existing target before changing it. Don't preflight a uniquely named temporary file or instance; create it, then reuse successful writes and returned IDs or paths without rereading them. Pull creates a new snapshot, so find existing IDs again afterward.
- After removing uniquely named temporary instances, confirm cleanup with one search for their shared name prefix; don't probe every deleted ID. If `br` returns `storeRemoved: true`, don't search that absent service.
- Don't read or edit `.renium` or `sourcemap.json` by hand; use `rbx`.
- `.renium/editor-history` is expected local revert data, not project content; don't inspect or restore its timestamps.
- Don't launch, close, or replace Studio as a fallback. Do it only when the task requires it; Renium handles its workflow confirmation internally.
- Run one mutation command at a time and inspect its result before the next. Never chain edits, deletes, pulls, pushes, Undo, Redo, package insertion, or recovery in one shell command.
- If a mutation fails, stop changing Studio. Verify the affected live roots before any pull, push, retry, Undo, Redo, package insertion, or recovery action.
- Don't rename, delete, or replace a Roblox package root to work around a failed edit. Use `desync-package-link` only when removing the package relationship is the intended change.

## Projects and targeting

Single-place projects use `src`. Multi-place projects use `places/<alias>/src` and `renium.experience.json`. Inside `places/<alias>`, commands infer that place. At the experience root, Renium uses the sole matching Studio place from the running daemon; otherwise put `--place <alias|placeId>` before the command. Studio commands also accept `gameId:placeId` and place names. Ambiguous commands list valid places instead of searching a nonexistent root `src`.

Edit `.lua` and `.luau` files directly. Renium stores other instance data in generated `.renium` files; inspect and edit it through `rbx`.

Project commands such as `f`, `bg`, and `bs` use saved files, not the live Studio tree. Use edit-context `rbx l` for a live Studio change, then `rbx pl`; use `rbx ps` to send saved file changes to Studio. `src/<Service>/...` maps to that Roblox service, so query the service named by the file path.
