# Network simulation

See [playtest](playtest.md) for the main workflow.

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
| `poor` | 100 ms | 30 ms | 0.5% | Highly variable, lossy connection |

`poor` keeps jitter at 30 ms: Studio applies jitter cumulatively to a 20 Hz replication stream, so 100 ms made a client fall a whole round behind.

These are test templates, not measured device profiles. Added round-trip delay is at least twice the listed delay. Studio supports 0–1000 ms delay/jitter per direction and at most 0.5% loss; a loss value of `0.5` means **0.5%**, not 50%. Presets do not simulate outages or bandwidth caps.

For custom asymmetric conditions, use `--in-delay`, `--out-delay`, `--in-jitter`, `--out-jitter`, `--in-loss` and `--out-loss`. Inbound means server→client; outbound means client→server. A preset fills all six values; explicit flags override it. Without a preset, omitted settings stay unchanged.

`set`, `reset` and `restore` return the selected runtime/PID, applied settings and changed fields; trust a successful result instead of rereading. `show` returns `settings`, `active` (any non-zero value) and the matching `preset` or `null`. A readback does not prove gameplay works under those conditions. Large latency jumps trigger congestion control—step changes gradually when measuring steady state.

Without `--player`, `net show/set/reset` targets the selected Studio's settings in Edit. During Play, use `--player` to avoid changing the server/defaults. These settings are process-local; Renium refuses a shared-process layout that cannot isolate the requested client. Client overrides are restored when its plugin unloads, without undoing later manual changes. Explicit `restore` is useful before ending a test. An abrupt process crash cannot run cleanup.

This uses Studio's plugin API, not injected game code or OS-level throttling, and does not touch `IncomingReplicationLag`, which stays additive if enabled. See [Roblox's network simulation reference](https://create.roblox.com/docs/studio/testing-modes#network-simulation).
