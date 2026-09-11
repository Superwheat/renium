# Sync pipeline timing

Use an owned Studio fixture and a fresh project directory. Keep the input RBXL
unchanged. Separate Studio launch from command time; label warm runs explicitly.

Run the installed CLI and its owned daemon at `--log-level trace`. Enabling trace
on the CLI does not change an already-running daemon's log level. Save stderr
separately from JSON command output. Trace records contain request/context IDs,
thread IDs, start timestamps and wall durations; normal logging omits them.

```powershell
node tools/renium/tests/analyze-sync-spans.mjs --self-test
node tools/renium/tests/analyze-sync-spans.mjs daemon.stderr.log audit/push --command-log push.stderr.log
```

The reader writes `audit/push.trace.json`, `audit/push.ranked.json`, and a compact
`audit/push.report.md` when a command log is supplied. The command
filter selects the matching daemon operation, including reviewed pushes, and its
correlated workers. `--operation NAME` instead selects the latest daemon operation
with that exact name. Do not rank an entire background log as one sync command.

`selfOrUnattributedMs` subtracts the union of nested intervals on that thread. It
is **not CPU time**. Worker rows overlap, and a bridge wait includes remote work.
The `profiles` section retains the matching Studio phase profiles. Use them to
split remote work; do not add their durations to the bridge wait.
Check `uncorrelatedSpans` (which can include background activity) and unexplained
residuals before claiming full coverage.
`accounting` partitions the daemon operation into non-overlapping main-thread
intervals; its rows sum to the operation duration. `commandAccounting` also covers
argument parsing, startup, command dispatch and output within the CLI process.
OS process launch/exit is outside `main`; measure that with the harness stopwatch,
and report its difference separately. A daemon wait contains daemon execution;
do not add the two views. Use request/context IDs to select the nested operation.

`combinedCommandAccounting` replaces those waits with their matching daemon work
and native phases, producing one additive command-wide ranking. It rejects a
missing/mismatched clock window instead of inserting another request's timings.
The daemon can start while the CLI is finishing its send. Correlation checks the
whole RPC interval, substitutes only the overlap with response waiting, and keeps
the concurrent prefix separate rather than double-counting or rejecting it.
Native profiles carry their individual host-call interval, so multiple calls in
one request cannot exchange profiles or disappear into an aggregated same-name
row. Older logs without intervals are accepted only for an unambiguous single
call. Unmatched calls remain explicitly unaccounted, not guessed by duration.
Timing records carry the preceding diagnostic write's completed interval, so
trace encoding/output costs can be measured without recursively profiling the
logger. Keep this opt-in output in logs, not routine command JSON.

An uncovered interval is explicitly `UNINSTRUMENTED`, with exact boundaries in
`segments`. `coverageWarnings` identifies missing instrumentation and correlation.
A balanced total does not prove detailed coverage: an opaque parent label is not
a diagnosis. Never relabel a residual as CPU time, engine time, or miscellaneous
overhead merely to claim 100% coverage. The trace does not yet establish a complete
cross-thread critical path; join timers include worker execution and waiting.

Large numeric settings columns are partitioned within a property group; a single
dominant CFrame/Vector column must not serialize JSON construction on one worker.
The serial and parallel decoders retain identical index ordering and validation.
Place construction preallocates its DOM and property vectors and reuses assigned
referents instead of generating and discarding another identity for each builder.

Native call profiles need a separate owned fixture and a pinned executable. Time
the complete native ABI, including every stack argument and hidden return buffer;
an incomplete diagnostic wrapper can corrupt the target. Subtract nested calls
before ranking: recursive ancestor notification totals are not additive. Record
depth overflow and restoration checks, and distinguish callback execution (which
also calls engine APIs) from the signal dispatcher and other native subscribers.
Use ordinary uninstrumented commands for speed comparisons.

