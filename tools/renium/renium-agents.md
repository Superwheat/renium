<!-- renium-version: 0.4.6 -->
# Renium for agents

Use `rbx` from the place's project folder. Renium handles the daemon.
PATH stale? `%USERPROFILE%\.renium\bin\rbx.exe` (Windows) or `~/.renium/bin/rbx`.

## Choose the smallest sufficient check

- **Saved code or data:** read the files; use focused assertions, the project's checks, or Renium's offline queries. Names, values, references, source edits, and pure logic usually need no Play session.
- **Unsaved Studio state:** make one bounded live query. Don't scan Studio for data already saved locally.
- **Runtime behavior:** use Play only for a specific unanswered question, such as input handling, replication, physics, or a runtime error. Identify the expected result first. A small edit is not itself a reason to playtest.
- **Visual behavior:** a screenshot checks one state; a recording checks a transition. Review the captured evidence, not merely whether capture succeeded.

Check Luau syntax offline with `rbx ck FILE...`, never with Studio `loadstring`, `LoadStringEnabled`, or by running scripts; project checks cover types, lint and behavior.

With healthy Live Sync, trust successful file edits: don't push, poll, or reread Studio after every save, and don't start Play to prove an edit synced. Use one `lst --wait` after a reported problem or when the next operation needs synchronization.

When Play is needed, reuse a suitable session, test related changes together with the fewest clients required, and don't stop a user's session to create your own.

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
| Models, places, packages, links, Git, live collaboration | `RENIUM/projects.md` |
| Open Cloud and creator assets | `RENIUM/opencloud.md` |
| Studio lifecycle and place management | `RENIUM/advanced.md` |
| Installed plugins and their commands | `RENIUM/plugins.md` |

Read only the guides the task needs, once per session, before using their commands; reread after `rbx upd` or when told.

## Targeting and edits

Single-place projects use `src`; experiences use `places/<alias>/src`.
A place folder selects its target. At the experience root, add `--place <alias|placeId>` when needed.
Studio commands also accept `gameId:placeId` or a Studio window name.

Edit scripts as files; use Renium for generated `.renium` stores and sourcemaps.
`f`/`bg`/`bb` read saved data; `q` searches a closed place; `v` inspects a model/place; `l` reads live Studio; `oc fetch --version` gets an older saved place. For a full place comparison, use `cmp BEFORE --full` (optionally `--against AFTER`).

Read an existing target once and reuse its ID; refresh IDs after a pull.
Run mutations one at a time and inspect each result; if one fails, check the affected state before retrying.
A usage error (unknown flag, unexpected argument, missing selector or path) means the invocation was malformed, not that Renium failed: fix it and rerun instead of stopping or asking. Quote code and JSON for the shell or pipe them on stdin.
Keep query results small (counts, slices, specific fields); write large captures to a file.
When a check exposes a defect in what you were asked to deliver, fix and verify it rather than reporting it. Never hand the user steps you could run yourself; do them, or offer to when it is their call.
After cleanup, one prefix search is enough; `storeRemoved: true` needs no follow-up query.

Renium marks affected linked packages Changed before edits. Report `autoDesyncedPackages`, including packages named in a failed edit. Publishing needs user authorization; it is not part of syncing. Say synced, not saved: with `teamCreate: true` in `rbx status` Studio saves to Roblox itself; otherwise only the user saves or publishes.

## Tools and boundaries

- Use project-declared tools through their normal commands; if one is unavailable, say so instead of hunting for executables in caches or extension folders.
- Pass arguments or pipe JSON/code through stdin; don't create payload files.
- Launch, close, or replace Studio only when the task needs it; `rbx ro` reopens a closed bound place. Never take focus or global input.
- Don't back up the project: `rbx rev` restores files a sync changed and Studio undo covers pushed edits; `.renium/editor-history` holds that data.
- Update with `rbx upd` when requested or an update is reported, then reread these guides.
- If Renium itself fails (internal error, crash, lost sync data), run `rbx report -m "what happened"` and give the user its ID and path.
