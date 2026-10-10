# Studio snapshot gate

Branch: `astra/snapshot-gate`, based on `main` at `48c63cd4`.

## Findings and change

The Windows mutation boundary is now shared by all remote-thread starts, including attribute observation. Place/service snapshots and native capture reject unverified builds before engine discovery. DLL loading checks compatibility before its resident-module fast path, and attribute observation checks before duplicating handles. The process handle is acquired before the build decision, preventing PID reuse between that decision and mutation. Read-only process inspection and the existing Windows audio exemption remain available.

`write_editor_place_snapshot` already catches native errors and performs the plugin export, retaining the supplied base-place merge. No publishing or fallback serialization logic was replaced. Snapshot success/fallback logs now identify the target PID and title; gate warnings identify the PID.

| Audited Windows surface | Result |
| --- | --- |
| `WriteProcessMemory`, `VirtualAllocEx` | One call each, gated in `ProcessMemory`. |
| `CreateRemoteThread` | One shared gated call; both ordinary helpers and attribute observation use it. |
| `LoadLibraryW`, resident DLL lookup | Gated before lookup/loading; helper extraction also rejects first. |
| Package/actions, properties/functions, history/Terrain, import, capture, snapshots | Invoke through the gated allocation/write/thread methods. Loader-discovery modules only inspect code. |
| `DuplicateHandle` into Studio | Gated; cleanup closes only previously acquired handles. |
| `VirtualFreeEx` | Cleanup of allocations obtained through the gated allocator. |
| `VirtualProtectEx`, `QueueUserAPC`, `SetThreadContext` | No calls in `serializer/*.rs`. In-process helper patches require a gated helper invocation. |
| Audio | Sole Windows exemption; local `LoadLibraryExW` only inspects its export without resolving DLL references. |

Scope remains the existing Windows cutoff. The macOS files use a separate resident-helper socket protocol and were audited but not changed or tested here.

**The incident's exact cause is not established.** At the supplied base revision, `write_live_snapshot` already allocated, wrote and ran through gated primitives. Attribute observation's direct thread call followed gated allocation/write; a cached DLL alone could not bypass the snapshot's later allocation gate. The two successful log entries at 10:18:58Z and 10:19:10Z contain neither PID nor executable identity. Both 0.741 and 0.742 Studio executables exist on disk; the 0.742 file's fixed minor version is correctly 742. These findings justify hardening the boundaries, but do not prove how those historical requests passed the gate. The running target and daemon image need correlation before calling this incident resolved.

## Could the snapshot explain the hang?

Yes, plausibly; causation is unproven. `ReniumRun` in `native/renium_studio_helper.cpp` submits a task to Studio's DataModel queue, changes native owner reference counts, invokes Studio's serializer using discovered addresses/layouts, allocates streams/buffers, and writes the snapshot from inside Studio. Place mode skips the separate context-builder/root-collector calls, matching the zero context/root timings in the logs. ABI/layout errors can corrupt memory even when a valid file is produced; successful serialization does not validate subsequent heap integrity.

Normally the task, references, buffers, event and request allocation are released. The helper DLL remains loaded: this path never unloads it. Snapshot execution itself installs no persistent hook or observer. On timeout, cancellation cannot interrupt an engine call already running; queued/running work can outlive the caller. Pre-existing hooks from other helper operations are separate.

For the owner inspecting PID 48516: preserve a full dump and module identities, including the loaded Studio executable and `renium-studio-helper-*.dll`, plus the daemon's executable path/build. Inspect every thread stack for helper frames, serializer work, heap operations, DataModel queue/lock waits and the UI thread's dependency. WinDbg's [`!analyze -hang`](https://learn.microsoft.com/en-us/windows-hardware/drivers/debugger/using-the--analyze-extension) can identify blocking threads; [`!heap -triage`](https://learn.microsoft.com/en-us/windows-hardware/drivers/debuggercmds/-heap) checks heap corruption. A resident helper module alone does not prove that its code is still running or caused the hang.

## Verification

- `cargo fmt`: passed.
- `cargo test`: 760 unit tests and 1 integration test passed; 16 existing tests ignored. Five new tests cover the build boundary, mutation/thread refusal, cold/resident loaders, snapshot/capture refusal for unknown builds, and the audio exemption.
- `cargo clippy --all-targets -- -D warnings -D clippy::cognitive_complexity -D clippy::too_many_arguments`: passed.
- Test fixtures ran under `E:\Documents\rblx\renium-cleanup-test\snapshot-gate-unit-tests`; owned artifacts were cleaned. An initial run under this checkout inherited its unrelated package-link configuration and failed two tests; the isolated full rerun passed.

No Studio window was opened, inspected or closed. No live plugin-export/publish test was run. The installed executable and running daemon were left unchanged; no install scripts or push were performed.
