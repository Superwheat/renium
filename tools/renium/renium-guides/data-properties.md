# Bulk and protected properties

See [data](data.md) for the main workflow.

## Bulk reads

Request the needed fields once and analyze locally; don't make Studio scan saved data.

```powershell
'{"ops":[{"type":"search","q":"Door","limit":5,"fields":"lookup,prop:Anchored"},{"type":"counts"}]}' | rbx bb Workspace -J -
'{"ops":[{"type":"counts","id":"editor:folder"},{"type":"search","id":"editor:folder","q":"Door","limit":5,"fields":"lookup"}]}' | rbx bb Workspace -J -
```

Results are flat in `rs`. Scope `search`/`counts` to one instance by `id`, `path`, `index`, `name`, or `className`; each must match exactly one. `q` filters that scope: a word matches names, `is:Class` a class, `tag:Name` a tag, `Prop=value` (`!=`, `<`, `>`, `<=`, `>=`) a property or attribute, and dotted `*`/`**` patterns match paths; `or` separates alternatives.
`path` accepts `"Workspace.Lobby.Barrier"` or `["Workspace","Lobby","Barrier"]`; use an array when a name contains dots or slashes. A search without `q` lists the selected subtree up to `limit` (default 20; 0 means unlimited).
Field presets: `lookup=id,n,c,path`, `tree=id,n,c,cc,ch`, `brief=id,n,c,path,cc`.
Use `prop:Name`/`attr:Tags` for fields; `src` is a source path, `prop:Source` is exact text.
Missing requested properties use class defaults.

## Protected Studio properties

Use ordinary file edits and queries first. `access` is for live engine properties that normal APIs cannot access, not a replacement for saved-data workflows.

```powershell
rbx access read Workspace StreamingEnabled
rbx access read ReplicatedStorage.Shared.PackageLink Status ServerStorage.Tools.PackageLink
rbx access approve REQUEST_ID
rbx access write Workspace.Mesh CollisionFidelity Hull
rbx access mode read-only
rbx access mode ask
```

`read` takes more targets after the property and reads them all in one call under one approval (64 per call); `results` lists each path, class and value. Enumerate the instances first (`f`, `bb` or `l`), then read once.
The default `ask` mode returns an exact `approval-required` request for unlisted properties. Inspect its target, property and write value before approving; approvals expire, are single-use, and cannot transfer to replacement instances or sessions. `reject REQUEST_ID` discards one. CollisionFidelity is allowlisted with validated enum values.

`read-only` allows protected reads and rejects protected writes. `read-write` allows both, but requires the user's explicit request, a warning about unknown scripts/plugins, and `--accept-risk`. Modes apply only to the selected runtime; they do not restrict ordinary edits or Live Sync. Built-in performance diagnostics remain trusted operations.

Values use Studio's text representation, up to 64 KiB. Writes verify the result and mark affected packages Changed; report `autoDesyncedPackages`. If a write times out, read the value before retrying—it may still finish. Windows and macOS Edit mode only, not play clients.

`access call TARGET FUNCTION '[ARGUMENTS]'` supports HttpRbxApiService's `GetAsync`, `GetAsyncFullUrl`, `PostAsync`, `PostAsyncFullUrl`, and `GetDocumentationUrl`. `access batch TARGET FUNCTION '[[ARGUMENTS], [ARGUMENTS]]'` groups up to 32 ordered calls under one exact approval (60 KiB input, 30-second deadline). Calls require approval in `ask` and are rejected in `read-only`; unsupported functions are rejected before execution. Batches stop on failure, retain completed results, and label unconfirmed and unexecuted calls separately. Never replay a whole partial batch. See the Open Cloud guide for creator inventory and asset sharing.

After a Studio update, Renium rediscovers the native entry points itself. If validation fails, the error names the detector that needs updating; don't force an old address or retry the same failure.
