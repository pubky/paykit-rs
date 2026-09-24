---
name: paykit
description: Integrate Paykit into wallets, payment processors, and other apps. Use for Rust SDK or Swift/Kotlin setup, payment and recovery workflows, browser support assessment, and distinguishing SDK capabilities from custom bindings or server APIs.
---

# Paykit Integration

Paykit coordinates payments over Pubky. It does not hold funds, execute payments,
or verify settlement for the integrating wallet.

## Choose the Integration Surface

Check the application's dependency version and existing Paykit integration first.
Read documentation and signatures at that version or commit; the links below
point to `master`. Use installed/generated declarations when documentation and
signatures disagree. Do not use APIs from unmerged PRs or silently upgrade the
application to follow an example. Paykit is pre-release software.

- Rust applications: use [paykit-sdk](https://github.com/pubky/paykit-rs/tree/master/paykit-sdk).
- Swift/Kotlin applications: use [paykit-ffi](https://github.com/pubky/paykit-rs/tree/master/paykit-ffi)
  and read [Mobile Integration](references/mobile.md).
- Stateless public discovery or protocol-level integrations: use
  [paykit-lib](https://github.com/pubky/paykit-rs/tree/master/paykit-lib).
  Prefer SDK-managed queues, checkpoints, and recovery where the SDK exposes
  the required workflow. Raw encrypted messaging is not the same surface as
  the SDK's typed payment workflows.

Read the reference that matches the work:

- [Integration Boundaries](references/boundaries.md): unfamiliar integrations,
  forks, web/WASM, custom chat payloads, or Paykit Server/Locks integrations.
- [Link Recovery](references/recovery.md): Encrypted Link setup, background
  synchronization, failed messages, missing state, or destructive reset code.
- [Payment Workflows](references/workflows.md): end-to-end wallet or checkout
  flows, Payment Requests, Receipts, and integration acceptance tests.
- [Mobile Integration](references/mobile.md): Swift/Kotlin callbacks, platform
  setup, reservation inputs, state persistence, and backup notifications.

These references describe the receiver-scoped SDK on master. When maintaining
the skill, change guidance alongside the API it describes; keep proposals and
fork-only features separate from supported behavior.

## Runtime and Identity

- Configure an explicit local `PaykitReceiverPath`, shaped as
  `{app}/{wallet|server}`, for example `example-wallet/wallet`.
  Counterparty APIs also need the **remote** receiver path; it need not match
  the local one.
- A Contact Record groups receivers under one Pubky identity. Discover paths
  with `paykit_receiver_paths`, then choose the receiver for the payment flow.
  Discovery does not imply receiver preference; select using the application's
  payment policy. Keep requests and replies tied to their receiver.
- Each receiver owns its SDK state and Noise secret. Sharing a Pubky identity
  does not synchronize private state between apps or devices.
- Construct `PaykitSdk` with a durable `StorageAdapter`, a
  `PubkySessionProvider`, a `PaymentAdapter`, and `PaykitSdkConfig`; call
  `initialize` before workflows. `InMemoryStorage` is for tests.
- Use `PubkySessionBootstrap` for account/session flows with a stable app-owned
  client ID. Request `config.required_session_capabilities()` rather than
  duplicating permission strings. Ring is optional, not an integration prerequisite.
- Generate and securely persist one `ReceiverNoiseSecretKey` per receiver;
  reuse it after restart or reauthentication. An externally authorized Pubky
  session can support private payments without the Pubky identity secret,
  provided the receiver Noise secret is supplied.

For exact setup contracts, read the
[SDK integration guide](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/README.md#integration-shape)
and [session API](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/pubky_session.rs).

## Publish and Resolve Payments

The following names are Rust SDK methods; mobile equivalents use camelCase.

When enabling private payments, explicitly publish a Receiver Marker with
`publish_paykit_receiver_marker`. New private links require both receivers'
markers to obtain their Noise public keys, even if public endpoints already
exist. Marker publication makes the receiver publicly discoverable; setup,
auth, and profile helpers do not publish it automatically. Use SDK marker/link
APIs rather than inventing discovery files or deriving a remote Noise key from
its Pubky key.

- `PaymentAdapter` supplies receiving details, candidate ordering, and targets.
  Wallet-specific validation, authorization, execution, and settlement stay in
  the app, not Paykit.
- Publish public details with `sync_public_endpoints` or
  `sync_public_endpoints_with_receiving_details`. Keep `ManagedOnly` cleanup
  unless the app intentionally owns the whole receiver's endpoint namespace.
- Call `ensure_link_with_peer` before the first private list sync. Queueing
  requires a link or persisted handshake; list sync does not start one. Use
  reservation-backed sync for wallet reservations, retaining IDs on retries.
- Use `prepare_and_resolve_private_contact_payment` for private preparation and
  resolution. Pass the peer key/path, optional amount, optional consumed list
  version, and handshake advance limit. It returns private endpoints only.
- `resolve_public_contact_payment` is a separate public flow. No API combines
  these sources or automatically falls back. Let the app choose explicitly.

Follow [Payment Workflows](references/workflows.md) for the complete sequence,
including list consumption, partial results, requests, proofs, and receipts.

Receiver discovery finds public endpoints or a valid Receiver Marker. A marker
also makes a receiver with no public endpoints discoverable.

## State, Retries, and Recovery

- Persist SDK state atomically. Received messages, derived indexes, and the
  advanced Noise checkpoint must commit together. A seed or Pubky account alone
  cannot reconstruct private runtime state; implement SDK backup/export too.
  Backups contain private messages and recovery keys; encrypt them before
  storage or upload and never log them.
- For background work, use `receive_private_messages_from_linked_peers` and
  `process_pending_private_messages`. Neither advances `Linking` peers; call
  `ensure_link_with_peer` for those peers on later cycles. For one peer, pair
  `receive_private_messages` with `process_outbound_private_messages`.
  A queued or locally `Sent` message is not proof the peer received it. Retain
  durable work and IDs on errors rather than recreating requests or link secrets.
- Inspect workflow reports as well as thrown errors. Private resolution has
  separate `status` and `state` fields. Preparation can throw `RecoveryRequired`
  while the stored peer is still `Linking`; retry the pending handshake later.
  Neither that error nor `RecoveryPending` means erase data or silently switch
  to public payment. See [Link Recovery](references/recovery.md).
- Keep protocol recovery separate from application history and payment execution.
  Read [Link Recovery](references/recovery.md) before adding reset or retry logic;
  a catch-all that deletes snapshots, queues, or history is not a recovery policy.
- `sign_out` revokes the grant and clears local identity-scoped SDK state.
  `forget_session_access` clears locally without revocation. Neither is an
  ordinary disconnect/pause operation; back up first if restoration is needed.

## Additional Workflows

- **Payment Requests and subscriptions:** use SDK lifecycle methods, not raw
  event JSON. Acceptance or submitted proof does not execute or settle a payment.
  Recurring requests still need app-owned scheduling and authorization. Read
  [Payment Requests](https://github.com/pubky/paykit-rs/blob/master/specs/payment-requests.md).
- **Receipts:** prepare issuance before network side effects and reuse a stable
  Receipt ID on retry. Use SDK receipt retrieval for fetch/decrypt/verification;
  the issuer still needs payment-method-specific settlement validation. Read the
  [receipt API](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/runtime/receipts.rs).
- **Profiles and contacts:** use SDK profile/avatar/blob and local contact
  helpers. `resolve_contact_profile` supports Paykit-to-Pubky profile fallback.
  Public Contact Markers are opt-in and distinct from Receiver Markers. Read the
  [profile namespace guide](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/README.md#profile-and-contact-namespace).

Test the integration's boundaries: restart with persisted state, retry after a
partial workflow, missing live session, receiver selection, and backup restore.
Use the installed SDK's structured errors and reports, not error-string matching;
inspect persisted state when a batch report lacks a precise failure category.
