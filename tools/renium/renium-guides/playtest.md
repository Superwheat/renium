# Play, live Luau, and consoles

## Decide before starting

Use saved files, offline queries, and focused tests first. A rename, constant change, source edit, or sync check does not require Play when its result can be verified programmatically.

Use `rbx ck FILE...` for offline Luau syntax checks. Don't wrap source in `loadstring`, run it, or require a module just to see whether it parses. Don't enable `LoadStringEnabled` or change place settings for validation. Successful syntax checks do not prove types or behavior; use the project's focused checks for those.

Start Play for a concrete runtime question that those checks cannot answer: replication between clients, input handling, physics, initialization, or an observed runtime failure. Choose the assertion or visual evidence before starting. Batch related checks and reuse a suitable running session; don't restart it for each edit.

## Sessions

```powershell
rbx status
rbx play -s                         # ordinary Play
rbx play -s --players 2             # local server and two clients
rbx cs
rbx play -x
```

Use ordinary Play for one-client checks. `--players 1` explicitly launches a separate server and client; `mode: "play"` means ordinary Play. Stop only a session you started or were asked to stop. File edits during Play can wait for Edit mode; that alone is not a sync failure.

## Query the right runtime

```powershell
rbx l "return game.PlaceId"
rbx lc "return game.Players.LocalPlayer.Name" 1
rbx co --server -n 20
rbx co --player 1 -n 20
```

`l` targets Edit when stopped and the server during Play. `lc CODE PLAYER` targets a client by name or index. Edit has no `LocalPlayer` or `PlayerGui`.

Return values instead of printing. Luau errors and timeouts exit nonzero; captured `print`/`warn` text returns to the caller without entering Studio Output. `co` reads game/Studio messages.

Keep live queries bounded. Don't run nested descendant scans over saved data. Don't hold a command open while issuing another: register an observer, return, act, then read its result. Runners are removed on return; persistent test fixtures need a temporary source script.

For larger programs, pipe code to `rbx l -` or `rbx lc - PLAYER`. In PowerShell, single-quote code containing double quotes; double an embedded apostrophe. Backslash does not escape PowerShell quotes.

## Network simulation

Use `net` for latency, jitter and packet loss. It does not start or restart Play. Configure an existing client by the same name/index used by `lc`:

```powershell
rbx net presets                              # Offline template list
rbx net show --player 1
rbx net set --player 1 --preset normal
rbx net set --player 1 --preset mid           # Change that client while Play stays running
rbx net set --player 2 --preset poor          # A different connection for client 2
rbx net set --player 1 --preset high --out-loss 0.2
rbx net restore --player 1                   # Restore values from before the first override
rbx net reset --player 2                     # Zero all six simulation values
```

| Preset | Minimum delay each way | Jitter each way | Packet loss each way | Use |
|---|---:|---:|---:|---|
| `normal` | 15 ms | 2 ms | 0% | Typical low-latency test |
| `mid` | 50 ms | 10 ms | 0.05% | Moderate latency |
| `high` | 100 ms | 15 ms | 0.1% | High latency |
| `poor` | 100 ms | 100 ms | 0.5% | Highly variable, lossy connection |

These are test templates, not measured device/network profiles. Minimum added round-trip delay is twice the listed delay; actual ping also includes jitter, real network latency and processing. Studio supports 0–100 ms delay/jitter per direction and at most 0.5% loss. `poor` uses those limits; it does not simulate arbitrary outages or bandwidth caps. A loss value of `0.5` means **0.5%**, not 50%.

For custom asymmetric conditions, use `--in-delay`, `--out-delay`, `--in-jitter`, `--out-jitter`, `--in-loss` and `--out-loss`. Inbound means server→client; outbound means client→server. A preset fills all six values; explicit flags override it. Without a preset, omitted settings stay unchanged.

Commands return the selected runtime/PID, applied settings and changed fields. Trust a successful result instead of rereading after every change. Use a live test only when measuring networking behavior; a configuration readback alone does not prove gameplay works under those conditions. Large latency jumps can trigger congestion control—step changes gradually when measuring steady-state behavior.

Without `--player`, `net show/set/reset` targets the selected Studio's settings in Edit. During Play, use `--player` to avoid changing the server/defaults. These settings are process-local; Renium refuses a shared-process layout that cannot isolate the requested client. Client overrides are restored when its plugin unloads, without undoing later manual changes. Explicit `restore` is useful before ending a test. An abrupt process crash cannot run cleanup.

This uses Studio's plugin API, not injected game code or global OS network throttling. It does not edit place files or change `IncomingReplicationLag`; that older setting remains additive if already enabled. Updated CLI and Studio plugin are required. See [Roblox's network simulation reference](https://create.roblox.com/docs/studio/testing-modes#network-simulation).
