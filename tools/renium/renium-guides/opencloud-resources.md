# Open Cloud resource operations

See [opencloud](opencloud.md) for the main workflow.

## Resource commands

`rbx publish --open-cloud` builds the project or accepts `--file`.
It and `rbx oc place publish FILE` validate size, parsing and binary float widths
before upload, including `versionType=Saved`. Check locally:
`rbx publish --open-cloud --file FILE --universe ID --place-id ID --dry-run`.

Before any Open Cloud upload (`--as` included), agents must verify `EditableImage`,
`EditableMesh`, `PartOperation`/`UnionOperation`, `SurfaceAppearance` and `BaseWrap`
subclasses are unchanged from the destination, including additions/removals and
serialized data. If changed or unverified, use Studio's `rbx publish`. Renium does
not compare destinations or reject these types for being present. Local checks
do not satisfy this verification; see
[validation and API limits](publishing-validation.md).

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
Other options: `--query`, `--filter`, `--cursor`, `--if-match`, `--form`, `--file`; repeated names keep every value.

`rbx oc CATEGORY --help` lists that category's actions with their values, method and path, marks paged and destructive ones, and says which flag fills `{place}` (`--place-id`) or `{universe}`. An unknown action fails at once with the valid list. Actions that answer with an empty body (such as `universe restart`) return what ran. On paged actions `-l N` is a total fetched 100 per request, `--all` follows every page, `--pages N` stops after N, and `"more": true` says a page remains.

```powershell
rbx oc universe                              # includes playing, visits, votes, updated (last save or publish)
rbx oc server list                           # active servers of the newest versions, by version
rbx oc server find JOB_ID
rbx oc server logs VERSION JOB -l 1000 --severity error --grep "Ranked"
```

Categories include data, ordered, memory, universe, place, restriction, secret, notification, user, group, interaction, team, asset, creator-store, pass, localization, config, luau, server, advertising, analytics, avatar, badge, experiment, event, ai, matchmaking, and thumbnail.
Use `oc request` for unlisted endpoints; pipe complex bodies through stdin.

`team members` lists who is in a place's Team Create session now; version-history saves only show who was there then, and neither blocks `rbx publish` from Studio. `team remove-members` does not end a session: it revokes their collaborator access for the whole universe; only run it when the user asks for exactly that.

After an upload, `publish` and `oc place publish` report `live: true` once the version history lists the version as the current published one, which is normally the first check (`--wait-live SECONDS` bounds it on `publish`); a restart right after that result lands on the new version. In `oc place history`, `publishStatus` 1 marks the version players currently get, 2 a published version a later publish replaced, 0 a save; Studio publishes show 0 like saves and only `isPublished` is true for Open Cloud uploads. The `updated` stamp from `oc universe` moves on saves as well as publishes, so it does not mean a publish went live.

## Publish while someone else is in the live place

An Open Cloud upload to a place that another collaborator has open in Team Create fails with HTTP 409; the error names the members and any newer unpublished saves. Publish from inside the session instead:

```powershell
rbx oc fetch --version N -r ./live             # the live place as a project
rbx cmp ./live/place.rbxl --against dev.rbxl --full   # confirm their saves changed nothing you need
rbx so ./live/place.rbxl                       # open the live place
rbx cfg set liveSync.initialSyncPriority verify   # in ./live: first contact must not overwrite anything
rbx --place LIVE_PLACE_ID ps src/Path/Changed.luau    # copy only the changed files in, then push them
rbx --place LIVE_PLACE_ID ps --verify
rbx --place LIVE_PLACE_ID publish              # Studio-side publish inside the session
```
