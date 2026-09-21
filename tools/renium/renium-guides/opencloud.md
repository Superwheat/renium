# Open Cloud and creator assets

Cloud commands run without Studio. Store a key once with `rbx oc key add NAME` (the key is read from a hidden prompt, never from arguments) and every `rbx oc` command uses it; `--key NAME` picks another stored key, `ROBLOX_API_KEY` or `--key-env ENV` / `--oauth-env ENV` override the store. Keys are kept per user outside every project (DPAPI on Windows, the Keychain on macOS). Never put credentials in arguments, project files or shell profiles.

```powershell
rbx oc key add studio
rbx oc key list
rbx oc key
```

`key` alone reports the active key's permissions without exposing the secret. Roblox enforces owner permissions, scopes, and targets; Renium does not widen access or switch credentials.

## Find and pull an experience

```powershell
rbx oc games
rbx oc games "Brainrot Town"
rbx oc fetch "Brainrot Town" -r ./BrainrotTown
rbx oc fetch --universe 8108639406 -o brainrot.rbxl
```

`games` lists what the key can reach: the universes it is scoped to plus the public experiences of the key's user and groups; a name matches ignoring case, emoji and punctuation. `fetch` downloads the experience's root place and, with `-r DIR`, imports it into that project (creating it) so the files are ready to open with `rbx so`. Private experiences the key is not scoped to need `--universe ID` or `--place-id ID`.

The project supplies universe/place IDs. Otherwise put `--universe ID` and `--place-id ID` before the resource.
Public reads can explicitly use `--anonymous`; authenticated requests never fall back to anonymous access.

## Resource commands

For place publishing, prefer `rbx publish --open-cloud`: it builds the selected
project (or accepts `--file`) and checks Open Cloud's place-file fidelity limits.
See `advanced.md` for Studio publishing, targeting, and dry runs.

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

For creator inventory reads that need Studio's existing login, use
`rbx access call HttpRbxApiService GetAsyncFullUrl '["https://apis.roblox.com/creator-inventory-api/v1/-/creator-inventory-items:search?maxPageSize=25&filter=assetTypes%3DModel%3Bsources%3DCreated"]'`,
then `rbx access approve REQUEST_ID` for that exact call. The response is in `value`;
pass each `nextPageToken` as `pageToken` until absent. Query asset types separately;
add `;groupIds=GROUP_ID` inside the URL-encoded filter for group uploads.
This does not change Roblox's owner permissions. Use `oc asset permissions` for
authorized sharing; its key needs `asset-permissions:write`.

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
