# Open Cloud and creator assets

Cloud commands run without Studio. Put API keys in `ROBLOX_API_KEY`, or select an environment variable with `--key-env ENV` / `--oauth-env ENV`. Never put credentials in arguments or project files.

```powershell
rbx oc key
```

This reports key permissions without exposing the secret. Roblox enforces owner permissions, scopes, and targets; Renium does not widen access or switch credentials.

The project supplies universe/place IDs. Otherwise put `--universe ID` and `--place-id ID` before the resource.
Public reads can explicitly use `--anonymous`; authenticated requests never fall back to anonymous access.

## Resource commands

Prefer a resource command over raw HTTP. Run writes only when requested.

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
rbx oc localization game-info
rbx oc localization product-name PRODUCT fr "Nom français"
rbx oc ai speech "Welcome back" --field speechStyle.voiceId=VOICE
```

Data/ordered stores default to `global`; override with `--scope`.
`update` requires an entry; `upsert` may create one.
`--field a.b=value` sets nested JSON. JSON-like values retain their types; others are strings.
Additional options include `--query`, `--filter`, `--cursor`, `--if-match`, `--form`, and `--file`. Repeated query/form/file names retain every value.

Discover routes only when needed:

```powershell
rbx oc routes data
rbx oc routes matchmaking
rbx oc routes
```

Categories include data, ordered, memory, universe, place, restriction, secret, notification, user, group, interaction, team, asset, creator-store, pass, localization, config, luau, server, advertising, analytics, avatar, badge, experiment, event, ai, matchmaking, and thumbnail.
Use `oc request` for unlisted endpoints; pipe complex bodies through stdin.

## Analytics and media

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

## Products, passes, and assets

```powershell
rbx oc product list
rbx oc product get PRODUCT_ID
rbx oc product create "Refresh Daily Rewards" --price 27 --for-sale --regional-pricing
rbx oc product update PRODUCT_ID --price 29 --regional-pricing=false
rbx oc pass create "VIP" --form price=99 --form isForSale=true --file imageFile=vip.png
rbx oc asset create Model "Street Lamp" "A lamp model" lamp.fbx --field creationContext.creator.userId=USER_ID
rbx oc asset create Model "Street Lamp" "A lamp model" lamp.fbx --field creationContext.creator.groupId=GROUP_ID
rbx iu reference.png --user USER_ID --name Reference
rbx oc --anonymous asset search --limit 5 -q query=car -q searchCategoryType=Model
```

Pass images use `--file imageFile=PATH`. Asset creation takes metadata/file positionally and requires a creator owner.

Creator Store search and Studio insertion/generation:

```powershell
rbx as "wooden crate" --limit 5
rbx ai ASSET_ID --parent Workspace
rbx gm "small wooden crate" --parent Workspace --name GeneratedCrate
rbx js JOB_ID --wait-seconds 30
```

Use normal web tools for Roblox documentation.
