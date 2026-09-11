# Sandbox

Use a sandbox only when the task needs an isolated Studio runtime for the current worktree. Saved code and data still use offline checks; owning a slot is not a reason to start Play.

The user configures `pool.json` and an API key first (see README.md). Never borrow a production experience or widen key permissions yourself.

From the worktree, with one stable task ID (`--session`, `RENIUM_SESSION_ID` or `CODEX_THREAD_ID`):

```powershell
rbx sandbox --session TASK_ID acquire
rbx sandbox --session TASK_ID prepare --slot slot-1
rbx sandbox --session TASK_ID run --slot slot-1 --args '["status"]'
```

`prepare` publishes a blank place, clears old test data, opens Studio, waits for it to connect and pushes a snapshot of the worktree. Each call makes bounded progress; repeat it until `ready` is true. Do not send runtime commands before that.

Edit the original worktree. After a batch of edits, `refresh --slot slot-1` replaces the sandbox contents with the current projection; stop the sandbox Play session first. Studio edits made inside the sandbox do not flow back.

`run --args` takes a JSON array of Renium command tokens with canonical short names (`play`, `l`, `co`, `sc`, `clk`, `perf`, ...). The owned place is bound for you; do not pass `--place` or `--project`. Starting Play or `tst` needs `--reason` naming the runtime question. Read the normal guide for each command and inspect returned captures before claiming a visual result.

```powershell
rbx sandbox --session TASK_ID refresh --slot slot-1
rbx sandbox --session TASK_ID run --slot slot-1 --reason "Verify client input reaches the vehicle controller" --args '["play","-s"]'
rbx sandbox --session TASK_ID run --slot slot-1 --args '["play","-x"]'
rbx sandbox --session TASK_ID release --slot slot-1
```

When done, repeat `release` until `released` is true. It stops and closes only the owned Studio, republishes the blank place and deletes the run's DataStore, ordered store and MemoryStore data. An error leaves the slot reserved: read the message, fix the cause and run the same command again. Never clear leases, delete journals, pass `--confirm-closed` or take another task's slot; those are the user's decisions.
