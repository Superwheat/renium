# Playtest teleport in Studio

Start with Studio's Play/Play Here or Server & Clients, `rbx play -s`, or `rbx play -s --players 2`. Windows prearms Edit and intercepts `TeleportService:TeleportAsync(placeId, players, options)`. CLI multiplayer arms before adding clients; early PlayerAdded calls wait for their bridges. Calls return native TeleportAsyncResult; the `teleport-service` trigger needs no adapter. Transient setup failures retry; Stop rearms the next launch.

Use published IDs and Open Cloud read access: `RENIUM/opencloud.md`. Every attempt authorizes the destination and selects its newest **saved** version, including unpublished saves. Unreadable history/destinations fail before leaving; caching never bypasses authorization.

`playTeleportEnabled` and `playTeleportNative` default to true. `rbx cfg set playTeleportNative false --scope place` selects content emulation; `rbx cfg set playTeleportEnabled false --scope place` disables travel. Restart Play after changes.

## Native arrival

Studio's place loader creates destination Play DataModels with native PlaceId/GameId, scripts, hierarchy, properties, Terrain and packages. Player:GetJoinData and TeleportService:GetLocalPlayerTeleportData receive TeleportOptions data. Studio API settings still apply; no production server, reservation or JobId is created.

Stop preserves the original Edit model and unsaved work. Further hops retain that controller. Preparation failures keep the current test; failures after switching return to Edit. Runtime identities and launch nonces reject late replies.

The title shows `Teleporting...`, then ` - Teleported to <destination displayName>` on arrival; return removes it. Roblox's default loading screen shows the destination both ways. Stop/disconnect restores the title without focus or movement; a lease covers daemon exit. After daemon restart, `rbx play -s` rearms completed solo arrivals; interrupted ones return to Edit.

Native travel supports same-universe destinations. Reserved-server codes, ShouldReserveServer and JobId targeting are refused. Custom GUIs are not transferred. TeleportOptions data must be JSON-compatible, at most 64 KiB for the group.

In Server & Clients, selected clients keep their windows; others stay on source. Different players' requests queue in order. First travel starts one destination server; later arrivals reuse it and its saved version, even when empty. Return rejoins the original source server. Root Stop closes all owned tests, including interrupted arrivals. Rejected requests return an error to TeleportAsync.

Test-player names and UserIds stay with their client across hops. Use the client's `runtimeId` from `rbx cs` with `-p` for exact targeting; ambiguous names are rejected. JoinData and local TeleportData carry the previous place and supplied data. Multiplayer daemon-restart recovery is unsupported; Stop and restart.

## Compatibility mode

If native loading is unavailable, compatibility mode imports content into existing Play models. Logs report `mode: "emulated"`; engine/backend identity remains source. Hops stay emulated until Stop. If interception is unavailable, including on macOS, use this server adapter:

```lua
if game:GetService("RunService"):IsStudio() then
    require(game:GetService("ReplicatedStorage"):WaitForChild("ReniumTeleport")).Teleport(placeId, players, options)
else
    game:GetService("TeleportService"):TeleportAsync(placeId, players, options)
end
```

The adapter returns a queued receipt. Pass connected Players and optional TeleportOptions. Early calls wait up to 30 seconds for arming. Disabled/disconnected sessions reject requests. Keep server validation. An existing game module named ReniumTeleport is preserved and disables the adapter.

Emulation supports Workspace, Lighting, ReplicatedFirst, ReplicatedStorage, ServerScriptService, ServerStorage, StarterGui, StarterPack, StarterPlayer, Teams and SoundService. Unsupported containers, Terrain voxels, PackageLinks and incompatible protected settings fail preflight. Custom player controllers need destination replacements. Client-created source children are cleared; failure afterward requires Stop.

`ReniumTeleport.GetContext()` reports mode, placeId, sourcePlaceId, originPlaceId, gameId, saved version, contentSource and attempt id. In emulation, use its GetJoinData(player), GetLocalPlayerTeleportData() and GetArrivingTeleportGui(). Existing Players remain: enumerate GetPlayers and subscribe to PlayerAdded. Content replacement cannot reset all callbacks, globals or module caches.

macOS currently uses compatibility mode and reports native switching and title support unavailable.

## Cache and diagnostics

The 1 GiB cache keys bytes by universe/place/version and pins active entries. Downloads/payloads are limited to 128 MiB; emulation to 250,000 instances. Logs report destination, version, mode and cache use.

`rbx status` reports native qualification; see `RENIUM/native-compatibility.md`.
