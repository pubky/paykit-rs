# Link Recovery

## SDK-Owned Synchronization

Use `ensure_link_with_peer(counterparty, max_advance_steps)`. It chooses roles,
resumes persisted handshakes, checks recovery markers before reusing/advancing
links, and validates current signed Noise keys against pinned authorization.
It owns checkpoints and outbox cleanup. Do not independently reset Noise files,
snapshots, keys, or queues.
An identity's App Registry Noise key is not a per-peer Encrypted Link Recovery
Marker; identity-wide key rotation is not the repair for one broken peer.

For an intentional repair of a stalled link, call
`publish_encrypted_link_recovery_marker(counterparty)`, then resume bounded
`ensure_link_with_peer` calls. After an uncertain publication, inspect
`encrypted_link_recovery_marker_status(&counterparty)` before retrying. Do not
request fresh recovery on every poll or mistake an offline peer for a broken link.

One bounded Rust-method pseudocode cycle for a pending/recovering counterparty:

```text
link = await sdk.ensure_link_with_peer(counterparty, max_advance_steps)
if link.state != Linked:
    schedule_a_later_sync()
    return
received = await sdk.receive_private_messages(counterparty)
sent = await sdk.process_outbound_private_messages(counterparty)
inspect_reports(received, sent)
```

Scheduling/inspection are app logic. Handle errors at each step, not by continuing
with a stale handle. A locally `Linked` snapshot is not proof of remote liveness.
Established-link intake already checks signed authorization, recovery markers,
and the next message slot. Idle polling need not precede every receive with
`ensure_link_with_peer` or unconditionally drain outbound work. For an explicit
recovery check outside intake/preparation, use
`observe_encrypted_link_recovery_marker(counterparty)`; observation can mutate
local recovery state.

All-peer intake uses `receive_private_messages_from_linked_peers()`; delivery
maintenance uses `process_pending_private_messages()`. Idle intake batches share
one state read and bound concurrent read-only probes. Keep maintenance scheduled
even when no incoming message is found. These do not advance `Linking` handshakes;
enumerate `linked_peers()` and schedule `ensure_link_with_peer` for pending or
recovery-required peers too. For pay-contact UX,
`prepare_and_resolve_private_contact_payment(counterparty, amount,
after_private_payment_list_version, max_advance_steps)` performs bounded
preparation and private-only resolution.
Do not surround combined preparation with duplicate ensure/receive/resolve calls;
inspect its reports and resume only work that remains pending. Listing saved
requests through `list_payment_requests` does not itself perform network intake.

The app owns background scheduling; a retry timer does not guarantee mobile
background execution. Avoid overlapping per-peer workers and indefinite UI
loops. A handshake step limit is not a network timeout. Shared-state contention
can delay even reads: use the [busy/uncertainty policy](identity-state.md#live-shared-storage).
Prioritize foreground work before queued maintenance; do not interrupt an active
durable write to make room. Bound background batches, keep queued work cancellable,
and validate identity/session ownership again after queue admission. Display
caches must not replace fresh payment or authorization checks. Measure app queue
wait separately from SDK execution rather than assuming more concurrency helps.

## Delivery and Replay

The SDK persists exact prepared ciphertext with its advanced Noise checkpoint
before publication. An uncertain send resumes that prepared ciphertext before
later messages advance the link; do not recreate or re-encrypt it yourself.

For valid supported Event Messages, intake durably stores/indexes the message and
queues a Delivery Confirmation. It acknowledges the original Event ID plus a
SHA-256 hash of the exact raw JSON, not business acceptance or settlement.
`confirmed_at` records this separately from local outbound `Sent`. The SDK
retries unconfirmed events with the same Event ID/payload, including after
relinking when their app remains authorized; duplicates are confirmed without
reapplying their business event. Delivery Confirmations are not themselves
Event Messages and are never confirmed. Run both receive and outbound work so
confirmations can travel in both directions.

Private Payment Lists are Latest-State Messages per App, not confirmed events.
Recovery/replay does not guarantee every unread old ciphertext survives, nor
does it automatically supply custom-chat delivery/history. Preserve application
logical IDs, history, wallet journals, and unresolved executions independently.
Never edit queue statuses to force replay or assign fresh IDs to uncertain work.

## Failure Handling

| Observation | Action |
| --- | --- |
| Unavailable session/network | Preserve state; restore access/connectivity and retry |
| Missing registry/key or invalid signed authorization | Resolve enrollment/authorizer access; never trust unsigned registry keys or reset state |
| Stored `Linking` with handshake | Advance later using the same keys/state |
| `RecoveryRequired` thrown during preparation | Inspect `linked_peers()`; a pending handshake can produce this error before any resolution is returned |
| Stored `RecoveryRequired` or remote marker/key change | Let the SDK ensure/recovery flow relink and resume eligible work |
| Lease conflict or `ConcurrentUpdate` | Stop overlapping work, reload current state, retry with backoff |
| `SharedStateBusy` | Back off after bounded lock contention or pending-write recovery; preserve credentials and intent, with no immediate retry loop |
| Storage failure, corrupt state, or key-generation mismatch | Preserve evidence; repair access or use explicit backup recovery, never overwrite with empty state |
| Invalid event, Event ID conflict, blocked peer, or policy rejection | Surface it; do not retry as a new event or silently unblock |
| Policy code `retention_limit_reached` on a per-peer receive | Intake for that peer stays refused. Only with explicit user consent: `block_peer`, `forget_peer`, `unblock_peer`, then relink. `forget_peer` deletes what that peer sent and is refused when there is payment history with it; discard cached proposals for the peer first |

Use structured errors, not debug-string matching. Some batch reports flatten
underlying causes into generic codes; inspect peer/queue records or use a typed
per-peer call rather than classifying all batch failures as retryable. Marker
publication or later workflow steps can fail after state has already changed. A failed
call does not imply rollback or prove the remote peer was notified.

## Verify Recovery

Use isolated identities and non-monetary fixtures. Preserve the original failing
state before repairs; deleting both peers' data only tests a fresh start.

- Restart an interrupted handshake with persisted keys and live shared state.
- Have one peer request recovery while the other is still locally `Linked`.
- Recover simultaneously without repeatedly resetting progress.
- Lose a send response and a confirmation separately; verify stable Event IDs,
  duplicate suppression, and eventual confirmation after relinking.
- Exercise two apps against the same state, contention/cancellation, stale keys,
  and the actual missing/corrupt-state case. Check retained history and wallet
  outcomes separately from successful new-message exchange.

Sources: [link lifecycle](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/runtime/encrypted_links.rs),
[receive transactions](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/domain/private_stream/mod.rs),
[outbound worker](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/runtime/outbound_private.rs),
[replay](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/domain/linked_peers/mod.rs).
