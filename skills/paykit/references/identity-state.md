# Identity and State

## Startup and Access

Rust construction uses `PaykitSdk::new(storage, session_provider,
payment_adapter, config)`, then `initialize().await?`. For shared operation,
construct `PubkySharedStateStorage::new(session_provider)` using a provider for
the same identity as the runtime. Mobile constructors are in
[Mobile Integration](mobile.md).

Ordinary apps request `PAYKIT_SESSION_CAPABILITIES` in Rust or
`requiredSessionCapabilities()` in bindings. An identity authorizer uses
`PAYKIT_AUTHORIZER_SESSION_CAPABILITIES` / `paykitAuthorizerSessionCapabilities()`
for signed-key publication, rotation, and recovery. Use the helpers, not copied
permission strings. Use grant-backed `PubkySessionAccess` and a stable app-owned
`PubkySessionBootstrap` client ID. Session creation, capability renewal, secure
credential storage, and key distribution remain the app's responsibility;
Ring is optional. A Pubky client ID and Paykit App ID have different roles.

| Access material | Consequence |
| --- | --- |
| Grant only | `PublicOnly`; public workflows can use local/callback storage, but live shared-state access cannot decrypt state |
| Grant + Pubky root secret | Can derive any Paykit key generation; absent an explicit Paykit key, session access derives generation 1 only |
| Grant + current `PaykitIdentitySecretKey` | Private/shared-state access without exposing the Pubky root secret; cannot derive other generations |
| No live session | Preserve state and restore access; shared-state reads are unavailable, not an empty identity |

Paykit derives distinct Noise and shared-state encryption keys from the Paykit
Identity Secret. Derive generation `g > 0` with
`PubkyLocalSecretKey::derive_paykit_identity_secret_key(g)`; delegated apps import
trusted 32-byte material with `PaykitIdentitySecretKey::new(bytes, g)`. Supply
the current generation explicitly after rotation, even with the root secret.
Do not generate app-specific Noise secrets or derive a remote Noise key from
its Pubky public key. A delegated key's derivation is not independently
verifiable without the root; import only through trusted authorization.

Before private app publication or delegation, the authorizer calls
`publish_paykit_noise_key_authorization()`. It needs the Pubky identity secret,
current Paykit key, and authorizer grant, including the separate authority path.
Ordinary delegated apps receive the current Paykit key and ordinary Paykit grant,
never the root secret or authority-path write access. Both peers need valid
Paykit Noise Key Authorizations; the SDK checks their signed routing/static keys
and generation and pins peer authorization. Missing, malformed, or conflicting
authorization fails closed. App Registry key fields are discovery metadata,
not a substitute. Key rotation uses the rotation API, not a fresh bootstrap
publication over an existing generation.

Publish this app with `publish_paykit_app(PaykitApp::new(display_name,
app_capabilities)?)`; its App ID comes from runtime config. `app_capabilities`
is a `PaykitAppCapabilities` record. Enable only supported `private_payments`,
`payment_requests`, `receipts`, and `outgoing_payments` features. These booleans
are not Pubky grant capabilities. Public-only registration can
omit the Noise key; private capabilities require current key material matching
the signed authorization. Both identities need initialized registry Noise keys
and signed authorizations for a new private link.

## Live Shared Storage

`PubkySharedStateStorage` encrypts the complete live state at the routing
constant `PAYKIT_SHARED_STATE_PATH` (`/pub/paykit/v0/shared-state.bin`). It is
not a backup uploaded after local mutations. Transactions load current state
under a renewed WebDAV write lock and publish changed state while holding it.
Bounded SDK operations reuse their loaded state and lock across compatible
transactions, but each changed checkpoint is durable before returning. Errors
do not roll back earlier checkpoints. Locks are released between outbound
messages and before wallet callbacks; do not wrap whole app workflows in a lock.
State-backed operations, including reads, require live session access and the
current Paykit secret. Do not replace a failed load/decode with empty state.
Encryption does not hide resource size/timing or prevent homeserver replay.

All participating apps use the same logical state. Separate local blobs are
only suitable for a single runtime owner; a custom shared adapter must provide
the atomic SDK contract. Raw inbound messages, indexes, and advanced Noise
checkpoints commit together. Outbound prepared ciphertext and its advanced
checkpoint commit before publication. App IDs separate ownership, not trust:
apps with shared keys/write access can access the same private state.

