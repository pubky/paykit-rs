# Link Recovery

## Let the SDK Own the Link

For SDK-supported flows, use `ensure_link_with_peer(counterparty,
counterparty_receiver_path, max_advance_steps)`. It selects handshake roles
deterministically, resumes persisted handshakes, and starts recovery for peers
already marked `RecoveryRequired`. It owns the associated checkpoint and outbox
cleanup. Do not duplicate role selection or manually clear outbox files around it.

Both participants need their receiver Noise secret and published Receiver Marker.
A Receiver Marker advertises capabilities and the receiver Noise public key.
An Encrypted Link Recovery Marker signals per-counterparty link repair. Rotating
the receiver key/marker is not a substitute for repairing one peer's link; it
affects other links using that key too.

`Linked` returned by `ensure_link_with_peer` means a local active snapshot exists.
That branch does not poll the peer's recovery marker or prove peer liveness.
Private payment resolution observes recovery markers, but an app that only runs
link/inbox operations should call `observe_encrypted_link_recovery_marker`
explicitly during its synchronization cycle when marker policy is enabled.
The observation can change local recovery state; it is not just a read-only poll.

## Bounded Synchronization Cycle

For a known peer, the following is Rust-method pseudocode. Serialize overlapping
work for the same peer; use the installed API's reports and error types.

```text
if recovery_marker_policy_enabled:
    marker = await sdk.observe_encrypted_link_recovery_marker(peer, receiver)

link = await sdk.ensure_link_with_peer(peer, receiver, max_advance_steps)
if link.state != Linked:
    schedule_a_later_sync()
    return

received = await sdk.receive_private_messages(peer, receiver)
sent = await sdk.process_outbound_private_messages(peer, receiver)
inspect_reports(received, sent)
```

Scheduling and `inspect_reports` are application logic, not SDK APIs. Apply the
error handling below at each awaited step; an error is not a reason to execute
the remaining steps against a stale handle. This is one bounded cycle, not a
claim that one pass completes communication. For pay-contact UX, prefer
`prepare_and_resolve_private_contact_payment`, which already advances the link
and processes available private work before resolving. It can throw
`RecoveryRequired` before returning a resolution if the handshake is still
`Linking`. Check that peer's stored state in `linked_peers`; a pending handshake
needs a later retry, not a reset or public fallback.

For all-peer background work, use `receive_private_messages_from_linked_peers`
and `process_pending_private_messages`. Both skip `Linking` peers, so enumerate
them with `linked_peers` and call `ensure_link_with_peer` on later cycles too.
Keep the per-peer marker observation above when marker policy is enabled.

The app owns foreground/background scheduling and connectivity retries. Do not
busy-loop until the peer responds, wait indefinitely inside a UI action, or
claim a local retry timer guarantees mobile background execution. Retain queued
work when the remote app is offline. Pubky/client configuration owns request
timeouts; a handshake step limit is not a network timeout.

## Failure Handling

| Observation | Integration action |
| --- | --- |
| Temporary network failure or unavailable live session | Preserve state; retry after access/connectivity returns |
| Remote Receiver Marker missing | Resolve enrollment/discovery; do not treat a missing advertised key as permission to replace local state |
| `Linking` with a persisted handshake | Continue SDK advancement on a later cycle; do not generate new keys |
| Peer-operation lease conflict | Avoid overlapping workers and retry later; do not erase leases or snapshots to force progress |
| Stored peer state is `RecoveryRequired` | Use the SDK recovery/ensure flow and inspect marker errors; the error variant alone does not distinguish this from a pending handshake |
| Remote recovery marker observed while locally linked | Let SDK observation update state, then ensure/advance the link; do not keep using an old raw handle |
| Storage conflict/failure or undecodable state | Do not overwrite with an empty state; retain evidence/state and resolve the storage failure |
| Invalid event, conflicting Event ID, blocked peer, or policy/validation failure | Surface the failure; do not retry automatically as a new event or silently unblock |

Do not classify failures from error-string fragments. Use `PaykitSdkError` in
Rust and structured binding errors where available. Some batch reports flatten
causes to strings and expose only generic codes such as `receive_failed` or
`queue_processing_failed`. Those codes do not distinguish transport from
recovery/policy failures. Inspect persisted peer/queue state and use typed
per-peer operations when needed; do not assume every batch failure is retryable.
Recovery-required state can be persisted even if publishing its remote marker
fails; inspect and retry that publication through SDK APIs rather than assuming
the peer was notified.
More generally, a failed call may already have persisted progress. Inspect the
current SDK state instead of assuming the operation rolled back completely.

## Recovery Is Not Delivery or History

Link restoration, local send completion, remote receipt, payment settlement,
and application processing are different outcomes. On master, outbound `Sent`
is a local checkpoint, not an acknowledgement by the peer. Event ID dedupe
prevents duplicate processing of supported events; it does not prove missing
messages arrived. A successful relink does not guarantee recovery of old unread
ciphertext or automatically deliver arbitrary chat messages.

Keep application history, attachment keys, and conversation identifiers separate
from replaceable Noise snapshots or binding handle IDs. Never delete those
records merely because a link cannot be restored. If custom messaging needs
delivery guarantees, retain stable logical IDs and an application outbox with
an explicit acknowledgement/reconciliation policy. Do not label that policy a
feature already supplied by Paykit or reset SDK queue statuses to force replay.

Preserve original Event IDs/payloads through SDK retries. Inspect terminal and
recovery-required records after relinking rather than assuming every old message
is safe to resend. Draining an old working link may save available inbound
messages, but cannot guarantee an offline peer has no more old-link work.

Local SDK transactions and per-peer leases do not make separately stored
snapshots on multiple devices safe to share. Do not restore the same receiver
state into concurrently active, uncoordinated instances and call it device sync.

## Verify the Failure, Not Just a Fresh Link

Use isolated test identities and non-monetary test messages. Preserve a failing
fixture before any repair attempt; do not delete real homeserver data as a test.

- A fresh pair establishes a link and exchanges supported messages in both directions.
- An interrupted handshake survives process restart using the same persisted keys/state.
- One peer requests recovery while the other is still locally `Linked`; observation and advancement restore communication.
- Simultaneous recovery does not reset repeatedly after both peers make progress.
- Temporary network failures preserve history, snapshots, and durable outbound intent.
- The exact missing/corrupt-state case under investigation recovers, or reports a specific remaining failure.
- After repair, check old pending work and application history separately from successful new-message exchange.

Inspect both clients' outcomes. Starting with clean state on both sides does not
test recovery from the preserved broken state.

Sources: [link lifecycle](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/runtime/encrypted_links.rs),
[recovery markers](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/runtime/recovery.rs),
[receive](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/runtime/private_stream.rs),
[outbound worker](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/runtime/outbound_private.rs).
