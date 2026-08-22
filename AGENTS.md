Renium is a full-fidelity two-way sync and automation tool for Roblox Studio. Its compact `rbx` CLI lets agents inspect and edit projects, capture screenshots, run playtests, and control Studio without manual interaction.

# Agent Notes

- Use `rbx`; the Renium installer adds both `renium` and `rbx` to `PATH`.
- If the current process has not picked up the new `PATH`, use `%USERPROFILE%\.renium\bin\rbx.cmd` on Windows or `~/.renium/bin/rbx` on macOS and Linux.
- Edit `src/*.ts`, not built `out/` (renium-vscode-extension).
- Full reference: `tools/renium/README.md`.

## Read

Prefer this order:

```powershell
# High-level first.
rbx f Workspace -c Script --limit 5
rbx tr Workspace Name --depth 2
rbx in Workspace -i editor:id

# Low-level (bb).
rbx bb Workspace -j '{"ops":[{"type":"counts"},{"type":"search","q":"text","limit":5,"fields":"lookup"},{"type":"children","id":"editor:id","fields":"tree"}]}'
rbx bb Workspace -j '{"ops":[{"type":"instance","id":"editor:id","fields":"brief"}]}'
```

```text
f       = locate ids by name/class
tr      = browse descendants
in      = one high-level node read
bb      = low-level reads, properties, duplicate paths, batches
write   = only after stable id/path
```

## `bb` recipes

```powershell
# Search, then inspect a returned id.
rbx bb Workspace -j '{"ops":[{"type":"search","q":"VipMan","limit":5,"fields":"lookup"},{"type":"instance","id":"editor:id","fields":"brief"}]}'

# Duplicate path: include ords.
rbx bb Workspace -j '{"ops":[{"type":"instance","path":["Workspace","A"],"ords":[2],"fields":"brief"},{"type":"children","path":["Workspace","A"],"ords":[2],"fields":"tree"}]}'

# Only request needed values.
rbx bb Workspace -j '{"ops":[{"type":"instance","id":"editor:id","fields":"brief,prop:Name,attr:Tags"}]}'

# Locate -> read -> mutate.
rbx f Workspace -n VipMan --limit 5
rbx bb Workspace -j '{"ops":[{"type":"instance","id":"editor:id","fields":"brief,prop:Name"}]}'
rbx bs Workspace -i editor:id -p DisplayName --str "VIP Man"
```

For a larger batch, pipe the query instead of creating a payload file.

```powershell
'{"ops":[{"type":"counts"},{"type":"search","q":"text","limit":5,"fields":"lookup"}]}' | rbx bb Workspace -J -
```

Fields:

```text
lookup = id,n,c,path
tree   = id,n,c,cc,ch
brief  = id,n,c,path,cc
prop:X only when needed
attr:X only when needed
```

Output keys:

```text
top:   f=settingsFile s=service rs=results t=opType q=requestId
count: r=rootIds rc=rootChildren d=descendants m=matches v=visibleIds ns=nodes
node:  id x=index n=name c=class pid/px=parent cc=childCount ch=childIds path ords src props attrs
```

Op aliases: `type|op`, `requestId|rid`, `q|query`, `limit|l`, `id|settingsId`,
`x|index`, `n|name`, `c|className`, `path`, `ords`, `props`, `attrs`.

## Multi-client testing

```powershell
rbx play -s --players 2        # 1 server + 2 clients
rbx cs                         # list bridges (role, player, place)
rbx lx --player 2 -e "code"    # run on one client (name|index)
rbx co --player 2 -n 20        # client console
rbx play -x                    # end test
```

`rbx l` = server during any test. `rbx lc "code" Player2` = `lx --player Player2 -e`.
Luau execution returns values and captured output; compile errors, runtime errors, and timeouts exit nonzero. Use `rbx lx -e "code" -t 5` to change the 10-second limit.

## Device emulation

