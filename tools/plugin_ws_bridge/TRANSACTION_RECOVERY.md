# Editor transaction recovery

`abandoned` is terminal for the active transaction lock. It does **not** mean
rollback succeeded, that files contain outside Studio edits, or that package
contents can be marked Unmodified. `committed` and `rolledBack` retain their
existing meanings. A retained failure reports `rollbackFailed` immediately.

Two failed **idle rollback attempts** transfer the session to a recovery record.
An idle attempt starts at or after 120 seconds without transaction activity.
Its failure timestamp is recorded when rollback returns; the next idle attempt
requires another 120 seconds. Timer scheduling and rollback duration can add
time. Timer and begin-time pruning use the same inclusive deadline. Refused
begins and status reads of failed transactions do not refresh this deadline.
Direct requests, cancellation and shutdown cleanup report failures immediately
but do not consume the two idle attempts unless cleanup itself starts after the
idle deadline. A deferred cancellation attempt at operation completion is not
an idle attempt. Healthy status reads continue to renew healthy transactions.

With instantaneous failures, prompt timers and no further mutations:

| Starting path at t=0 | Immediate state | t=120 | t=240 | Rollback attempts through transfer |
| --- | --- | --- | --- | --- |
| Healthy transaction left idle | open | rollbackFailed, idle attempt 1 | abandoned, idle attempt 2 | 2 |
| Commit fails and rollback fails | rollbackFailed | idle attempt 1 fails | abandoned, idle attempt 2 | 3 |
| Explicit rollback fails | rollbackFailed | idle attempt 1 fails | abandoned, idle attempt 2 | 3 |
| Lease cancellation rollback fails | rollbackFailed | idle attempt 1 fails | abandoned, idle attempt 2 | 3 |
| Immediate shutdown cleanup fails | rollbackFailed | idle attempt 1 fails | abandoned, idle attempt 2 | 3 |

These timings require continuing valid ownership and exclude native work still
inside an operation fence. For 30-second yielding rollback attempts, idle starts
at 120/270 and transfers at 300; the other four paths start at 0/150/300 and
transfer at 330. The generation-change path below does not consume idle retries.

Native service insertion negotiates `nativeHandoff: 1`. Prepare arms observation
and both operation fences; the daemon must then receive an explicit `invoke`
permission before calling native code. A prepare with no invoke permission expires at
120 seconds, independently of status polls: no native invocation was authorized,
so both fences can be released and ordinary rollback attempted. Expired or
duplicate invocation permissions are refused. Old peers must update before
preparing this protocol.

A legitimate daemon suspended or delayed for 120 seconds between prepare and
invoke can lose its preparation. Polling at t=100 does not extend the window.
The invoke handler refuses at t=120 even if its expiry callback has not run;
the daemon requires affirmative permission before a native write. This is safe
refusal/retry, not permission to write after rollback. The Rust path performs
the ready-response check and invoke RPC before payload/native helper preparation.
Actual slow-machine and Studio scheduling latency is not measured here.

Once invocation is authorized, its outcome is conservatively unknown until a
validated terminal receipt and completed observation arrive. This includes a
daemon killed between authorization and the native call. Neither timeout,
disconnect nor shutdown authorizes rollback over a possibly running write.
`getEditorTransactionState` reports `nativeOutcomeUnknown`, the import identity
and the reason; begin refusal and daemon errors explain that restarting Studio
clears the stuck operation. This restart is not a content-rollback guarantee.
A replacement daemon under the same Studio ownership generation can deliver a
verified receipt using a new request lease. Successful completion is cached against that validated completing
lease, so its duplicate recovers a lost reply; unrelated leases are refused.
Receipt or observation failures keep both fences. A validated terminal native failure releases them before reporting
failure. Unknown native work has no automatic rollback deadline and consumes no
idle rollback attempt. The separate native root setter keeps its existing
five-second watchdog based on its two-second call budget, through the owned
exclusive gate. If its generation is revoked, that watchdog cannot write under
a new owner; its retained fence also reports unknown outcome/restart guidance.

Timer cleanup enters the production exclusive queue with the Studio generation
captured by the transaction. It binds that generation only while holding the
gate and restores the previous binding on success or error. Prune, cancellation,
native prepare deadlines and recovery retries use the same ownership binding.
Lost ownership invalidates that scheduled scene-work attempt. A disconnected
request lease is not a substitute for Studio ownership.
Checks precede writes and terminal publication, including source-update callbacks
and operations after engine yields. Session operation fences always retire after
cancellation cleanup, even if publication fails. Completed commit/rollback results
awaiting publication are retained separately: timer/prune, shutdown/cancellation,
and direct commit/rollback retries all publish the first retained result instead
of replaying structural work. Commit keeps its expiry callback until the outcome
cache accepts publication, and a committed fence cannot enter structural rollback.
Publication still needs valid ownership; if the original generation is invalid,
the local handoff below publishes a retained result under a validated new owner.
Mutating state queries use the exclusive queue; ordinary status queries remain
available. Terminal publication and expectation release precede fallible cleanup.
Each failed cleanup keeps its handle and diagnostic.

