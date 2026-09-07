# Sandbox workflow

Use this only when the task needs an isolated Studio runtime. For source, constants, syntax and saved data, use offline checks. Acquiring a slot does not justify starting Play.

The user must first configure `pool.json` and a dedicated API key; see README.md. Never borrow a production experience or broaden key permissions yourself.

From the associated project/worktree:

```powershell
rbx sandbox --session TASK_ID acquire
rbx sandbox --session TASK_ID prepare --slot slot-1
rbx sandbox --session TASK_ID run --slot slot-1 --args '["status"]'
```

Reuse one stable task ID. `RENIUM_SESSION_ID` or the host's `CODEX_THREAD_ID` can supply it. Continue `prepare` as its result directs; while Studio is connecting, don't busy-poll. Do not send runtime commands until `ready: true`.

Edit the original worktree. After a batch of changes, run `refresh --slot slot-1` before the next runtime test. This replaces the disposable Studio's contents with the current projection. Stop your sandbox Play session first. It does not change the original project's sync binding; Studio edits in the disposable copy do not flow back automatically.

```powershell
rbx sandbox --session TASK_ID refresh --slot slot-1
rbx sandbox --session TASK_ID run --slot slot-1 --reason "Verify client input reaches the vehicle controller" --args '["play","-s"]'
rbx sandbox --session TASK_ID run --slot slot-1 --args '["play","-x"]'
rbx sandbox --session TASK_ID release --slot slot-1
```

`run --args` takes a JSON argument array, not a shell command. It binds the owned target for you. Read the normal Renium guide for each test/capture command. Inspect returned screenshots/recording frames before claiming a visual result. Never enable `LoadStringEnabled` or run `loadstring` for validation.

When done, continue `release` until `released: true`. It stops/closes only the owned sandbox, resets its blank published place and removes test data. A failed command or timeout leaves it reserved. Read the error; don't clear leases, delete journals, take another task's slot or turn an uncertain result into success. A lost launch receipt may require manual recovery before cleanup can safely continue.

This source-only plugin has not been built or live-tested. Do not describe its runtime behavior as verified until the separate acceptance checks in README.md pass.
