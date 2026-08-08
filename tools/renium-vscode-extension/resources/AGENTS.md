# Renium automation

Use the compact Renium automation API for Studio and project operations. On Windows, the stable launcher is `%USERPROFILE%\.renium\bin\rbx.cmd`. On macOS and Linux, it is `~/.renium/bin/rbx`. If `rbx` is not on `PATH`, invoke that launcher directly; do not search extension folders.

## Bind once

Bind the intended project and place before every sequence. The returned `r.id` is the daemon-local context ID used as `CX` below.

```powershell
rbx a bind . 101945566570840
rbx a context CX
```

The place selector may be a place ID or `gameId:placeId`. If multiple Studio runtimes match, inspect the candidate IDs in `e.d.candidates`, put `root`, `place`, and the exact `runtime` in `bind.json`, then run `rbx a bind -J bind.json`. For an empty folder, set `bootstrap:true` in the bind payload; that context permits only `project-init` and `project-validate` until the project is created. A context becomes stale after daemon restart, project identity changes, runtime disconnect, or a plugin rebuild. Bind again after `stale_cx`.

## Operations

- Context: `cap`, `bind`, `context`, `unbind`
- Sync: `pull`, `push`, `live-start`, `live-stop`, `live-status`, `retry-pending`, `discard-pending`
- Read: `find`, `tree`, `inspect`, `batch`
- Edit: `get-property`, `set-property`, `set-source`, `add`, `clone`, `move`, `remove`, `revert`
- Files: `import-model`, `export-model`, `export-place`, `import-snapshots`, `export-snapshots`, `sourcemap`
- Studio: `studios`, `studio-status`, `studio-open`, `studio-close`, `luau`, `console`, `play-start`, `play-stop`, `shot`, `device`
- Input: `ui`, `press`, `click`, `key`, `type`, `wait`, `goto`
- Project: `project-init`, `project-validate`, `place-add`, `place-rename`, `place-reorder`
- Review: `review-prepare`, `review-apply`, `review-reject`

`pull` writes Studio into project files. `push` writes project files into Studio. Live sync stays two-way. When an operation returns `rejected` with `e.n` set to `review-prepare`, the exact operation needs a receipt before `review-apply` can execute it. The `rbx a` wrapper handles that receipt for direct commands.

```powershell
rbx a pull CX
rbx a push CX
rbx a find CX -J find.json
rbx a tree CX -J tree.json
rbx a inspect CX -J inspect.json
rbx a bb CX Workspace -J ops.json
```

## Payloads

Put structured parameters in a JSON file. For example, `ops.json` can contain:

```json
{
  "ops": [
    { "type": "search", "q": "Door", "limit": 5, "fields": "lookup" },
    { "type": "counts" }
  ]
}
```

Use stdin when the payload is generated:

```powershell
Get-Content .\set-property.json | rbx a set-property CX -J -
```

```sh
rbx a set-property CX -J - < ./set-property.json
```

Never put structured JSON directly in a shell argument.

## Compact protocol fields

- Request: `v` protocol version, `id` request ID, `op` numeric opcode, `cx` bound context, `p` parameters.
- Success: `ok:1`, `ms` elapsed milliseconds, `r` result.
- Failure: `ok:0`, `e.c` stable code, `e.m` message, `e.rt` retry flag, `e.n` next operation, `e.d` details.
- Instance fields: `id`, `n` name, `c` class, `path`, `cc` child count, `ch` child IDs. Request only the fields needed.

Use a stable instance ID after `find` or `tree`. Duplicate paths require ordinal selectors. Never guess the project, place, runtime, sync direction, or duplicate instance.
