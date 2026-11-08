# Saved data

These commands read project files, not Studio. Select the Roblox service that owns
the target. Scripts live under the configured source folders; instance stores live
in `instances/` beside `src`.

## Find and read

```powershell
rbx f Workspace -n Door --limit 5
rbx f ServerScriptService -c Script --limit 5
rbx f ReplicatedStorage --path Vehicles.Cars -c Model
rbx tr Workspace Door --depth 2 --limit 100
rbx in Workspace -i editor:id
rbx in ReplicatedStorage Cars.Deltor.Engine Cars.Deltor.Wheels
rbx bg Workspace Lobby.Door Anchored Transparency CanCollide
rbx bg Workspace -i editor:id Name
rbx bg Workspace -p Gravity
rbx ss DataStoreService UpdateAsync --limit 20
rbx sg RemoteEvent --limit 100
```

`f SERVICE text` searches text; `-n` matches an exact name; `--path` limits the search to one subtree. `tr`, `in` and `f` also take one dotted argument (`rbx tr StarterGui.Main.HUD.Timer`; the first segment is the service). A `Property not found` error lists the nearest stored and class property names; `rbx in SERVICE TARGET -F props` lists them all.
`ss` finds scripts containing every keyword, case-insensitively; `sg` finds literal lines, case-sensitive by default. Limits cap returned results, not totals. Read known scripts directly; `sr` reads them when file access is unavailable.

Several new instances are one `rbx ba --batch -` run: pipe `{"ops":[{"parent":"ServerScriptService.Battle","name":"Abilities","class":"Folder","ref":"abilities"},{"parent":"#abilities","name":"CarMods","class":"ModuleScript","source":"return {}"}]}`; each store is written once; a validation failure writes nothing, and the error names any store already written.
`bg` with several property names prints one object; `in` with several targets prints one array: one decode, one call. Several reads across instances are one `bb` batch, not a `bg` per value: `'{"ops":[{"type":"search","q":"Door","limit":5,"fields":"lookup,prop:Anchored"},{"type":"counts"}]}' | rbx bb Workspace -J -`; `in SERVICE TARGET -F attrs,props` reads every field of one instance.
Use one lookup on the relevant state: `f` for saved data, `q` for a closed place, `l` for unsaved Studio state. Refine ambiguous results instead of repeating the query through other tools.

## Edit

```powershell
rbx bs Workspace -i editor:id -p Name --str "VIP Man"
rbx bs Workspace Lobby.Door -p Transparency --num 0.5
rbx bs Workspace -p Gravity --num 120
rbx bs Workspace -i editor:id -p Reviewed --scope attribute --bool true
rbx ba Workspace -n NewModel -c Model
rbx ba Workspace -I Lobby.Props -n NewPart -c Part -p 'Anchored=true' --push
rbx bs Workspace Lobby.Door -p 'Transparency=0.5' -p 'CanCollide=false' --push
rbx bs Workspace Lobby --where 'is:Part Transparency>0.5' -p 'CanCollide=false' --push   # every match under Lobby
rbx bss Workspace -i editor:script --str "return 1"
rbx bcl Workspace -i editor:source -I editor:parent
rbx mv Workspace -i editor:id -I editor:parent --push
rbx mv StarterGui -i editor:id --to-service ReplicatedStorage -I editor:parent
rbx cp Workspace -i Lobby.Pad -I Props -n Pad2 --push           # copy, name the copy, push it
rbx br Workspace Lobby.OldPart Lobby.Kiosk.Sign --push          # several targets in one call
rbx br Workspace Lobby --where 'Trim or is:PointLight' --push   # every match under Lobby
rbx rn Workspace -i Lobby --where 'Glass' --replace Glass Window_ --push   # Glass3 becomes Window_3
```

Every store command takes the target the same way: a positional name or dotted path (`Lobby.Door`), `-i ID`, `-x INDEX`, `-n NAME` or `-c CLASS`. `bg` takes the property after the target or as `-p`; `bs` takes `-p`. `bg`/`bs` with no target address the service itself. Parent options (`-I`) take an ID or a dotted path. Duplicates take inline ordinals (`Borders.Border[4]`, the form compact output prints) or `--ords`.
Use a service name or `-f STORE`, not both.
Use a service name or `-f STORE`, not both.

