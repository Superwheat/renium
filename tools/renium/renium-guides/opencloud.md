# Roblox Cloud and creator assets

Read `RENIUM.md` first. Cloud commands run without Studio or a daemon. Put the key in `ROBLOX_API_KEY`; for OAuth, put the token in another environment variable and add `--oauth-env ENV`. Never put credentials in commands or project files.

Roblox keys may belong to a user, a dedicated account used for a group, or be limited to selected resources. Renium uses the same commands for all three. Roblox enforces the key owner's permissions, its scopes, and its allowed universe, data-store, or creator targets. Check the active API key without exposing it:

```powershell
rbx oc key
```

Use `--key-env ENV` when a task needs a different key. A dedicated group key is normally stored in its own environment variable. Renium doesn't widen a key's access or retry with another credential.

The universe and place come from the current Renium project. Outside one, add `--universe ID` and, when needed, `--place-id ID` before the resource name.

## Native Open Cloud operations

Use the resource command instead of entering an HTTP method, route, or JSON body. Values that look like JSON numbers, booleans, arrays, or objects keep that type; other values are strings.

```powershell
rbx oc data stores --limit 25
rbx oc data get PlayerData user-42
rbx oc data upsert PlayerData user-42 '{"coins":100}'
rbx oc data increment Counters visits 1
rbx oc ordered list Wins --limit 20
rbx oc memory queue-add Matchmaking '{"userId":42}' --field ttl=60s
rbx oc universe message updates refresh
rbx oc restriction ban 42 "Exploit abuse" --field gameJoinRestriction.duration=86400s
rbx oc user inventory 42 --limit 25
rbx oc group role-assign GROUP MEMBERSHIP groups/GROUP/roles/ROLE
rbx oc place publish build.rbxl
rbx oc asset create Model "Street Lamp" "A lamp model" lamp.fbx --field creationContext.creator.groupId=GROUP
rbx oc localization game-info
rbx oc localization product-name PRODUCT fr "Nom français"
rbx oc ai speech "Welcome back" --field speechStyle.voiceId=VOICE
```

Data and ordered-store commands use the `global` scope by default; add `--scope NAME` for another scope. `update` requires an existing entry, while `upsert` may create one. `--field a.b=value` sets nested request fields without a JSON file. `--query name=value`, `--filter`, `--cursor`, `--if-match`, `--form`, and `--file` cover less common endpoint options.

List exact native operations and positional values only when needed:

```powershell
rbx oc routes
rbx oc routes data
rbx oc routes matchmaking
```

Native categories include `data`, `ordered`, `memory`, `universe`, `place`, `restriction`, `secret`, `notification`, `user`, `group`, `interaction`, `team`, `asset`, `creator-store`, `pass`, `localization`, `config`, `luau`, `server`, `advertising`, `analytics`, `avatar`, `badge`, `experiment`, `event`, `ai`, `matchmaking`, and `thumbnail`.

Use `rbx oc request` only when Roblox has added an endpoint that `rbx oc routes` doesn't list. Pipe a complex body to stdin instead of creating a payload file.

## Products, passes, and assets

Developer products have concise typed fields:

```powershell
rbx oc product list
rbx oc product get PRODUCT_ID
rbx oc product create "Refresh Daily Rewards" --price 27 --for-sale --regional-pricing
rbx oc product update PRODUCT_ID --price 29 --regional-pricing=false
```

Game-pass multipart fields use `--form`; images use `--file imageFile=PATH`. Native asset creation takes its main metadata and file as positional values; use `--field` for the user or group creator. Image asset upload requires an explicit owner:

```powershell
rbx oc pass create "VIP" --form price=99 --form isForSale=true --file imageFile=vip.png
rbx oc place publish place.rbxl
rbx oc asset create Model "Street Lamp" "A lamp model" lamp.fbx --field creationContext.creator.userId=USER_ID
rbx iu reference.png --user USER_ID --name Reference
```

Creator Store search and Studio insertion are separate direct commands:

```powershell
rbx as "wooden crate" --limit 5
rbx ai ASSET_ID --parent Workspace
rbx gm "small wooden crate" --parent Workspace --name GeneratedCrate
rbx js JOB_ID --wait-seconds 30
```

Uploads, writes, restrictions, notifications, server control, and other mutations must be explicitly requested. Use the normal web tool for current Roblox documentation.
