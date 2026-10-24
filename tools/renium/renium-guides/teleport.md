# Playtest teleport in Studio

Start solo Play with `rbx play -s`. On Windows, Renium intercepts the owned server's `TeleportService:TeleportAsync(placeId, players, options)`, including startup PlayerAdded calls. The call returns a native TeleportAsyncResult. No game adapter is needed when `play -s` reports `trigger: "teleport-service"`.

Use published source IDs and stored Open Cloud metadata, saved-version history and delivery read access: `RENIUM/opencloud.md`. Each attempt freshly authorizes the destination and selects its newest **saved** version, including unpublished saves. An unreadable history or destination fails before leaving the current test. Cached bytes never replace authorization.

`playTeleportEnabled` and `playTeleportNative` default to true. `rbx cfg set playTeleportNative false --scope place` selects content emulation; `rbx cfg set playTeleportEnabled false --scope place` disables travel. Restart Play after changes.

## Native arrival

The original Studio controller loads the destination's saved place into new server and client Play DataModels in the same window. Native PlaceId and GameId are set during construction. Destination scripts, properties, hierarchy, Terrain and packages use Studio's place loader. Native Player:GetJoinData and TeleportService:GetLocalPlayerTeleportData receive TeleportOptions data. Studio's API access settings still apply; this does not create a production server, reservation or JobId.

Stop preserves the original Edit model and its unsaved work. Further teleports use that same controller. Preparation failures retain the current test; failures after switching starts return to Edit. Runtime identities and launch nonces reject late replies.

On Windows, arrival appends ` - Teleported to <destination displayName>` to the ordinary Studio title. Returning to the source removes it. Stop, disconnect and daemon exit restore the title without activating or moving the window. A process-local cleanup lease also restores it after an abrupt daemon exit. A restarted daemon can rearm completed arrivals with `rbx play -s`; interrupted arrivals return to Edit.

The current executor supports solo Play/Play Here and same-universe destinations. Server & Clients routing, partial groups, reserved-server codes, ShouldReserveServer and JobId targeting are refused. Custom teleport GUIs are not transferred. TeleportOptions data must be JSON-compatible and at most 64 KiB for the group.

## Compatibility mode

When native destination loading cannot be qualified, Renium can import supported content into the existing Play models. The result/log identifies `mode: "emulated"`; engine identity and backend context remain the source's. Further hops stay in this mode until Stop, including after reconnecting. If raw interception is unavailable, including on macOS, the optional server adapter is:

```lua
if game:GetService("RunService"):IsStudio() then
    require(game:GetService("ReplicatedStorage"):WaitForChild("ReniumTeleport")).Teleport(placeId, players, options)
else
    game:GetService("TeleportService"):TeleportAsync(placeId, players, options)
end
```

The adapter returns a queued receipt. Pass connected Players and optional TeleportOptions. WaitForChild handles plugin startup; early calls wait up to 30 seconds for arming. Disabled, disconnected or busy sessions reject requests. Keep the game's server validation. A game module already named ReniumTeleport is preserved and disables the adapter.

Emulation supports Workspace, Lighting, ReplicatedFirst, ReplicatedStorage, ServerScriptService, ServerStorage, StarterGui, StarterPack, StarterPlayer, Teams and SoundService. Unsupported containers, Terrain voxels, PackageLinks and incompatible protected settings fail preflight. Source overrides of default player controllers require destination replacements. Client-created source children are cleared before commit. Failure after clearing requires Stop.

`ReniumTeleport.GetContext()` reports `mode` (`source`, `native` or `emulated`), destination `placeId`, previous `sourcePlaceId`, original Edit `originPlaceId`, `gameId`, saved `version`, `contentSource` (`cache`/`download`) and attempt `id`. In emulation, use its GetJoinData(player), GetLocalPlayerTeleportData() and GetArrivingTeleportGui() methods. Existing Players remain, so enumerate GetPlayers as well as subscribing to PlayerAdded. Old callbacks, globals and module caches cannot all be reset by content replacement.

macOS currently uses compatibility mode and reports native switching and title support unavailable.

## Cache and diagnostics

Renium's 1 GiB `cache/play-teleport` cache keys saved bytes by universe/place/version and pins active entries. Downloads and prepared payloads are limited to 128 MiB; compatibility staging is limited to 250,000 instances. Daemon logs report mode, destination IDs, saved version and cache use.

`rbx status` reports native qualification. Read-only discovery is documented in `RENIUM/native-compatibility.md`.
