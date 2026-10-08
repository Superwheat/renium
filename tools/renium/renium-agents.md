<!-- renium-version: 0.4.8 -->
# Renium for agents

Use `rbx` from the place's project folder.

## Choose the smallest sufficient check

- **Saved code or data:** read the files; use focused assertions, the project's checks, or Renium's offline queries. Names, values, references, source edits, and pure logic usually need no Play session.
- **Unsaved Studio state:** make one bounded live query. Don't scan Studio for data already saved locally.
- **Runtime behavior:** use Play only for a specific unanswered question, such as input handling, replication, physics, or a runtime error. Identify the expected result first. A small edit is not itself a reason to playtest.
- **Visual behavior:** numbers read by scripts show state, not what is on screen (a chase camera moves with the car, so a dip can be invisible). When the user reports what they saw, or you are judging whether something is visible, record the real client view with real input (`rs`, `inp`, `re`, then look at the `rf` frames) instead of re-measuring. A screenshot checks one state.

Check Luau syntax offline with `rbx ck FILE...`, never with Studio `loadstring`, `LoadStringEnabled`, or by running scripts; project checks cover types, lint and behavior.

With healthy Live Sync, trust successful file edits: don't push, poll, or reread Studio after every save, and don't start Play to prove an edit synced. Use one `lst --wait` after a reported problem or when the next operation needs synchronization.

When Play is needed, reuse a session, batch related checks with the fewest clients, and don't stop a user's session. Wait for runtime state with `rbx wait EXPR -t N`, never a shell loop of `l` calls.

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

Read only the guides the task needs, once per session, before using their commands; reread after `rbx upd`.

## Targeting and edits

Single-place projects use `src`; experiences use `places/<alias>/src`.
A place folder selects its target. At the experience root, add `--place <alias|placeId>` when needed.
With two Studios open on one project, put `--place <placeId|window name>` before the command (or set `RENIUM_PLACE`); `--runtime-id` does not choose.

Edit scripts as files; use Renium for generated `.renium` stores and sourcemaps.
`f`/`bg`/`bb` read saved data; `q` searches a closed place; `v` inspects a model/place; `l` reads live Studio; `oc fetch --version` gets an older saved place. For a full place comparison, use `cmp BEFORE --full` (optionally `--against AFTER`).

Read a target once and reuse its ID; refresh after a pull.
Run mutations one at a time and inspect each result; if one fails, check the affected state before retrying.
A usage error (unknown flag, unexpected argument, missing selector) means the invocation was malformed, not that Renium failed: fix it and rerun. Quote code and JSON or pipe them on stdin.
Keep query results small (counts, slices, specific fields); `l` tables over 128 keys are cut with `_truncated`. `co --grep` is a regex (`-F` for text). Write large captures to a file.
When a check exposes a defect in what you were asked to deliver, fix and verify it. Never hand the user steps you could run yourself.

Renium marks affected linked packages Changed before edits. Report `autoDesyncedPackages`, including packages named in a failed edit. Publishing needs user authorization; it is not part of syncing. Say synced, not saved: with `teamCreate: true` in `rbx status` Studio saves to Roblox itself; otherwise only the user saves or publishes.

## Tools and boundaries

- Use project-declared tools through their normal commands; if one is unavailable, say so instead of hunting for executables.
- Pass arguments or pipe JSON/code through stdin; don't create payload files.
- Launch, close, or replace Studio only when the task needs it; `rbx ro` reopens a closed bound place. Never take focus or global input.
- Don't back up the project: `rbx rev` restores files a sync changed and Studio undo covers pushed edits.
- Update with `rbx upd` when asked or an update is reported, then reread these guides.
- If Renium itself fails (internal error, crash, lost sync data), run `rbx report -m "what happened"` and give the user its ID and path.
