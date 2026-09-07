# Sandbox

A source-only Renium workflow plugin for disposable Studio testing. **Not compiled, installed or live-tested yet.**

It reserves a slot for one task/worktree, publishes a blank baseline, clears previous test data, opens that published place and pushes an isolated snapshot of the worktree into it. No Play session starts automatically. `refresh` replaces the snapshot after local edits. `release` closes the owned Studio, republishes the blank baseline, deletes test entries and verifies cleanup before releasing ownership.

## User setup

1. Create a throwaway group and private test experiences. Each slot must use a **different universe**, with its root place. Use this pool on one host only; no Team Create collaborators, public servers or other writers may use it.
2. Sign into Studio with an account allowed to edit those places. Enable Studio API access only in these disposable experiences if tests need DataStores.
3. Copy `pool.example.json` to `pool.json`. Set the real group/universe/place IDs. Declare every ordered DataStore name and scope the code can use, then set `orderedInventoryComplete` to true. An empty list is valid only if the tested code uses no ordered stores. Dynamic or unknown ordered store names are not supported safely.
4. Set `RENIUM_SANDBOX_API_KEY` to a key scoped only to these universes: universe read, place publishing, standard store/entry listing and entry read/delete, ordered entry read/write, and MemoryStore flush. Do not grant access to production universes. Keys stay in the environment, never in files or Studio.
5. When explicitly ready to build/test this plugin, build its separate Cargo project, then run `rbx plugin install PATH`. Renium never builds plugins on installation. `rbx plugin check PATH` only checks metadata.

The repository's Renium build does **not** build this plugin. Its SDK dependency points to `../../renium-plugin-sdk`; plugin authors outside this repository can use the self-contained starter from `rbx plugin new NAME`.

## Commands

- `acquire [--slot NAME]`: reserve a slot; no Studio changes yet.
- `prepare --slot NAME`: make bounded progress toward a clean, loaded Studio.
- `refresh --slot NAME`: snapshot the original worktree and replace the stopped sandbox contents.
- `run --slot NAME --args JSON`: run a targeted runtime/capture command. Starting Play requires `--reason`.
- `release --slot NAME`: close/reset/clean; resume until `released: true`.
- `status`: inspect local ownership without opening Studio.

Pass `--session TASK_ID` on the sandbox command, or set `RENIUM_SESSION_ID`. Global Renium options such as `--project` go **before** `sandbox`.

Snapshots have independent project configuration and sync state. Edits in the original worktree remain the source of truth; sandbox Studio changes are disposable. This first version uses explicit refresh, not another live watcher or a second sync engine. Existing Renium projection and push code handles game data, including non-script instances and packages. Unrelated workspace files and credentials are not copied.

Commands have a 20-second host budget. Preparation and large cleanups return resumable progress instead of holding one command open indefinitely. No automatic lease expiry: an interrupted run cannot make an unclean slot available. The same owner can resume with the same task ID and worktree. Corrupt journals and unknown launch ownership stay quarantined for manual recovery.

Preparation checks that the running daemon supports exclusive resource leases before changing cloud data. Each targeted command also requires an ownership acknowledgement; an older daemon cannot silently ignore the lease.

## Cleanup limits

Standard stores are discovered with pagination; the wildcard scope includes all scopes. Every declared ordered store/scope is paginated too. Deleted entries are checked, then collections are rescanned before MemoryStore flush completes. Cloud permission errors, throttling, non-advancing pages, remaining entries and surviving runtimes block release. Historical DataStore revisions remain subject to Roblox retention; this is not immediate physical erasure.

Private places and an exclusive local lease do not prevent a human or unrelated API key from writing to the universe. A dedicated, single-host pool with no outside writers is required. Native plugins are trusted programs, not security sandboxes. Game scripts can still call external services if the user allows them; do not put production service credentials in test code.

## Acceptance checks before first use

Build/lint this plugin only when authorized. With a disposable authenticated pool, verify on Windows and macOS:

- Full worktree projection and deletion of leftover content; scripts, references, Terrain and package fidelity.
- Two tasks racing for a slot, different slots concurrently, rapid repeated commands and core commands attempting to use another task's place.
- Manual/automatic Play transitions and owned-PID checks without focus/input changes.
- Interruptions before/after launch, snapshot, push, close, delete and final release; no reuse before cleanup.
- Multiple standard scopes, Unicode keys, empty pages with continuations, ordered stores, throttling, lost responses and asynchronous MemoryStore completion.
- Cleanup leaves no active entries, no owned Studio processes or snapshot folders; another acquisition starts clean.

API references: [storage](https://create.roblox.com/docs/cloud/reference/features/storage), [universe metadata](https://create.roblox.com/docs/cloud/reference/features/universes), [place publishing](https://create.roblox.com/docs/cloud/guides/usage-place-publishing).
