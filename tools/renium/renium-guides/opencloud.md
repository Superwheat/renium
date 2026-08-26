# Roblox Cloud and creator assets

Cloud commands don't use Studio or a daemon. Store API keys in `ROBLOX_API_KEY`; store OAuth tokens in another environment variable and use `--oauth-env ENV`. Never put credentials in commands or project files.

The same commands support user, group-automation, and resource-limited keys. Roblox enforces owner permissions, scopes, and allowed targets. Inspect the active key without exposing it:

```powershell
rbx oc key
```

Use `--key-env ENV` for another key. Renium never widens access or switches credentials.

Public reads may use `--anonymous`, for example `rbx oc --anonymous asset search --limit 5 -q query=car -q searchCategoryType=Model`. Authenticated requests never fall back to anonymous access.

The current project supplies universe and place IDs. Otherwise add `--universe ID` and, if needed, `--place-id ID` before the resource.

## Native Open Cloud operations

Prefer resource commands over raw HTTP routes. JSON-like values keep their type; other values are strings.

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

Data and ordered stores default to `global`; change it with `--scope`. `update` requires an entry; `upsert` may create one. `--field a.b=value` sets nested fields. Less common options use `--query`, `--filter`, `--cursor`, `--if-match`, `--form`, or `--file`.

List routes only when needed:

```powershell
rbx oc routes
rbx oc routes data
rbx oc routes matchmaking
```

Categories include `data`, `ordered`, `memory`, `universe`, `place`, `restriction`, `secret`, `notification`, `user`, `group`, `interaction`, `team`, `asset`, `creator-store`, `pass`, `localization`, `config`, `luau`, `server`, `advertising`, `analytics`, `avatar`, `badge`, `experiment`, `event`, `ai`, `matchmaking`, and `thumbnail`.

Use `rbx oc request` only for an unlisted endpoint. Pipe complex bodies through stdin.

## Analytics, events, experiments, and thumbnails

These commands use the current universe from the project unless `--universe ID` is set:

```powershell
rbx oc analytics metrics --field metric=DailyActiveUsers --field granularity=OneDay --field startTime=2026-01-01T00:00:00Z --field endTime=2026-02-01T00:00:00Z
rbx oc analytics metrics-operation OPERATION
rbx oc event list --limit 10 -q fields=id,title,startTime,visibility
rbx oc event get EVENT -q fields=id,title,userRsvpStatus
rbx oc experiment list --limit 25 -q searchKey=BossHealth
rbx oc experiment stats EXPERIMENT
rbx oc experiment start EXPERIMENT
rbx oc thumbnail personalization --limit 10
rbx oc thumbnail personalization-create --field homepageThumbnailIds='["THUMBNAIL_1","THUMBNAIL_2"]'
rbx oc thumbnail upload first.png --file files=second.png
rbx oc thumbnail upload-status -q operationIds=OPERATION_1 -q operationIds=OPERATION_2
```

JSON writes use `--field`. Repeating the same `-q`, `--form`, or `--file` name keeps every value.

## Products, passes, and assets

Developer products:

```powershell
rbx oc product list
rbx oc product get PRODUCT_ID
rbx oc product create "Refresh Daily Rewards" --price 27 --for-sale --regional-pricing
rbx oc product update PRODUCT_ID --price 29 --regional-pricing=false
```

Game passes use `--form`; images use `--file imageFile=PATH`. Asset creation takes metadata and file positionally; set the creator with `--field`. Image uploads require an owner:

```powershell
rbx oc pass create "VIP" --form price=99 --form isForSale=true --file imageFile=vip.png
rbx oc place publish place.rbxl
rbx oc asset create Model "Street Lamp" "A lamp model" lamp.fbx --field creationContext.creator.userId=USER_ID
rbx iu reference.png --user USER_ID --name Reference
```

Creator Store and Studio commands:

```powershell
rbx as "wooden crate" --limit 5
rbx ai ASSET_ID --parent Workspace
rbx gm "small wooden crate" --parent Workspace --name GeneratedCrate
rbx js JOB_ID --wait-seconds 30
```

Run mutations only when requested. Use normal web tools for Roblox documentation.
