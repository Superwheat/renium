# Studio snapshot gate

Branch: `astra/snapshot-gate`, based on `main` at `48c63cd4`.

## Findings and change

The Windows mutation boundary is now shared by all remote-thread starts, including attribute observation. Place/service snapshots and native capture reject unverified builds before engine discovery. DLL loading checks compatibility before its resident-module fast path, and attribute observation checks before duplicating handles. The process handle is acquired before the build decision, preventing PID reuse between that decision and mutation. Read-only process inspection and the existing Windows audio exemption remain available.

`write_editor_place_snapshot` already catches native errors and performs the plugin export, retaining the supplied base-place merge. No publishing or fallback serialization logic was replaced. Snapshot success/fallback logs now identify the target PID, resolved Studio build and title; gate warnings identify the PID.

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

## Cause of the apparent gate bypass

**The reported incident process was Studio 0.741, not 0.742.** Windows process metadata for PID `48516` identifies `C:\Users\Superwheat\AppData\Local\Roblox\Versions\version-76e1a02649ad4f35\RobloxStudioBeta.exe`, whose file/product version is `0, 741, 19, 7411056` and fixed minor version is `741`. Its creation time is `2026-10-10T08:57:39.1465690Z`, matching the incident transcript's `10:57:39` local start time. This is the same process, not a subsequently reused PID. Only WMI process metadata and the executable file were read for this identification; no Studio command, process-memory read, debugger attachment or mutation targeted DTE or Baseplate.

The two requests at `10:18:58Z` and `10:19:10Z` were the recorded dry-run and actual Publish As for place `127769757912519`. Build `741` passes the existing `build <= 741` gate. The newer installed executable, `version-9b554450a0fc4e65`, is `0, 742, 0, 7421053`; its existence and other processes' unlabelled rejection warnings do not establish that the incident process ran it. `git=fa8a07de434e` identifies the daemon build, not the Studio build.

The live daemon is PID `28920`, started at `08:51:22Z`. Read-only comparison of its loaded gate, process-open, build-reader and snapshot machine code against its installed executable found identical bytes. The installed code calls the gate from process-open, reads the selected process's executable version, and rejects build `742` and above. Its SHA-256 is `5AFBB1DDC9BBC8B60D62983F50F680364ED73271D2271B7342E63E0B9575B54A`.

There is no demonstrated 0.742 bypass to repair or reason to change the cutoff. The earlier boundary hardening remains, and the follow-up adds the resolved build to diagnostics and a live regression covering repeated 0.742 snapshots with an audio-exempt handle alive. The historical snapshot lines contain no PID, so they cannot independently prove which process each call selected; they also cannot support the original claim that native serialization ran on 0.742.

## Could the snapshot explain the hang?

Yes, plausibly; causation is unproven, including on the incident's actual 0.741 build. A compatibility cutoff is not proof that a snapshot cannot hang after Play stops. `ReniumRun` in `native/renium_studio_helper.cpp` submits a task to Studio's DataModel queue, changes native owner reference counts, invokes Studio's serializer using discovered addresses/layouts, allocates streams/buffers, and writes the snapshot from inside Studio. Place mode skips the separate context-builder/root-collector calls, matching the zero context/root timings in the logs. ABI/layout errors can corrupt memory even when a valid file is produced; successful serialization does not validate subsequent heap integrity. The hang itself was not investigated inside the protected Studio process.

Normally the task, references, buffers, event and request allocation are released. The helper DLL remains loaded: this path never unloads it. Snapshot execution itself installs no persistent hook or observer. On timeout, cancellation cannot interrupt an engine call already running; queued/running work can outlive the caller. Pre-existing hooks from other helper operations are separate.

For the owner inspecting PID 48516: preserve a full dump and module identities, including the loaded Studio executable and `renium-studio-helper-*.dll`, plus the daemon's executable path/build. Inspect every thread stack for helper frames, serializer work, heap operations, DataModel queue/lock waits and the UI thread's dependency. WinDbg's [`!analyze -hang`](https://learn.microsoft.com/en-us/windows-hardware/drivers/debugger/using-the--analyze-extension) can identify blocking threads; [`!heap -triage`](https://learn.microsoft.com/en-us/windows-hardware/drivers/debuggercmds/-heap) checks heap corruption. A resident helper module alone does not prove that its code is still running or caused the hang.

## Verification

- `cargo fmt`: passed.
- `cargo build`: passed (development build; no installation).
- `cargo test`: 760 unit tests and 1 integration test passed; 17 tests ignored by default. The audio exemption test now also checks repeated engine opens while the exempt handle remains alive.
- `cargo clippy --all-targets -- -D warnings -D clippy::cognitive_complexity -D clippy::too_many_arguments`: passed.
- The new ignored `live_unverified_studio_snapshot_gate` regression was explicitly run and passed against scratch PID `3260`, opened with `rbx so` from a copy of `E:\Documents\rblx\renium-cleanup-test\detach-test\detach.rbxl`. Its executable was `version-9b554450a0fc4e65\RobloxStudioBeta.exe`, version `0, 742, 0, 7421053`; the production build reader returned `742`.
- Both `write_live_place` (the native Publish As snapshot path) and `write_live_service` returned the exact native-disabled error on two consecutive attempts while an audio-exempt process handle remained open. No engine helper DLL was loaded, no output snapshot was created, and the source place stayed byte-for-byte unchanged. Studio remained responsive and only scratch PID `3260` was closed afterward.
- These live calls used the newly compiled test executable from this checkout. The installed executable and daemon remained unchanged. The complete daemon/plugin-export route was not exercised: the sandbox's Windows account cannot decrypt the shared daemon credential (`Private Renium credential protection failed: Key not valid for use in specified state`). No authentication workaround was used.
- Follow-up artifacts used the dedicated `.snapshot-gate-checks` directory in this writable checkout, with an empty project boundary isolating unit fixtures from the parent project's package links. Owned scratch files and test artifacts were removed after verification.

No installation, daemon restart, publishing or push was performed. DTE and Baseplate received no Studio operations.