Use Studio's plugin-level simulator directly. No keyboard, mouse, focus, coordinates, or ribbon interaction is needed.

```powershell
rbx dev list
rbx dev set "iPhone 16 Pro" --orientation portrait
rbx dev set --scaling fit
rbx dev set --resolution 1179x2556 --pixel-density 460
rbx dev status
rbx sc --studio -o iphone-16-pro.png
rbx dev stop
```

Notched phones reproduce Studio's real safe-area behavior for `DeviceSafeInsets`, `ClipToDeviceSafeArea`, and `SafeAreaCompatibility` validation. Device names and stable ids from `rbx dev list` are both accepted. With emulation active, `rbx sc` automatically captures the simulated Studio viewport; `--studio` makes that target explicit and `--client` forces the latest Play client instead.

2+ games open = cmds refuse and list places; pin with `PLACE=<name|id>`
(substring ok).

## Input (real OS input, no focus needed; `-p` = client name|index)

```powershell
rbx ui -p 2                              # FIRST: visible buttons/textboxes (path, id, text, x/y)
rbx pr "PlayerGui.Shop.BuyButton" -p 2   # press GuiButton (path under PlayerGui)
rbx pr "Shop[2].BuyButton" -p 2          # [n] = duplicate names
rbx pr -i 0_328107 -p 2                  # press by id from ui output
rbx clk 450 323 -p 1                     # raw click
rbx ky E -p 2                            # key (A-Z 0-9 Space Enter Escape Tab arrows Shift Ctrl Alt)
rbx ty "hi" --path "PlayerGui.P.Box" --enter -p 2   # focus TextBox + type
rbx sc -o shot.png -p 2                  # screenshot (unfocused/minimized ok)
rbx wait "workspace:GetAttribute('Ready') == true" -c -t 20   # poll until truthy
```

Ambiguous path = fails listing candidates; one visible match = auto-picked.
Verify via `rbx co --player N` or `sc`.

## World

```powershell
rbx goto "Workspace.Shop.Door" -p 2      # pathfind-walk (--tp = teleport)
rbx goto --pos "745,40,510" -p 2
rbx pr "Workspace.Button" --world -p 2   # click part's screen position
rbx ky E -p 2                            # ProximityPrompt after goto
```

`--world` fails off-screen (goto first). ClickDetectors NEVER fire from injected
clicks (engine limit) — use ProximityPrompts or UIS/raycast. `pr` auto-scrolls
ScrollingFrames. `ui` = on-screen only (`--all` for rest). `--hold <ms>` on pr/clk.

Each `l`/`lc` kills threads spawned by the previous one; persist state server-side.

## Play / Luau

Keep Luau compact:

```text
action-first
short direct code
compact locals: P, R, lp, ch, r
no helper wrappers
no setup/staging unless required
poll only values that can be missing
prefer wait() over task.wait() when equivalent
use rbx lc for client cmds in Play
```

```lua
local P=game:GetService("Players")
local lp repeat wait() lp=P.LocalPlayer until lp
local ch=lp.Character or lp.CharacterAdded:Wait()
local r=ch:WaitForChild("HumanoidRootPart")
ch:PivotTo(r.CFrame+r.CFrame.LookVector*50)
```

## Write

```powershell
rbx bs Workspace -i editor:id -p Name --str NewName
rbx bss Workspace -i editor:script --str "print('hi')"
rbx ba Workspace -n NewModel -c Model
rbx bcl Workspace -i editor:source -I editor:parent
rbx br Workspace -i editor:id
```

## Rules

```text
use `f` to get an id first
use service name first
use -f only if needed
never use both service and -f
use -i editor:id when names repeat
use --path '["Workspace","A"]' --ords 2 for duplicate paths
do not mix --path with -i/-x/-n/-c
```

Error `Provide either SERVICE_OR_FILE or --file, not both` = you passed both.

⁣Read and follow RENIUM.md.⁣
