# Links, version control, and collaboration

See [projects](projects.md) for the main workflow.

## Reusable local/Git links

```powershell
rbx lka --id logger --source links/Logger.luau --service ReplicatedStorage --path '["ReplicatedStorage","Shared","Logger"]'
rbx lk
rbx lks
rbx lkb --service ReplicatedStorage --path '["ReplicatedStorage","Shared","Logger"]' --remove
```

`lka` adds a target, `lk` applies it, `lks` reports status, `lkb` detaches it (`--remove` also drops its record). Sources are project-relative; links are read-only unless `--writable`. Live Sync sends changed paths; otherwise push only when Studio needs them.
`lk --check` compares without applying. Package checks, status and read-only push guards compare script files as well as stored data and allow a linked model's placement to differ. Edit the source and reapply a drifting read-only copy; `--override-packages` on a push deliberately bypasses that protection.
Pulls keep read-only `.renium` mirrors at the saved package version, including ordinary local packages and missing targets. Only pulled services are restored; writable and broken links are left alone. Results list `readOnlyLinksRestored`. A difference does not tell which side is newer: shared source files may have changed since the package was saved. Run `lk` to repack from the source, or push the mirror path to apply the saved package in Studio. An override push keeps its own readback; a later pull restores the package again.
Git sources use `--source-type git --source REPOSITORY --ref REF --subpath PATH`; `lk` refreshes the cached ref, `--offline` requires a cache.
`lkp --link-folder packages --id ID` packages a subtree (same `--service`/`--path`) and registers its source; reuse the ID with `lka`. `lkd --id ID --action` deletes a package: `delete-unused` refuses active uses, `delete-uses` removes them, `unlink-uses` keeps editable copies.

### Shared links across places

```powershell
rbx --place lobby lkp --share --service ReplicatedStorage --path '["ReplicatedStorage","Public"]'
rbx lk --experience
rbx lks --experience
```

`lkp --share` in the source place packs `links/ID.renium` at the experience root, records it in `renium.experience.json` (`sharedLinks`) and links each place with the same path read-only (`--all-places`: every place). Edit only the source, then `rbx lk` in any place (`--experience` from anywhere) re-packs and updates every copy; Live Sync does this itself and pushes each copy. Copies hold read-only script files; other files under their root move to `.renium/link-replaced/ID/`.
Pull restoration also moves stray shared-copy files there, into separate folders so earlier replacements survive. Ordinary packages refuse unowned files; move those files outside the target and retry. Restore failures publish nothing and leave Studio changes pending. Successful restores refresh the sourcemap and Live Sync's file watcher while retaining Studio's exported version for later reconciliation.
A PackageLink right under the root is left out (`strippedPackageLinks`) and dropped from the copies; `rbx upl` unlinks it in the source. Nested packages travel as they are: pushes into packages the account does not own are refused before Studio is touched, so copies get those only unchanged. `lks --experience` shows `stale` packs and places as `ok`, `drift` or `missing`.

## Version control

Run `rbx vci` to install Git ignore/diff/merge rules; reruns are safe. `vct` renders stores as text; Git calls `vcm` for store merge conflicts.
Avoid `--untracked-files=all` on generated packages. Git tracks files; a commit or push does not publish Roblox content.

## Live collaboration

```powershell
rbx collab start --relay
rbx collab relay deploy
rbx collab join wss://host.trycloudflare.com/?token=abc -r C:/proj
rbx collab status
rbx collab invite
rbx collab stop
```

`start` shares the project's files as one live document and prints an invite; every participant's folder mirrors it. Without `--relay` the room runs on the host through a Cloudflare quick tunnel and dies with it; with `--relay` a relay keeps the room and its history, and its copy wins over a local folder on join. `relay deploy` publishes the relay to the user's free Cloudflare account once (needs Node.js; may open a browser sign-in) and makes it the default; `relay set URL` picks an existing one.
`join` fills an empty folder from the room and overwrites an existing one. Only the host runs Live Sync; others edit files and see results through Team Create or the host.
`status` lists participants, their open file and selection. Never start a second session for one folder; stop the first.
