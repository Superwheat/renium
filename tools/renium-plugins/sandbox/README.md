# Sandbox

A separately built Renium plugin that gives one task exclusive use of a disposable, published Studio place. **Authenticated cloud/Studio acceptance testing remains incomplete.** Builds and offline tests pass on Windows and ARM64 macOS; the Mac release binary has also passed isolated host installation and command checks.

A task acquires a slot, and `prepare` publishes an empty place as the baseline, clears previous test data, opens that place in its own Studio and pushes an isolated snapshot of the worktree into it: scripts, instance properties, attributes, identities, references, Terrain and packages. No Play session starts automatically. `refresh` replaces the contents after local edits. `release` closes the owned Studio, republishes the blank baseline, deletes the run's test data and verifies the cleanup before returning the slot to the pool.

## User setup

1. Create private throwaway experiences, owned by your account or by a group. Each slot must use a **different universe**, with its root place. Use this pool on one host only; no Team Create collaborators, public servers or other writers may use it.
2. Sign into Studio with an account allowed to edit those places. Enable Studio API access only in these disposable experiences if tests need DataStores.
3. Copy `pool.example.json` to `pool.json` in this folder. Set `userId` (or `groupId` instead) to the account that owns the experiences, and the real universe/place IDs. Declare every ordered DataStore name and scope the code can use, then set `orderedInventoryComplete` to true. An empty list is valid only if the tested code uses no ordered stores; unknown ordered store names cannot be discovered through Open Cloud.
4. Create an Open Cloud key restricted to these universes, with `universe-places:write`, `universe-datastores.control:list`, `universe-datastores.objects:list/read/delete`, `universe.ordered-data-store.scope.entry:read/write` and `memory-store:flush`. Never grant access to production universes. Store it with `rbx oc key add sandbox` (a hidden prompt) and keep `"keyName": "sandbox"`, or set `keyEnv` to the name of an environment variable holding it instead. Every cloud call goes through Renium, so the key never reaches this plugin, its files or Studio.
5. Build with `cargo build --release --locked`, then run `rbx plugin install PATH`. Renium never builds plugins on installation. `rbx plugin check PATH` only checks metadata.

The repository's Renium build does **not** build this plugin. Its SDK dependency points to `../../renium-plugin-sdk`; plugin authors outside this repository can use the self-contained starter from `rbx plugin new NAME`. The sandbox needs a Renium build newer than 0.3.10: older daemons refuse every command sent under the slot's place lease.

## Commands

- `acquire [--slot NAME]`: reserve a free slot for this task and worktree; nothing opens yet.
- `prepare --slot NAME`: make bounded progress toward a clean, loaded Studio; repeat until `ready` is true.
- `refresh --slot NAME`: snapshot the worktree again and replace the stopped sandbox contents.
- `run --slot NAME --args JSON`: run a runtime, input or capture command against the owned place. Starting Play or `tst` requires `--reason`.
- `release --slot NAME`: close, reset and clean; repeat until `released` is true.
- `status`: show slot ownership and progress without touching Studio.

Pass `--session TASK_ID` after `sandbox`, or set `RENIUM_SESSION_ID`. Global Renium options such as `--project` go **before** `sandbox`.

`run` accepts canonical short command names and their aliases: status, cs, net, perf, pf, access, play, tst, co, l, lc, wait, ss, sg, sr, ui, inp, clk, ky, pr, go, ty, dev, sc, rs, re and rf. Sync, publishing, package, daemon and target-override options are rejected because the snapshot, not the sandbox Studio, is the source of truth. Performance captures keep the owned target, for example `run --slot NAME --args '["perf","micro-start","--frames","256"]'`.

Snapshots have independent project configuration and sync state. Edits in the original worktree remain the source of truth; sandbox Studio changes are disposable. Only the latest pushed snapshot is kept on disk. This version uses explicit refresh, not a second live watcher.

## Timing and interruptions

`prepare` and `refresh` may run up to 15 minutes, `release` up to 10 and `run` up to 5. Pushes, Studio start-up and cleanup are waited for inside those budgets, so an agent does not need to poll. Before each long step (snapshot, push, blank publish) the command checks that the remaining budget covers that step's own cap; otherwise it stops at the last journaled phase and returns `next`, so the host never interrupts a push. Repeating the command resumes from the journal. Only the revision Studio currently holds is kept on disk; superseded and interrupted revisions are removed before the next snapshot and after each successful push.

There is no automatic lease expiry: an interrupted run cannot make an unclean slot available. The same task ID from the same worktree resumes where it stopped. Corrupt journals and unknown launch ownership stay quarantined.

Preparation checks that the running daemon supports exclusive resource leases before changing cloud data. Each targeted command also requires an ownership acknowledgement; an older daemon cannot silently ignore the lease.

## Recovery

`rbx sandbox status` shows each slot's owner task, worktree and phase. If the owning agent is gone, finish its cleanup yourself: from that worktree, run `rbx sandbox --session OWNER_ID release --slot NAME` with the owner ID from `status`, repeating until `released` is true.

If the sandbox Studio could not open its place (for example, the signed-in account cannot edit it), `prepare` stops with Studio's reason and `release` closes that Studio, identified by its process ID and start time. If a launch receipt was lost, `release` cannot prove which Studio window belongs to the slot and refuses to guess. Close the test place yourself, then add `--confirm-closed` to `release`. Leases live under `plugins/leases` in Renium's local app data; editing or deleting them by hand bypasses every safety check and should be a last resort.

## Cleanup limits

Standard stores are discovered with pagination; the wildcard scope includes all scopes. Every declared ordered store/scope is paginated too. Deleted entries are checked, then collections are rescanned before MemoryStore flush completes. Cloud permission errors, throttling, non-advancing pages, remaining entries and surviving runtimes block release. Historical DataStore revisions remain subject to Roblox retention; this is not immediate physical erasure.

Cleanup erases all data in the slot's universe. That is safe only because the universe is dedicated to this pool, which is why validation requires the private root place of an experience owned by the configured account and a single host with no other writers. A slot that was acquired but never prepared is released without touching Studio or the cloud. An exclusive local lease does not prevent a human or unrelated API key from writing to the universe. Native plugins are trusted programs, not security sandboxes. Game scripts can still call external services if the user allows them; do not put production service credentials in test code.

## Acceptance checks before first use

Run `cargo test --locked` and `cargo clippy --all-targets --locked -- -D warnings` for offline checks. Cleanup tests intercept HTTP requests without credentials; they cover paginated scopes, Unicode paths, throttling, resumed journals, deletion verification and asynchronous MemoryStore completion. These checks do not substitute for a disposable authenticated pool. Verify on Windows and macOS:

- Full worktree projection and deletion of leftover content; scripts, references, Terrain and package fidelity, including `src/client` and `src/server` tree mappings.
- Two tasks racing for a slot, different slots concurrently, rapid repeated commands and core commands attempting to use another task's place.
- Manual/automatic Play transitions and owned-PID checks without focus/input changes.
- Interruptions before/after launch, snapshot, push, close, delete and final release; no reuse before cleanup; recovery with the owner's task ID and with `--confirm-closed`.
- Multiple standard scopes, Unicode keys, empty pages with continuations, ordered stores, throttling, lost responses and asynchronous MemoryStore completion.
- Cleanup leaves no active entries, no owned Studio processes or snapshot folders; another acquisition starts clean.

API references: [storage](https://create.roblox.com/docs/cloud/reference/features/storage), [universe metadata](https://create.roblox.com/docs/cloud/reference/features/universes), [place publishing](https://create.roblox.com/docs/cloud/guides/usage-place-publishing).
