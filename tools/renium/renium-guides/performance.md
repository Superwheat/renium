# Performance: lag spikes, replication and resource limits

## Access and targeting

Built-in performance diagnostics are a trusted Renium workflow. They do not require switching protected-property modes or asking for per-property approval. This applies only to Renium's authenticated profiling operations, not arbitrary scripts, engine methods or property writes.

Select the intended place and Edit/server/client runtime. Reuse existing sessions; profiling is not a reason to start Play when the data needed is already captured. Keep raw captures tied to their runtime, build and timestamps. Report measured frame times and supporting scopes, not guessed causes or sums of overlapping work.

## Renium frame counters

```powershell
rbx perf snapshot --player 1
rbx perf start --player 1 --seconds 10
rbx perf read --player 1 --capture ID
rbx perf stop --player 1 --capture ID
rbx perf export --player 1 --capture ID --out reports/performance.json
```

Use the ID from `start`. Capture stops at its duration limit; `stop` ends it earlier. `read` returns percentiles and the five slowest frames. Request `--page N` only for raw samples. `export` saves every frame and refuses an unfinished or changed capture.

Target a play client with `--player`, the play server with `--server`, or Edit mode by omitting both. These commands never start Play. Reuse an existing runtime when the question requires runtime measurements.

The sampler records Heartbeat intervals, available CPU/physics timings, Edit/client render CPU/GPU timings, and memory/network/object counters at 5 Hz. Times are milliseconds; network rates are **kilobytes per second**. Memory categories require tracking. Unavailable is not zero.

Engine timings overlap and may retain their last reported value during a stall. Heartbeat gaps measure scheduler stalls, not viewport FPS. Do not add timings or infer a cause from one counter. Captures stop at 120 seconds or 60,000 frames. A new capture replaces the previous completed capture; export it first if needed. Nothing is sampled while inactive.

## MicroProfiler scopes and spikes

```powershell
rbx perf micro-start --player 1 --frames 256
# Reproduce the issue in that existing session.
rbx perf micro-stop --player 1 --out reports/spike.gprx
rbx perf analyze reports/spike.gprx
rbx perf analyze reports/spike.gprx --frame 84 --scope '*Script*|*Physics*'
rbx perf analyze reports/spike.gprx --thread '*Main*' --counter '**/Luau/**'
```

`micro-start` enables collection without opening the profiler UI. `micro-stop` freezes and saves the current rolling capture, then disables instrumentation. `micro --out FILE` snapshots without ending collection: Renium briefly pauses its own capture while copying, then resumes it, including if copying fails. It does not control a capture this runtime did not start. Controls affect the selected **Studio process**, not just one DataModel within it; don't overlap another profiling task. A capture holds at most 256 recent frames and can contain gaps from earlier collection.

Edit-mode diagnostics can run during a sync, without changing its target or acquiring its mutation lock. Studio must still yield to handle the capture; a busy engine can delay it. Capture the relevant interval, not just the idle frames after the operation.

Keep the generated `.gprx.json` sidecar with its dump. It records the runtime, PID, Studio/Renium versions, capture time and file hash; analysis checks that it matches. Raw dumps without a sidecar are also supported.

Analysis runs offline, outside Studio. It returns the five slowest frames in the newest continuous segment, with bounded per-thread scope and counter summaries. `--frame` selects one capture frame; results also give its absolute frame ID. Filters narrow the data; `--top 1–50` bounds output, and `--out FILE.json` saves the result. The original `.gprx` retains all captured data.

Scopes rank by `wallMs`: occupied time for one timer/thread, counting recursive overlap once. `inclusiveMs` sums calls and can exceed a frame through recursion. Different scopes and threads still overlap; neither is an additive frame breakdown or proof of the critical path. Inspect the relevant thread before naming a cause. Sleeping threads are separate; use `--scope Sleep` to inspect them. GPU frame bounds alone do not prove a GPU bottleneck. LibMP does not expose scope-instance labels or allocation/network event metadata; automatic `$Script` scopes cannot identify a script by name. User `debug.profilebegin` names remain visible.

The first offline analysis downloads Roblox's hash-pinned LibMP parser; later runs use the verified local cache. Capture files are data, never executable scripts. Do not start Play, request protected-property access, paste dump contents into the chat, or build ad-hoc Studio scans to use this workflow.

## Constrain resources

Profiles constrain connected Studio process trees, not FPS. The profile is global and reapplies when a connected process is replaced. It does not move windows or take input.

```powershell
rbx pf ls
rbx pf use iphone-11
rbx pf show
rbx pf off
```

Device names are approximate performance tiers, not hardware emulation.
`ls` shows tiers this host can enforce; profiles cannot grant extra resources.
First use calibrates if needed. Run `pf cal` after hardware or power-mode changes.

```powershell
rbx pf adv cpu=25 cores=2 headroom=1g prio=low
rbx pf adv cpu=40 cores=4 headroom=2g save=slow-test
rbx pf use slow-test
```

- `cpu`: aggregate CPU percent.
- `cores`: logical processors.
- `headroom`: memory allowed above current committed memory.
- `prio`: `normal`, `below`, or `low`.
- `mem=1g`: absolute memory cap; can crash Studio and requires `risk=crash`.

Windows applies and verifies Job Object limits. `pf off` removes limits; neutral job membership lasts until process exit.
macOS/Linux report profiles unavailable when controls cannot be enforced.
