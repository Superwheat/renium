# Performance: lag spikes, replication and resource limits

## Access and targeting

Performance diagnostics need no protected-property mode or per-property approval.

Select the intended place and Edit/server/client runtime. Reuse existing sessions; profiling is not a reason to start Play when the data needed is already captured. Keep raw captures tied to their runtime, build and timestamps. Report measured frame times and supporting scopes, not guessed causes or sums of overlapping work. Record the render quality with the numbers (`rbx gfx`, `--player N` for a client): frame times at level 21 and at automatic are different measurements. Change it only when the task names a level, and `rbx gfx restore` afterwards.

## Renium frame counters

```powershell
rbx perf snapshot --player 1
rbx perf start --player 1 --seconds 10
rbx perf read --player 1 --capture ID
rbx perf stop --player 1 --capture ID
rbx perf export --player 1 --capture ID --out reports/performance.json
```

Keep the `captureId` from `start`; `read`, `stop` and `export` require it. Capture stops at its duration limit; `stop` ends it earlier. `read` returns percentiles and the five slowest frames. Request `--page N` only for raw samples. `export` saves every frame and refuses an unfinished or changed capture.

Target a play client with `--player`, the play server with `--server`, or Edit mode by omitting both. These commands never start Play. Reuse an existing runtime when the question requires runtime measurements.

The sampler records Heartbeat intervals, available CPU/physics timings, Edit/client render CPU/GPU timings, and memory/network/object counters at 5 Hz. Times are milliseconds; network rates are **kilobytes per second**. Memory categories require tracking. Unavailable is not zero.

Engine timings overlap and may retain their last reported value during a stall. Heartbeat gaps measure scheduler stalls, not viewport FPS. Do not add timings or infer a cause from one counter. Captures stop at 120 seconds or 60,000 frames. A new capture replaces the previous completed capture; export it first if needed. Nothing is sampled while inactive.

## Luau heap snapshots

```powershell
rbx perf heap --server --out reports/server-heap.json
rbx perf heap --player 1 --out reports/client-heap.json
```

`heap` asks the play server or a play client for the report behind Studio's Luau Heap tool and saves it as JSON. The result summarises memory categories, object tags, the scripts holding the most, and `unparentedInstanceReferences`: Instances no longer in the DataModel that Luau still references, each with the paths (script, table, key) keeping it alive. That is how a departed Player, a destroyed car or a finished round stays in memory. Snapshot, drive the suspected leak (kick players, run rounds), snapshot again and compare the counts; the file holds the full per-script graph (`Report.Graph`) and every root (`Refs.Roots`). Edit mode has no heap report; the snapshot needs a running Play session.

## MicroProfiler and resource limits

See [MicroProfiler and resource limits](performance-profiler.md).