Windows uses the native service reader for eligible imports by default, with
the existing transport retained for unsupported Studio reader contracts. The
internal `RENIUM_NATIVE_SERVICE_IMPORT=0` control selects the previous transport
for A/B runs; set it on the owned daemon. Other platforms retain their transport.
For the native service-reader path, `native.import.batch` identifies each
batch's services, bytes and reader duration. One `native.import.reader` profile
splits queueing, total read and identity collection. Batches run separate
DataModel write jobs with one shared factory/identity scope; per-batch read times
nest inside the reader total. Whole subtrees fit into the instance budget;
oversized trees split using exact ancestor identities. The internal
`RENIUM_NATIVE_IMPORT_BATCH_INSTANCES=0` control selects one payload for A/B runs;
the default target is 512 instances. Set this on the owned daemon, not merely on
a CLI connected to an existing daemon. Ancestor anchors can exceed the budget.
The producer returns the DataModel lock after every batch and inserts an explicit
gap after 16 ms of accumulated reader work. This bounds continuous import work
without sleeping after every small batch. `RENIUM_NATIVE_IMPORT_BATCH_PAUSE_MS`
controls that gap (0–16 ms, default 1); zero removes it for A/B measurement. Report the complete
reader span as well as its work timers: gaps and scheduling add wall time.
Windows uses an owned high-resolution waitable timer for that gap, falling back
to `Sleep` when unavailable. It does not change the system timer resolution.
Compare actual pacing time and frame progress: removing the gap can starve frame
jobs without improving the complete push, even though the pacing row disappears.
Native receipt version 6 adds an exclusive QPC timeline: request setup, dispatch/
queue, target validation/patch setup, engine read, verification/restoration,
identity capture/release, response write, worker completion, actual pacing sleep,
and producer bookkeeping. The native phases are contiguous and must sum exactly
in clock ticks. Host invocation/return is measured separately from that timeline.
The analyzer rejects negative or non-balancing native accounting. Old logs are
flagged rather than filled with guessed pacing or transport durations.

The engine-read timer includes synchronous Lua/native callbacks. Native factory
and constructor timers nest within that read. Lua tracking profiles span the
entire import observation window, including deferred callbacks between native
batches; they cannot be subtracted wholesale from the engine-read timer. Initial
baseline/listener timers nest within tracker-added callbacks, and export-property
timers nest within the global-property callback timer. Do not add them twice or
describe their subtracted remainder as pure deserialization cost.
Compare complete pushes into fresh blank fixtures and saved-file fidelity—not
the sum of loader times alone. Both modes use one transaction and final commit.
`Studio native import tracking` measures observer callbacks during the native
job, including initial property/attribute baselines, listener setup and the
`nativeStructureCallbacksMs`/`nativeContentCallbacksMs` export observers. These
costs overlap the native operation; they are not additional full-push phases. Export
and tracking share engine subscriptions, but retain separate measurements.

Studio commit profiles include per-service `attachment:*` measurements:

- `expectMs` precedes parenting; `parentMs` covers the synchronous assignment.
- `trackerAddedCallbacksMs` includes tracker work during that assignment;
  `trackerPreExpectationMs`, `baselineMs`, `attributeBaselineMs` and
  `listenerSetupMs` are nested costs, not additional time.
- `nativeStructureCallbacksMs` and `nativeContentCallbacksMs` measure Renium's
  separate export-generation observers. Counts help detect deferred callbacks
  outside the assignment scope; they are not necessarily equal to root counts.

The profiling scope is restored even if parenting fails. It does not defer,
disconnect or suppress observers. Unaccounted parenting time can include engine
work, dispatch, other plugins and nested callbacks; subtraction does not prove
an engine-only floor. Per-instance instrumentation itself adds overhead.

Optimize the largest measured cost until it falls below the second-largest, then
remeasure and re-rank. Confirm gains in repeated ordinary runs with trace disabled,
and verify saved RBXL comparisons before accepting an optimization.

Full replacement stages the captured source files concurrently with decoding
service settings. Both read the same immutable snapshot; staging does not read
the user's files again. Private staging removes old targets and creates parent
directories before parallel independent writes; paths containing symlinks retain
the ordered implementation. Public file publication keeps its existing order.
Batch ancestor/reference discovery also runs in parallel,
with DOM changes applied after the workers join. Inspect worker spans when ranking
these joins; adding staging, decoding, or batch-worker durations would overcount.

Native import completion can carry the final compact identity receipt when the
plugin advertises `nativeInlineReceipt`. Older plugins retain the append request.
Successful push acknowledgement also releases its transient tracking guard in
the same request, so the release cost belongs to that combined stage.

To rebuild the native-setter A/B fixture from current source, run
`node tools/plugin_ws_bridge/tests/prepare-no-native-diagnostic.mjs`, then build
`tools/plugin_ws_bridge/diagnostic-no-native.project.json` with Rojo. The generated
module stays in `audit/release-readiness`. This diagnostic intentionally skips
explicit native setters and is unsuitable for fidelity tests or ordinary sync.

`node tools/renium/tests/live-settle-latency.mjs CLI FIXTURE_ROOT --allow-disposable-edit`
checks 100 alternating Studio/file edits with simultaneous status requests in an
unpublished `ReniumPropertyPackageTest.rbxl` (an optional final argument selects
another disposable place). It verifies each value and instance identity, requires
`daemon.settled`, and rejects waits over five seconds. A zero final pending count
alone does not prove that a wait completed: a timed-out wait refreshes status too.
