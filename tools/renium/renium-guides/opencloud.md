# Roblox Cloud and creator assets

Read `RENIUM.md` first. Open Cloud commands run locally: they don't need Studio, a daemon, or `rbx a bind`. Put the key in `ROBLOX_API_KEY`; Renium also reads newly saved Windows user variables without a restart. Never put credentials in arguments, JSON, project files, or output. For an OAuth access token, save it in another environment variable and pass its name with `--oauth-env ENV`.

Use the normal web tool to read current Roblox documentation. Renium doesn't duplicate HTTP/document search.

## Developer products

The universe ID comes from `renium.experience.json`; otherwise pass `--universe ID`. Mutations are read back after Roblox accepts them.

```powershell
rbx cloud product list
rbx cloud product get PRODUCT_ID
rbx cloud product create "Refresh Daily Rewards" --price 27 --for-sale --regional-pricing
rbx cloud product update PRODUCT_ID --price 29 --regional-pricing=false
```

`--description`, `--image`, `--managed-pricing`, and `--store-page` are available where Roblox supports them.

## Any Open Cloud endpoint

Paths can use `{universe}` and `{place}` from the current project. `--field` builds JSON. `--form`, `--json-part`, and `--file` build multipart data. `--url-field` builds URL-encoded data. `--body-file` sends a raw file; add `--content-type` when its extension isn't enough. Use `--output FILE` for binary responses. `--header` handles endpoint metadata such as Data Store headers, but authentication comes only from the environment. Values that parse as JSON keep their type; other values are strings.

```powershell
rbx cloud request get /cloud/v2/universes/{universe}/data-stores --query maxPageSize=25
rbx cloud request post /cloud/v2/universes/{universe}:publishMessage --field topic=updates --field message=refresh
rbx cloud request patch /cloud/v2/universes/{universe}/user-restrictions/42 --field active=true
```

For a nested body, pipe one batch instead of creating a temporary file:

```powershell
'{"requests":[{"method":"POST","path":"/cloud/v2/universes/{universe}:publishMessage","body":{"topic":"updates","message":"refresh"}}]}' | rbx cloud batch -J -
```

Use `--anonymous` only for public endpoints. Use `--param name=value` for custom path placeholders and `--universe` or `--place-id` when no project identity is available. `rbx upload-place` is the direct streaming command for `.rbxl` and `.rbxlx` publishing.

## Creator assets

Creator Store search doesn't need Studio. Insertion and model generation change the matching live Edit runtime.

```powershell
rbx asset-search "wooden crate" --limit 5
rbx asset-insert 182451181 --parent Workspace --name AuditCrate
rbx generate-model "small wooden crate" --parent Workspace --name GeneratedCrate --size 4,4,4 --max-triangles 2000
rbx job-status JOB_ID --wait-seconds 30
rbx image-store assets/reference.png
rbx cloud image-upload assets/reference.png --user USER_ID --name Reference
```

Image upload requires explicit `--user` or `--group` ownership and writes to Roblox. Run it only when the user asked for an upload.
