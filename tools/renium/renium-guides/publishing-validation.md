# Place-file validation and Open Cloud limits

Roblox's [place publishing API limitations](https://create.roblox.com/docs/cloud/guides/usage-place-publishing#limitations)
apply to `publish --open-cloud`, `publish --as`, and `oc place publish FILE`,
including uploads with `versionType=Saved`. The API cannot update
`EditableImage`, `EditableMesh`, `PartOperation` (including `UnionOperation`),
`SurfaceAppearance`, or `BaseWrap` (including `WrapLayer` and `WrapTarget`).
Roblox requires publishing from Studio after modifying those instances. Their
presence alone does not prevent an Open Cloud upload.

Before uploading, the agent must verify that these instances are unchanged from
the destination place, including their serialized data, properties and hierarchy.
Check the exact upload file against a current destination version; account for
additions and removals too. Use `oc place history` and `oc fetch --version N` for
the intended destination, then `cmp BEFORE AFTER --full --all` to inspect changes.
A script-only diff is insufficient, and the full comparison's ignored or
normalized fields do not establish that opaque instance data is unchanged (see
[file comparisons](data-inspect.md)). If anything changed or cannot be verified,
do not upload through Open Cloud; use `rbx publish` from the destination's Studio
session. `--as` still uses Open Cloud and has the same restrictions. This is an
agent verification requirement: Renium does not fetch or compare the destination,
require a confirmation flag, or reject a file merely for containing these types.

Before any place-file upload, Renium checks the exact bytes it will send: nonempty,
at most 100 MiB and parseable binary/XML. Binary checks
reject known floating-point wire-type mismatches before the repair reader can
normalize them, such as `Camera.OrthographicSize` or `Camera.ViewStretch` encoded
as Float64 instead of Float32. It sends the validated bytes unchanged with the
matching content type. Direct `oc place publish` and raw requests to the place
version endpoint use the same checks; neither is a bypass.

Run the checks locally without credentials or an upload:

```powershell
rbx publish --open-cloud --file build.rbxl --universe 123 --place-id 456 --dry-run
```

A passing check is not proof that Studio or a Roblox server can load or run the
place. Validation depends on Renium's parser and reflection database; it does not
test engine-only constraints, asset access, scripts, player joins, or teleports.
Opening a file with Renium or comparing its values can hide repaired encodings.
An accepted upload and `live: true` establish version publication, not server
health. When engine loading or joins need verification, test the exact file in
Studio or an authorized test place; Renium does not automatically save, publish
or run an engine test as part of local validation.

## Engine checks without local Studio

[Open Cloud Luau Execution](https://create.roblox.com/docs/cloud/reference/features/luau-execution)
loads a selected place version on Roblox's servers. To check a staged full place:

1. Fetch the destination and import it with `pi`. Preserve that build base:
   `bep` reports `baseSource: import`, or use `--base FILE`. This retains services
   and saved fields omitted from project files.
2. Apply changes, build, compare and verify limited instances as above. Upload
   with `publish --open-cloud --file FILE --saved` and explicit destination IDs.
   Saved uploads require authorization and obey the same API limits.
3. Run `oc luau run-version VERSION` against the returned saved version. Assert
   the intended source, hierarchy, service settings, references and relevant data.
   Read the task result and logs; upload acceptance is not a passing check.
4. Recheck destination history for concurrent changes. When the checks pass and
   publication is authorized, a task loading that verified version can call
   `AssetService:SavePlaceAsync({PlaceId = game.PlaceId, SaveWithoutPublish = false})`.
   Confirm the published version in history, then load it for a final check.

This workflow uses `oc luau`; `publish` does not run it automatically. Tasks have
execution/input limits and do not automatically run place scripts or simulate
physics. Loading, assertions and raycasts do not prove player joins, teleports,
rendering, asset delivery or gameplay.

Do not pass an entire `.rbxl` to `SerializationService:DeserializeInstancesAsync`
as a place replacement. [SerializationService](https://create.roblox.com/docs/reference/engine/classes/SerializationService)
handles `.rbxm` instance trees; non-creatable service roots are unsupported. A
full-place experiment omitted Workspace, StarterGui and StarterPack contents.
Importing explicit child trees into existing services can work, but external
references, service properties, Terrain, packages and conflicts need separate
handling. It is not a general substitute for the full-place upload above.