Values: `--str`, `--num`, `--bool`, `--null`, or `-j JSON` (`-j -` reads stdin); several `-p NAME=JSON` set several properties at once. Composite values take the property's type: `[x,y,z]` Vector3, `[x,y]` Vector2, `[r,g,b]` Color3 (bytes when any channel is above 1), `"Neon"` or `{"_type":"EnumItem","name":"Neon"}` for enums, `"Bright red"` or `{"_type":"BrickColor","number":21}` for a BrickColor, `[scale,offset]` UDim, `[xScale,xOffset,yScale,yOffset]` UDim2, `[min,max]` NumberRange, `{"_type":"CFrame","position":[x,y,z],"orientation":[rx,ry,rz]}` with the orientation in degrees (`Position` and `Orientation` given as properties fold into the CFrame of a part, attachment or camera, since Studio saves only the CFrame), and an instance path such as `"SoundService.SFX"` for a reference property (PrimaryPart, Attachment0/1, SoundGroup, Part0/Part1, an ObjectValue's Value); the stores keep the canonical form `rbx bg` shows. `--push` on `ba`, `bs`, `mv` and `rn` pushes that instance at once, no review; `ba --batch --push` pushes every created instance.

`rbx bs SERVICE -p ...` with no target edits the service itself (Lighting, SoundService, StarterPlayer); `ClockTime` on Lighting is stored as `TimeOfDay`, the form Studio saves. A service with no store yet is pulled once with `rbx pl -s SERVICE`. Properties a push cannot set (hidden or engine-managed in Studio's API, such as `Font`, `FontSize` and `BrickColor`) are refused with the replacement named: fonts are `FontFace` values, and `rbx l 'return Font.fromEnum(Enum.Font.GothamBold)'` returns the value to store. A `--push` result carries `studio`, the live read-back of the pushed instance (or the first 64 matches), and `verified`: `true` when every pushed property and attribute reads back equal and the instance sits at its path with its class, `false` when one differs or the instance is missing (the `studio` row shows which), absent when a live value's shape cannot be compared. `rn -i SCOPE --where QUERY --replace FROM TO` renames every match under the scope whose name contains FROM, in one push with `renamed`, `studio` and `verified`. `br` takes several targets after the first; `br --push` also destroys the removed instances in the open place (`studio.destroyed`, `studio.missing`); `br --where QUERY` removes every match under the target, 500 at most. `bs --where QUERY` applies the assignments to every instance under the target (the whole service without one) that matches the bulk-read query language: words match names, `is:Class`, `tag:Name`, `Prop=value`, `Prop>0.5` (also `!=`, `<`, `<=`, `>=`), `or` between alternatives. 500 matches at most per call; `--push` pushes them together and `studio` reads each back (the first 64). The result lists `instances` and `matched`.
`--null` removes an override; writing the default stores one.
Unknown properties are rejected. `--scope property` explicitly selects a real property absent from the schema; `--scope attribute` selects an attribute.

References use `-j '{"_type":"Ref","settingsId":"editor:target"}'`; clear with `--null`. Without an ID, use `pathSegments` and `pathOrdinals`.

Edit existing script files directly. For a new script, write the file and create its entry with `ba` (it adopts the file); `bss --str` or `--source-file` writes source through the store.

Check changed scripts together with `rbx ck`, which takes the `.luau`/`.lua` files git reports as changed in the project; name files or folders (`rbx ck src/ServerScriptService`) otherwise. It parses Luau offline without executing it; use `rbx ck -` for UTF-8 source on stdin. Errors include the file and syntax location, and return a failing exit code. This is not type checking, lint, or a behavior test.

`changedPaths` lists actual file changes; empty means no-op. Live Sync sends them automatically; without it, push returned paths with their settings IDs, not an entire service.

## Bulk and protected properties

See [Bulk and protected properties](data-properties.md).

## Inspect saved files

See [Inspect saved files](data-inspect.md).