**Production constraint:** the homeserver must fence expired lock holders at
write commit and durably publish whole files, including after storage or database
failure. Check the deployed server against the
[SDK storage requirements](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/README.md#profile-and-contacts)
at the application's pin. A Pubky/Noise dependency upgrade does not upgrade the
server or establish those guarantees. Keep the SDK's uncertain-write cooldown;
it is a mitigation, not proof that an arbitrarily delayed write cannot publish.

An unconfirmed write leaves a pending marker. The next transaction waits five
minutes under a renewed lock before reloading. Cancellation restarts that wait
on the next call; competing operations can receive `SharedStateBusy`
(`shared_state_busy`). Reads may also block; there is no fixed completion
deadline. Back off and show pending recovery, not a tight retry loop or blank
state. `SharedStateBusy` can also mean ordinary lock contention, not just a
pending-write cooldown. Do not retry it as a revision conflict inside an
invalidated operation. Do not infer a safe retry from failure to inspect markers.
Ordinary successful writes have no timed cooldown. HTTP request timeouts belong
to the Pubky client; whole-operation cancellation must account for this wait.
After any uncertain multi-step call, inspect durable records and resume intent
rather than creating another request/payment.

## Sign-Out, Removal, and Rotation

- `sign_out()` revokes this app's current grant and clears local session access;
  it preserves shared state and public publication. Revocation/clearing can fail;
  do not report a fully completed sign-out then.
- `forget_session_access()` clears local access without revocation. Use only for
  intentional local-only cleanup; other copies of that grant remain valid.
- `remove_paykit_app()` withdraws this app's endpoints and registry entry, not
  shared history or another app's data. While still authenticated, inspect
  `paykit_app_removal_blockers()`: finish/cancel active requests, drain unconfirmed
  events and Receipt issuance, and clear nonempty app-owned Private Payment
  Lists. Failed cleanup leaves new app work blocked; retry removal or explicitly
  republish to reactivate. Removal is not grant revocation or key rotation.
- `rotate_paykit_identity_key(replacement_key)` requires the Pubky identity
  secret, authorizer grant, and current signed authorization. It replaces
  generation `g` with its root-derived successor `g + 1`; persist it first. Quiesce
  other writers; live peer leases block rotation, but expired leases cannot
  fence already-dispatched remote writes. History survives; old links require
  recovery and accounting reconciliation. Shared state commits before registry
  and replacement authorization publication, so retry uncertainty with the exact
  same current/replacement pair, then supply the replacement to every remaining
  authorized app.

Grant revocation does not erase an app's copied secrets/history. App removal
does not revoke its grant. Generation rotation limits delegated old-key access
to future state, but cannot revoke a holder of the Pubky root secret, which can
derive every generation. Treat root compromise separately from app enrollment.

## Backups and Restore

`export_backup_state()` and FFI `exportBackupString()` export sensitive plaintext
state, including private messages, Noise snapshots, and Receipt Decryption Keys.
Hex/serialization is not encryption. Encrypt backups before storage/upload;
never log them. Preserve session credentials and current key/generation securely
outside the backup. The live encrypted shared resource and backups are different:
with current access, apps reopen live state after sign-out/reinstall; keys alone
cannot reconstruct deleted private history.

Normal `restore_backup_state(backup, link_policy)` /
`restoreBackupString(backup, linkPolicy)` requires empty or matching
identity-only storage. Never overwrite healthy shared state with a stale app
backup. Republish participating App Registry entries after restore and reconcile
complete wallet execution history before automation. Restore retains peer
authorization pins; a saved snapshot does not bypass checks against current
signed keys.

The restored link policy is required because the SDK cannot tell whether a
backup is current. Pass `RequireRecovery` unless the app knows no runtime for
this identity sent or received private messages after the backup was exported,
then relink every peer in `recovery_required_peers`. The SDK also sends on its
own (Delivery Confirmations, Private Payment List syncs, retries). `Resume` on a
stale backup reuses a transport key and nonce and overwrites a published
message slot.

For missing/corrupt Pubky shared state, use
`recover_shared_state_from_backup(backup, replacement_key)` (FFI takes an
`SdkBackupBlob`, not the backup string). It needs the Pubky identity secret,
authorizer grant, existing protected authorization, and current and root-derived
successor keys; persist the successor first. Missing or conflicting authorization
fails before replacement. It rejects healthy state, unreadable generation
headers, unknown generations, and corrupt successor-generation state. Recovery
loses data newer than the backup, discards old Noise checkpoints/prepared sends,
and requires relinking and wallet reconciliation. Retry with the same keys and
backup; already-recovered valid state is preserved, including subsequent work.
Recovery publishes the replacement registry and authorization after committing
state. Distribute the replacement key to remaining authorized apps before they
resume private work.

Sources: [identity keys/access](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/identity.rs),
[shared storage](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/storage/pubky_shared.rs),
[app lifecycle](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/runtime/app_registry.rs),
[signed keys](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/runtime/noise_key_authorization.rs),
[rotation](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/runtime/key_rotation.rs),
[backup](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/runtime/backup.rs).
