# Place-file validation and Open Cloud limits

Roblox's [place publishing API limitations](https://create.roblox.com/docs/cloud/guides/usage-place-publishing#limitations)
apply to `publish --open-cloud`, `publish --as`, and `oc place publish FILE`,
including uploads with `versionType=Saved`. The API cannot update
`EditableImage`, `EditableMesh`, `PartOperation` (including `UnionOperation`),
`SurfaceAppearance`, or `BaseWrap` (including `WrapLayer` and `WrapTarget`).
Roblox requires publishing from Studio after modifying those instances. Renium
conservatively refuses files containing them, since a file alone cannot establish
whether they changed. Use `rbx publish` from the destination's Studio session;
`--as` still uses Open Cloud and has the same restrictions.

Before any place-file upload, Renium checks the exact bytes it will send: nonempty,
at most 100 MiB, parseable binary/XML, and no unsupported instances. Binary checks
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
health. Test the exact file in Studio and use an authorized test place for engine
and join checks before deploying; Renium does not automatically save, publish or
run an engine test as part of local validation.
