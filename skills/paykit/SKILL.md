---
name: paykit
description: Integrate Paykit into wallets, payment processors, and apps using Rust or Swift/Kotlin. Use for shared-identity setup, public/private payments, session and key lifecycle, durable state and recovery, or assessing browser/custom-binding support.
---

# Paykit Integration

Paykit coordinates payments over Pubky. The app owns funds, authorization,
financial execution, settlement validation, scheduling, and product policy.

## Establish the Surface

Check the application's dependency pin, source, and generated declarations.
This skill describes the shared-identity architecture in the accompanying
source. Repository links use `master` for navigation, not as a release guarantee;
read the corresponding files at the application's actual version. Do not
silently upgrade dependencies or treat an unmerged change as an installed API.
Add migrations only for explicitly supported deployed formats; do not add
speculative compatibility.

- Rust apps: use [paykit-sdk](https://github.com/pubky/paykit-rs/tree/master/paykit-sdk).
- Swift/Kotlin apps: use [paykit-ffi](https://github.com/pubky/paykit-rs/tree/master/paykit-ffi)
  and [Mobile Integration](references/mobile.md).
- Stateless discovery/protocol work: use [paykit-lib](https://github.com/pubky/paykit-rs/tree/master/paykit-lib).
  Do not independently advance raw Noise handles alongside an SDK owning the
  same identity's links, checkpoints, or queues.
- Browser, custom chat, or service integration: read
  [Integration Boundaries](references/boundaries.md) before promising support.

Use [THESAURUS.md](https://github.com/pubky/paykit-rs/blob/master/THESAURUS.md)
for domain language. Reuse validated identifiers and Pubky routing helpers;
do not construct storage paths from unchecked strings.

## Shared Identity Model

- Configure a stable `PaykitAppId`, for example `bitkit`, with
  `PaykitSdkConfig::new("bitkit")`. An App ID attributes work and scopes public
  Payment Endpoints; it is not a receiver path or separate private channel.
- One Pubky identity shares Contact Records, Encrypted Links, private stream
  checkpoints, queues, requests, receipts, and accounting across its apps.
  Use the same live durable state, normally `PubkySharedStateStorage`, not
  independent device-local blobs presented as synchronization.
- Explicitly call `publish_paykit_app` to enroll this app. Discover apps through
  `paykit_app_registry(owner)`, not directory scans or Receiver Markers. The
  identity-wide App Registry advertises apps, capabilities, default preferences,
  and the current identity-wide Noise public key and generation.
- Private setup also requires a Paykit Noise Key Authorization signed by the
  Pubky identity. The authorizer publishes it before private enrollment or
  delegation; ordinary delegated apps cannot write that authorization. Registry
  key fields alone are not a trust anchor.
- Private Payment Lists supersede older lists from the same App and aggregate
  across authorized apps. Preserve App ID attribution when selecting a target.
  App IDs do not cryptographically isolate mutually authorized apps.

Before implementing startup, auth, storage, sign-out, removal, rotation, or
backups, read [Identity and State](references/identity-state.md). It covers the
live-session/key requirements and the homeserver locking limitation that gates
production multi-app use.

## Choose the Payment Flow

Read [Payment Workflows](references/workflows.md) for publication, list
consumption, explicit request-bound destinations, execution claims, and receipts.

- Public and private resolution are separate choices. A missing/recovering
  private link never authorizes an automatic public fallback.
- `PaymentAdapter` supplies receiving details, candidate selection, and payment
  targets. It does not execute or validate settlement for the wallet.
- Persist Private Payment List consumption before wallet submission. Keep it
  consumed through pending or uncertain execution; a retried SDK operation must
  not produce a second financial execution.
- Inspect per-target reports as well as exceptions. `failedToQueue` is not a
  rollback guarantee. Resume durable intent with stable IDs after uncertainty.

For background workers or broken links, read [Link Recovery](references/recovery.md).
The SDK confirms durable receipt of supported Event Messages and replays
unconfirmed events; neither local `Sent` nor a Delivery Confirmation means
payment acceptance, execution, or settlement.

Keep SDK backups encrypted by the app: exported blobs/strings contain sensitive
plaintext private state even though the live Pubky shared resource is encrypted.
Do not log keys, backup/state exports, endpoint payloads, or private messages.
