<!-- renium-version: 0.3.5 -->
# Renium for agents

Use `rbx` from the place's project folder. Renium handles connections and the daemon.
If PATH is stale: Windows `%USERPROFILE%\.renium\bin\rbx.exe`; macOS/Linux `~/.renium/bin/rbx`.

## Choose the smallest sufficient check

- **Saved code or data:** read the files; use focused assertions, the project's checks, or Renium's offline queries. Names, values, references, source edits, and pure logic usually need no Play session.
- **Unsaved Studio state:** make one bounded live query. Don't scan Studio for data already saved locally.
- **Runtime behavior:** use Play only for a specific unanswered question, such as input handling, replication, physics, or a runtime error. Identify the expected result first. A small edit is not itself a reason to playtest.
- **Visual behavior:** a screenshot checks one state; a recording checks a transition. Review the captured evidence, not merely whether capture succeeded.

Check Luau syntax offline with `rbx ck FILE...`; it parses without executing code. Use project checks for types, lint, and behavior. Never use Studio `loadstring` for validation or enable `LoadStringEnabled` to make a check work. Don't execute or require scripts merely to check syntax.

With healthy Live Sync, trust successful file edits. Don't push, poll, or reread Studio after every save. Use one `lst --wait` after a reported problem or when the next operation needs synchronization. Don't start Play to prove a file edit synced.

When Play is needed, reuse a suitable session. Test related changes together, with the fewest clients required. Don't stop a user's session just to create your own.

## Read the relevant guide

| Task | Guide |
|---|---|
| Saved instances, scripts, properties, queries | `RENIUM/data.md` |
| Configuration, adapters, filters, imports, validation | `RENIUM/configuration.md` |
| Pull, push, Live Sync | `RENIUM/sync.md` |
| Play, live Luau, clients, consoles, network simulation | `RENIUM/playtest.md` |
| UI, input, movement | `RENIUM/input.md` |
| Screenshots, recording review, device simulation | `RENIUM/capture-device.md` |
| Lag spikes, MicroProfiler dumps, network traffic, resource limits | `RENIUM/performance.md` |
| Models, places, packages, links, Git | `RENIUM/projects.md` |
| Open Cloud and creator assets | `RENIUM/opencloud.md` |
| Studio lifecycle and place management | `RENIUM/advanced.md` |
| Installed plugins and their commands | `RENIUM/plugins.md` |

Read only guides needed for the task, before using their commands. Use command help for options not covered here.

## Targeting and edits

Single-place projects use `src`; experiences use `places/<alias>/src`.
A place folder selects its target. At the experience root, add `--place <alias|placeId>` when needed.
Studio commands also accept `gameId:placeId` or a Studio window name; ambiguity returns candidates.

Edit existing scripts as files. Use Renium for generated `.renium` stores and sourcemaps.
`f`/`bg`/`bb` read saved data; `q` searches a closed place; `v` inspects a model/place; `l` reads live Studio. For a full place comparison, use `cmp BEFORE --full` (optionally `--against AFTER`). Counts cover the whole place; request `--all` or `--values` only when needed.
Choose the source that answers the question. Compare states only when the task calls for it.

Read an existing target once and reuse its ID; refresh IDs after a pull.
Run mutations one at a time and inspect each result. If one fails, check the affected state before retrying or recovering.
An empty `changedPaths` is a no-op. After cleanup, one prefix search is enough; `storeRemoved: true` needs no follow-up store query.

Renium marks affected linked packages Changed before edits. Report `autoDesyncedPackages`, including packages named in a failed edit. Publishing needs user authorization; it is not part of syncing.

## Tools and boundaries

- Use project-declared tools through their normal commands. If unavailable, report that; don't hunt for executables inside caches or extension folders.
- Pass arguments or pipe JSON/code through stdin; don't create payload files.
- Launch, close, or replace Studio only when the task needs it. Never take focus or global input.
- Ignore `.renium/editor-history`; it is local revert data.
- Update with `rbx upd` when requested or an update is reported, then reread these guides.
