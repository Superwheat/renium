# Typed operations and reviews

Read `RENIUM.md` first. Use `rbx a` only when the direct surface doesn't cover the operation. Payloads are JSON objects read from stdin with `-J -`; don't save them in the project. Bind once per sequence, then pass its context ID. Use `rbx clients` or `rbx studios` to list Studio runtimes without binding.

<!-- automation-operations:start -->
- Context: `cap`, `bind`, `context`, `unbind`
- Sync: `pull`, `push`, `live-start`, `live-stop`, `live-status`, `retry-pending`, `discard-pending`
- Read: `find`, `tree`, `inspect`, `batch`, `script-search`, `script-read`, `script-grep`
- Edit: `get-property`, `set-property`, `set-source`, `add`, `clone`, `move`, `remove`, `revert`, `multi-edit`
- Files: `import-model`, `export-model`, `export-place`, `import-snapshots`, `export-snapshots`, `sourcemap`
- Studio: `studios`, `studio-status`, `studio-open`, `studio-close`, `luau`, `console`, `play-start`, `play-stop`, `shot`, `device`
- Input and capture: `ui`, `press`, `click`, `key`, `type`, `wait`, `goto`, `input`, `record-start`, `record-end`
- Project: `project-init`, `project-validate`, `place-add`, `place-rename`, `place-reorder`
- Review: `review-prepare`, `review-apply`, `review-reject`
- Roblox Cloud and creator assets: `asset-search`, `asset-insert`, `generate-model`, `job-status`, `image-upload`, `image-store`
<!-- automation-operations:end -->

Common structured payload fields follow their names: selectors use `service` plus one of `settingsId`, `index`, `name`, `className`, or `pathSegments`/`pathOrdinals`; file operations use `model`, `output`, or `changedPaths`; client operations use `player`. Batch accepts an `ops` array.

Project creation and place management use `project-init`, `project-validate`, `place-add`, `place-rename`, and `place-reorder`; reorder uses published place IDs. Studio open/close, protected-property fallback, and destructive replacement use `review-prepare` and the returned receipt. Never retry a permanent error with a different operation.

```powershell
rbx a bind . --bootstrap
rbx a project-init CX
rbx a bind . <PLACE_ID>
rbx a place-add CX <PLACE_ID> "Place Name" --game-id <GAME_ID> --alias main
rbx a place-rename CX <PLACE_ID> lobby
rbx a place-reorder CX <PLACE_ID> <OTHER_PLACE_ID>
```

Rebind after `project-init`, `place-add`, `place-rename`, or `place-reorder` changes project identity.

Errors use stable codes. Rebind on `stale_cx`; choose a candidate on `ambiguous_place`; connect the matching Studio place on `no_studio` or `bridge_off`; correct the payload on `bad_req`. Only retry when `rt` is `1`, and retry the same operation at most once.