After release/reacquire or a bridge disconnect that changes the Studio generation,
an inactive old transaction is locally retired under the validated current owner.
The next begin retires it immediately; without a request, the next idle gate does
so within 120 seconds of the preceding idle check/activity with prompt scheduling.
If a timer runs during the release gap, it keeps another 120-second opportunity.
No valid owner, a busy gate or an actual operation still running can delay this.
The old generation is never rebound to replay rollback, history or scene cleanup.
Retirement does not finish an old history recording; `historyPending` reports it.
Studio recording availability and Undo behavior after this handoff remain unqualified.
Tracker records/observers leave the active journal slot and remain retained for
eviction, allowing the next begin. A completed pending result remains committed
or rolledBack; otherwise retirement publishes abandoned with the old/new generation,
a reason, `retryStopped: true` and a retention deadline. It does not promise idle
rollback retries under the new generation or claim that contents were restored.
A revoked prepare can release its two preparation fences only when invocation
was never authorized and no other operation is running. Authorized or possibly
running native work stays active and fenced, without a retention eviction deadline.

Recovery records retain the journal, detached outside roots, snapshots,
replacement maps, native undo roots and holding folder. They retry at 120-second
intervals under the exclusive gate when no newer transaction is active. They
never rerun structural snapshot rollback after releasing the lock. They restore
only identity-owned roots and replay journal edits with an original parent or a
proven rollback replacement. A matching path or surviving ancestor is not proof.
Native roots use a separate pending set. Each successful verified attachment
retires its entry permanently; the immutable undo evidence is kept separately.
The original target's child identities/names, outside-edit generation, serialized
content generation and raw attribute observation must still match the recovery
checkpoint, including newer Renium writes. Missing/lost attribute observation is
a conflict. Verified own insertions advance only their content guard. A changed target or service is a retained
conflict, even if conservative service-level detection covers an unrelated edit.
Journal recovery guards advance only the placement/property/attribute/tag that
was just verified, allowing transient partial replay failures to resume while
rejecting later external edits. Source callbacks check again before writing.
Conflicting later edits stay retained. Nonempty holding folders are never
destroyed. Empty containers and proven committed staging objects are retired;
failed disposal is retried with the original resource handle. Healthy commit
destroys detached outgoing roots directly, without registering per-descendant
expectations. There is one additional invoke handshake on native service imports.

`getEditorTransactionState` includes `nativePending`, `nativeOperation`,
`idleRollbackFailures`, `pendingTerminal` or a `recovery` summary as applicable.
Direct abandoned responses also include the summary. `stats.transactionRecoveries`
exposes unresolved edit/native-root/detached-root counts, pending cleanup, snapshots,
identities and errors. Summaries explicitly report `lifetime: "pluginSession"`,
`pluginSessionId`, `lossWarning`, `retentionSeconds: 1800`, `expiresAtUnix`,
`retryLimit: 8` and `retryStopped`. The initiating daemon error includes the lifetime
warning, unresolved counts and recovery state.

Recovery gets at most eight scheduled opportunities (including the first one at
abandonment); an active newer transaction uses an opportunity without replay.
Then automatic retries stop with a final diagnostic. Generation retirement stops
old-generation retries immediately instead. Local eviction is scheduled 30 minutes
from terminal transfer, not from begin or the last retry, and waits for the exclusive
gate; this is not an unconditional wall-clock maximum. Successfully cleaned-up
records may be released earlier. Each timer owns only its transfer token/id, so an
old timer cannot evict a newer recovery reusing the id. No retained instance is destroyed
or reparented by eviction. A compact `expired` summary preserves the unresolved
counts and warns that recovery data may be lost; at most 64 such diagnostics are
kept, with no additional time TTL. The recent-cleanup-error ring keeps 16 entries.
Local journal/expectation ownership is relinquished without disconnecting
observers claimed by a newer journal; final cleanup failures are reported. This
bounds retry work and the lifetime of local recovery snapshots/closures. Eviction
removes only imports bound to the retiring transaction identity, including inactive
and finished native imports. It clears their payloads and transaction callbacks,
restoring import slots and buffered-byte accounting, and drops large session fields
even if a delayed callback still owns the session table. A newer import with the
same id is untouched; possibly running work is never evicted. Engine-owned holding
folders and unresolved instances are not destroyed to reclaim memory; their
actual lifetime/GC requires Studio qualification. Disposal waits for the exclusive
gate if another operation is in progress. There is no per-recovery or global byte
cap: memory is proportional to retained snapshots/graphs and diagnostics, potentially
the affected place. The 512 MiB import-buffer limit does not bound recovery snapshots.
The 64-diagnostic cap is a count bound, since a diagnostic can contain many errors.
There is no durable export: plugin reload or Studio close can lose this data
earlier, and saving the visible place does not save detached recovery contents.
A fresh module reports `notFound`, its current `pluginSessionId` and
`recoveryDataMayBeLost`; the daemon compares the session id captured at begin and
reports a session change distinctly from an ordinary missing outcome. It cannot
reconstruct the previous session's counts. Ordinary terminal outcomes still
expire after 300 seconds and remain capped at 64. Replaying outside edits never
proves complete content restoration; abandoned packages remain Changed.

Restarting Studio clears unknown native execution and in-memory fences, but can
lose all retained recovery data; it does not prove rollback or recover detached
contents. A plugin reload similarly loses session-only records and is not proof
that a possibly running native write stopped. This delivery does not restart Studio.

Offline regression tests use the full editor module, production exclusive queue,
production runtime generation binding and actual session-lock predicate, with
engine boundaries substituted. Generation-handoff workloads also use production
journal-slot retirement. Invoke tests include duplicate permission, the inclusive
deadline before timer delivery, and completion without permission while both fences
are held; removing either gate fails the maintained suite. The 10,000-descendant commit fixture records the
before/after expectation counts and timing. These tests do not qualify real
Studio native execution, history/Undo/Redo, shutdown GC, Team Create or latency.
