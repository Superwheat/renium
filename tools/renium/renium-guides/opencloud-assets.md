# Open Cloud assets and analytics

See [opencloud](opencloud.md) for the main workflow.

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
rbx oc asset create Model "Car" "Group car" car.glb --field creationContext.creator.groupId=GROUP_ID --content-type model/gltf-binary
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

`as --details` returns the store's full records (asset, creator, voting) including `hasScripts`; check them before inserting. Live Sync writes inserted assets to the project like any Studio edit: take what you need from inspection-only inserts and delete them in the same step.

Use normal web tools for Roblox documentation.
