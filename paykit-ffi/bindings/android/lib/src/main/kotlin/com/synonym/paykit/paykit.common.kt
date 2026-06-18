

@file:Suppress("RemoveRedundantBackticks")

package com.synonym.paykit

// Common helper code.
//
// Ideally this would live in a separate .kt file where it can be unittested etc
// in isolation, and perhaps even published as a re-useable package.
//
// However, it's important that the details of how this helper code works (e.g. the
// way that different builtin types are passed across the FFI) exactly match what's
// expected by the Rust code on the other side of the interface. In practice right
// now that means coming from the exact some version of `uniffi` that was used to
// compile the Rust component. The easiest way to ensure this is to bundle the Kotlin
// helpers directly inline like we're doing here.

public class InternalException(message: String) : kotlin.Exception(message)

// Public interface members begin here.


// Interface implemented by anything that can contain an object reference.
//
// Such types expose a `destroy()` method that must be called to cleanly
// dispose of the contained objects. Failure to call this method may result
// in memory leaks.
//
// The easiest way to ensure this method is called is to use the `.use`
// helper method to execute a block and destroy the object at the end.
@OptIn(ExperimentalStdlibApi::class)
public interface Disposable : AutoCloseable {
    public fun destroy()
    override fun close(): Unit = destroy()
    public companion object {
        internal fun destroy(vararg args: Any?) {
            for (arg in args) {
                when (arg) {
                    is Disposable -> arg.destroy()
                    is ArrayList<*> -> {
                        for (idx in arg.indices) {
                            val element = arg[idx]
                            if (element is Disposable) {
                                element.destroy()
                            }
                        }
                    }
                    is Map<*, *> -> {
                        for (element in arg.values) {
                            if (element is Disposable) {
                                element.destroy()
                            }
                        }
                    }
                    is Array<*> -> {
                        for (element in arg) {
                            if (element is Disposable) {
                                element.destroy()
                            }
                        }
                    }
                    is Iterable<*> -> {
                        for (element in arg) {
                            if (element is Disposable) {
                                element.destroy()
                            }
                        }
                    }
                }
            }
        }
    }
}

@OptIn(kotlin.contracts.ExperimentalContracts::class)
public inline fun <T : Disposable?, R> T.use(block: (T) -> R): R {
    kotlin.contracts.contract {
        callsInPlace(block, kotlin.contracts.InvocationKind.EXACTLY_ONCE)
    }
    return try {
        block(this)
    } finally {
        try {
            // N.B. our implementation is on the nullable type `Disposable?`.
            this?.destroy()
        } catch (e: Throwable) {
            // swallow
        }
    }
}

/** Used to instantiate an interface without an actual pointer, for fakes in tests, mostly. */
public object NoPointer













/**
 * Stateful Paykit SDK runtime handle.
 */
public interface FfiPaykitSdkInterface {

    /**
     * Start an Encrypted Link Handshake as the responder.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `acceptLinkWithPeer`(`counterparty`: kotlin.String): FfiLinkedPeerHandshakeReport

    /**
     * Queue acceptance for a received Payment Request and return local derived state.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `acceptPaymentRequest`(`counterparty`: kotlin.String, `paymentRequestId`: kotlin.String): FfiPaymentRequestRecord

    /**
     * Return received Payment Requests that need a local payer response.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `actionableReceivedPaymentRequests`(): List<FfiPaymentRequestRecord>

    /**
     * Return accepted recurring Payment Requests across non-blocked counterparties.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `activeRecurringPaymentRequests`(): List<FfiPaymentRequestRecord>

    /**
     * Advance the stored Encrypted Link Handshake for one counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `advanceLinkHandshake`(`counterparty`: kotlin.String): FfiLinkedPeerHandshakeReport

    /**
     * Block a counterparty for local Paykit private workflows.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `blockPeer`(`counterparty`: kotlin.String): FfiLinkedPeerRecord

    /**
     * Queue cancellation for a known non-terminal Payment Request.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `cancelPaymentRequest`(`counterparty`: kotlin.String, `paymentRequestId`: kotlin.String, `reason`: kotlin.String?): FfiPaymentRequestRecord

    /**
     * Return this runtime's configuration.
     */
    public fun `config`(): FfiPaykitSdkConfig

    /**
     * Return one local Contact Record.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `contactRecord`(`publicKey`: kotlin.String): FfiContactRecord?

    /**
     * Return all local Contact Records.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `contactRecords`(): List<FfiContactRecord>

    /**
     * Return the latest valid Private Payment List view for a counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `currentPrivatePaymentList`(`counterparty`: kotlin.String): FfiPrivatePaymentListView?

    /**
     * Delete a blob by `pubky://` URI or configured Paykit profile path.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `deletePaykitBlob`(`uriOrPath`: kotlin.String)

    /**
     * Return tracked Encrypted Link recovery marker state for a counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `encryptedLinkRecoveryMarkerStatus`(`counterparty`: kotlin.String): FfiEncryptedLinkRecoveryMarkerReport?

    /**
     * Queue the current complete Private Payment List for one counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `enqueuePrivatePaymentList`(`counterparty`: kotlin.String): FfiQueuedPrivateMessage

    /**
     * Export SDK-managed backup state as an opaque blob.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `exportBackupState`(): FfiSdkBackupBlob

    /**
     * Fetch a public Paykit Profile.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `fetchPaykitProfile`(`publicKey`: kotlin.String): FfiPaykitProfileRecord?

    /**
     * Fetch public Pubky file bytes.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `fetchPubkyFile`(`uri`: kotlin.String): kotlin.ByteArray?

    /**
     * Fetch public Pubky app follows.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `fetchPubkyFollows`(`publicKey`: kotlin.String): List<kotlin.String>

    /**
     * Fetch a public Pubky app profile.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `fetchPubkyProfile`(`publicKey`: kotlin.String): FfiPubkyProfileRecord?

    /**
     * Fetch a public Pubky UTF-8 text file.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `fetchPubkyText`(`uri`: kotlin.String): kotlin.String?

    /**
     * Return current identity status, when initialized.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `identityStatus`(): FfiIdentityStatus?

    /**
     * Initialize durable SDK identity state.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `initialize`(): FfiInitializationReport

    /**
     * Start an Encrypted Link Handshake as the initiator.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `initiateLinkWithPeer`(`counterparty`: kotlin.String): FfiLinkedPeerHandshakeReport

    /**
     * Prepare, store, and queue Receipt Access for private delivery.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `issueReceipt`(`counterparty`: kotlin.String, `draft`: FfiReceiptDraft): FfiReceiptIssuanceView

    /**
     * List issued receipts across non-blocked counterparties, newest first.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `issuedReceipts`(): List<FfiReceiptIssuanceView>

    /**
     * List issued receipts for one counterparty, newest first.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `issuedReceiptsTo`(`counterparty`: kotlin.String): List<FfiReceiptIssuanceView>

    /**
     * List locally tracked Linked Peer records.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `linkedPeers`(): List<FfiLinkedPeerRecord>

    /**
     * Return Payment Requests matching a local SDK filter.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `listPaymentRequests`(`filter`: FfiPaymentRequestFilter): List<FfiPaymentRequestRecord>

    /**
     * Observe a counterparty's public recovery marker.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `observeEncryptedLinkRecoveryMarker`(`counterparty`: kotlin.String): FfiEncryptedLinkRecoveryMarkerReport

    /**
     * Return all Payment Requests across non-blocked counterparties.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `paymentRequests`(): List<FfiPaymentRequestRecord>

    /**
     * Return Payment Requests involving one counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `paymentRequestsWith`(`counterparty`: kotlin.String): List<FfiPaymentRequestRecord>

    /**
     * List counterparties with queued private messages ready for retry.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `pendingOutboundPrivateCounterparties`(): List<kotlin.String>

    /**
     * Prepare a receipt issuance and persist it before network side effects.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `prepareReceiptIssuance`(`counterparty`: kotlin.String, `draft`: FfiReceiptDraft): FfiReceiptIssuanceView

    /**
     * Send queued outbound private messages for one counterparty in order.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `processOutboundPrivateMessages`(`counterparty`: kotlin.String): FfiOutboundPrivateSendReport

    /**
     * Process queued outbound private messages for every pending counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `processPendingPrivateMessages`(): List<FfiOutboundPrivateCounterpartySendReport>

    /**
     * Continue storage and Receipt Access queueing for a prepared issuance.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `processReceiptIssuance`(`counterparty`: kotlin.String, `receiptId`: kotlin.String): FfiReceiptIssuanceView

    /**
     * Queue a new Payment Request proposal and return local derived state.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `proposePaymentRequest`(`counterparty`: kotlin.String, `terms`: FfiPaymentRequestTerms): FfiPaymentRequestRecord

    /**
     * Publish a minimal local recovery marker for a counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `publishEncryptedLinkRecoveryMarker`(`counterparty`: kotlin.String): FfiEncryptedLinkRecoveryMarkerReport

    /**
     * Publish a blob under this identity's Paykit profile namespace.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `publishPaykitBlob`(`blobName`: kotlin.String, `bytes`: kotlin.ByteArray): FfiPaykitBlobRecord

    /**
     * Publish this identity's Paykit Profile.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `publishPaykitProfile`(`profile`: FfiPaykitProfile): FfiPaykitProfileRecord

    /**
     * Publish a public Contact Marker for a local Contact Record.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `publishPublicContact`(`publicKey`: kotlin.String): FfiContactRecord

    /**
     * List Receipt Access across non-blocked counterparties, newest first.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `receiptAccess`(): List<FfiReceiptAccessView>

    /**
     * List Receipt Access received from one counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `receiptAccessFrom`(`counterparty`: kotlin.String): List<FfiReceiptAccessView>

    /**
     * List indexed Receipt Access records for one counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `receiptAccessRecords`(`counterparty`: kotlin.String): List<FfiReceiptAccessView>

    /**
     * List local receipt issuance records for one counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `receiptIssuanceRecords`(`counterparty`: kotlin.String): List<FfiReceiptIssuanceView>

    /**
     * List decrypted Receipt records for one issuer, newest first.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `receiptRecords`(`issuer`: kotlin.String): List<FfiReceiptRecord>

    /**
     * List decrypted receipts across non-blocked issuers, newest first.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `receipts`(): List<FfiReceiptRecord>

    /**
     * List decrypted receipts from one issuer, newest first.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `receiptsFrom`(`issuer`: kotlin.String): List<FfiReceiptRecord>

    /**
     * Receive and durably persist available private messages.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `receivePrivateMessages`(`counterparty`: kotlin.String): FfiPrivateStreamIntakeReport

    /**
     * Receive private messages from every locally linked counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `receivePrivateMessagesFromLinkedPeers`(): List<FfiPrivateStreamCounterpartyIntakeReport>

    /**
     * Return inbound Payment Requests received from one counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `receivedPaymentRequestsFrom`(`counterparty`: kotlin.String): List<FfiPaymentRequestRecord>

    /**
     * Refresh the cached Paykit Profile for a local Contact Record.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `refreshContactPaykitProfile`(`publicKey`: kotlin.String): FfiContactRecord?

    /**
     * Queue rejection for a received Payment Request and return local derived state.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `rejectPaymentRequest`(`counterparty`: kotlin.String, `paymentRequestId`: kotlin.String, `reason`: kotlin.String?): FfiPaymentRequestRecord

    /**
     * Remove a local Contact Record when it has no public marker to clean up.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `removeContact`(`publicKey`: kotlin.String): FfiContactRecord?

    /**
     * Remove the local public recovery marker for a counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `removeEncryptedLinkRecoveryMarker`(`counterparty`: kotlin.String): FfiEncryptedLinkRecoveryMarkerReport

    /**
     * Remove a public Contact Marker.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `removePublicContact`(`publicKey`: kotlin.String): FfiContactRecord?

    /**
     * Resolve payable endpoints for one counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `resolveContactPayment`(`request`: FfiContactPaymentResolutionRequest): FfiContactPaymentResolution

    /**
     * Resolve display metadata for a contact.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `resolveContactProfile`(`publicKey`: kotlin.String, `allowPubkyProfileFallback`: kotlin.Boolean): FfiContactProfileResolution?

    /**
     * Restore SDK-managed backup state from an opaque blob.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `restoreBackupState`(`backup`: FfiSdkBackupBlob): FfiRestoreReport

    /**
     * Fetch, decrypt, and store a receipt from an indexed Receipt Access event.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `retrieveReceipt`(`counterparty`: kotlin.String, `receiptId`: kotlin.String): FfiReceiptRecord

    /**
     * Save or update a local Contact Record.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `saveContact`(`update`: FfiContactUpdate): FfiContactRecord

    /**
     * Clear live Pubky session access and SDK-managed identity-scoped state.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `signOut`(): FfiIdentityStatus

    /**
     * Queue a Payment Proof for an accepted Payment Request.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `submitPaymentProof`(`counterparty`: kotlin.String, `paymentRequestId`: kotlin.String, `proof`: FfiPaymentProofSubmission): FfiPaymentRequestRecord

    /**
     * Retry pending public Contact Marker publication/removal work.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `syncPublicContactMarkers`(): List<FfiContactRecord>

    /**
     * Publish current public receiving details and remove stale SDK-managed endpoints.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `syncPublicEndpoints`(): FfiEndpointSyncReport

    /**
     * Remove a local peer block and return the peer to NotLinked.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `unblockPeer`(`counterparty`: kotlin.String): FfiLinkedPeerRecord

    public companion object
}




/**
 * Payment adapter payload text with redacted debug output.
 */
public interface FfiPaymentPayloadInterface {

    /**
     * Export the payload text for payment adapter execution.
     */
    public fun `exportText`(): kotlin.String

    public companion object
}




/**
 * Payment Reference text with redacted debug output.
 */
public interface FfiPaymentReferenceInterface {

    /**
     * Export the reference text for explicit payment execution or display.
     */
    public fun `exportText`(): kotlin.String

    public companion object
}




/**
 * Private workflow error with redacted default context.
 */
public interface FfiPrivateOperationErrorInterface {

    /**
     * Stable error category for app branching.
     */
    public fun `category`(): kotlin.String

    /**
     * Stable error code for app branching.
     */
    public fun `code`(): kotlin.String

    /**
     * Export raw debug details for explicit diagnostic handling.
     */
    public fun `exportDebugDetails`(): kotlin.String

    /**
     * Redacted error context safe for normal UI/log surfaces.
     */
    public fun `redactedContext`(): kotlin.String

    public companion object
}




/**
 * Pending Pubky auth request.
 */
public interface FfiPubkyAuthRequestInterface {

    /**
     * Return the auth URL to show as a deeplink or QR code.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `authorizationUrl`(): kotlin.String

    /**
     * Wait for auth approval and validate the resulting session capabilities.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `complete`(`localSecretKey`: FfiPubkyLocalSecretKey?, `requiredCapabilities`: kotlin.String): FfiPubkySessionBootstrapResult

    public companion object
}




/**
 * Local Pubky secret key bytes supplied by platform secure storage.
 */
public interface FfiPubkyLocalSecretKeyInterface {

    /**
     * Export the raw bytes for platform secure storage.
     */
    public fun `exportBytes`(): kotlin.ByteArray

    public companion object
}




/**
 * Live Pubky access material supplied by platform session storage.
 */
public interface FfiPubkySessionAccessInterface {

    /**
     * Export the local Pubky secret key, when available.
     */
    public fun `exportLocalSecretKey`(): FfiPubkyLocalSecretKey?

    /**
     * Export the Pubky session bearer secret for platform secure storage.
     */
    public fun `exportSessionSecret`(): kotlin.String

    public companion object
}




/**
 * Pubky session bootstrap helper.
 */
public interface FfiPubkySessionBootstrapInterface {

    /**
     * Approve a Pubky auth URL with this local secret key.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `approveAuth`(`authUrl`: kotlin.String, `expectedCapabilities`: kotlin.String, `localSecretKey`: FfiPubkyLocalSecretKey)

    /**
     * Import an exported Pubky session secret.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `importSession`(`sessionSecret`: kotlin.String, `localSecretKey`: FfiPubkyLocalSecretKey?, `requiredCapabilities`: kotlin.String): FfiPubkySessionBootstrapResult

    /**
     * Resume a short-lived auth flow from its authorization URL.
     */
    @Throws(PaykitFfiException::class)
    public fun `resumeAuth`(`authorizationUrl`: kotlin.String, `expectedCapabilities`: kotlin.String): FfiPubkyAuthRequest

    /**
     * Sign in with a local Pubky secret key and return session access material.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `signIn`(`localSecretKey`: FfiPubkyLocalSecretKey): FfiPubkySessionBootstrapResult

    /**
     * Sign up on a homeserver and return session access material.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `signUp`(`localSecretKey`: FfiPubkyLocalSecretKey, `homeserverPublicKey`: kotlin.String, `signupCode`: kotlin.String?): FfiPubkySessionBootstrapResult

    /**
     * Start a sign-in auth flow for an external signer.
     */
    @Throws(PaykitFfiException::class)
    public fun `startSignInAuth`(`capabilities`: kotlin.String): FfiPubkyAuthRequest

    /**
     * Start a signup auth flow for an external signer.
     */
    @Throws(PaykitFfiException::class)
    public fun `startSignUpAuth`(`capabilities`: kotlin.String, `homeserverPublicKey`: kotlin.String, `signupToken`: kotlin.String?): FfiPubkyAuthRequest

    public companion object
}




/**
 * Reservation attribution metadata with redacted debug output.
 */
public interface FfiReservationAttributionInterface {

    /**
     * Export attribution fields for payment adapter cleanup.
     */
    public fun `exportFields`(): Map<kotlin.String, kotlin.String>

    public companion object
}




/**
 * SDK backup blob owned by the app.
 */
public interface FfiSdkBackupBlobInterface {

    /**
     * Export the raw bytes for app-controlled backup storage.
     */
    public fun `exportBytes`(): kotlin.ByteArray

    public companion object
}




/**
 * Platform-owned payment adapter callbacks.
 */
public interface FfiSdkPaymentAdapter {

    /**
     * Return current receiving details for a scope.
     */
    @Throws(PaykitFfiException::class)
    public fun `currentReceivingDetails`(`scope`: FfiReceivingDetailScope): List<FfiReceivingDetail>

    /**
     * Reserve receiving details for a counterparty's Private Payment List.
     */
    @Throws(PaykitFfiException::class)
    public fun `reserveReceivingDetails`(`counterparty`: kotlin.String): List<FfiPaymentEndpointReservation>?

    /**
     * Cancel a previously reserved receiving detail.
     */
    @Throws(PaykitFfiException::class)
    public fun `cancelReceivingDetailReservation`(`cancellation`: FfiPaymentEndpointReservationCancellation)

    /**
     * Return payable candidate ids in adapter-preferred order.
     */
    @Throws(PaykitFfiException::class)
    public fun `selectPaymentEndpointIds`(`request`: FfiPaymentEndpointSelectionRequest): List<kotlin.String>

    /**
     * Build a payment target from a payable endpoint.
     */
    @Throws(PaykitFfiException::class)
    public fun `buildPaymentTarget`(`endpoint`: FfiPaymentEndpointCandidate): FfiPaymentTarget

    public companion object
}




/**
 * Platform-owned Pubky session provider.
 */
public interface FfiSdkPubkySessionProvider {

    /**
     * Load current live Pubky session access, when available.
     */
    @Throws(PaykitFfiException::class)
    public fun `loadSessionAccess`(): FfiPubkySessionAccess?

    /**
     * Report whether unauthenticated public Pubky storage can be used.
     */
    @Throws(PaykitFfiException::class)
    public fun `publicStorageAvailable`(): kotlin.Boolean

    /**
     * Clear platform session access during explicit SDK sign-out.
     */
    @Throws(PaykitFfiException::class)
    public fun `clearSessionAccess`()

    public companion object
}




/**
 * SDK state blob owned by platform storage.
 */
public interface FfiSdkStateBlobInterface {

    /**
     * Export the raw bytes for platform storage.
     */
    public fun `exportBytes`(): kotlin.ByteArray

    public companion object
}




/**
 * Platform-owned durable blob store for SDK state.
 */
public interface FfiSdkStateBlobStore {

    /**
     * Load the current SDK state blob, when one exists.
     */
    @Throws(PaykitFfiException::class)
    public fun `loadStateBlob`(): FfiSdkStateBlobSnapshot?

    /**
     * Atomically save a new SDK state blob.
     *
     * `expected_revision` is `None` when no previous blob was loaded. The
     * platform store should reject the write if the stored revision changed.
     */
    @Throws(PaykitFfiException::class)
    public fun `saveStateBlobAtomically`(`blob`: FfiSdkStateBlob, `expectedRevision`: kotlin.String?): kotlin.String

    public companion object
}




/**
 * Time interval a recurring Payment Proof applies to.
 */
@kotlinx.serialization.Serializable
public data class FfiBillingPeriod (
    /**
     * RFC3339 UTC start timestamp.
     */
    val `startsAt`: kotlin.String,
    /**
     * RFC3339 UTC end timestamp.
     */
    val `endsAt`: kotlin.String
) {
    public companion object
}



/**
 * Result of resolving contact Payment Endpoints.
 */

public data class FfiContactPaymentResolution (
    /**
     * General payment resolution outcome.
     */
    val `status`: FfiContactPaymentResolutionStatus,
    /**
     * Private-payment-specific state for this resolution.
     */
    val `privateState`: FfiContactPaymentResolutionPrivateState,
    /**
     * Payable Payment Endpoints in adapter-preferred order.
     */
    val `payableEndpoints`: List<FfiResolvedPaymentEndpoint>
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`status`,
            this.`privateState`,
            this.`payableEndpoints`,
        )
    }
    public companion object
}



/**
 * Request to resolve payable endpoints for one counterparty.
 */
@kotlinx.serialization.Serializable
public data class FfiContactPaymentResolutionRequest (
    /**
     * Counterparty to pay.
     */
    val `counterparty`: kotlin.String,
    /**
     * Optional amount context used by the payment adapter.
     */
    val `amount`: FfiPaymentAmountContext?,
    /**
     * Include public Payment Endpoints after private candidates.
     */
    val `includePublicEndpoints`: kotlin.Boolean
) {
    public companion object
}



/**
 * Contact display profile resolved by trying Paykit Profile first.
 */
@kotlinx.serialization.Serializable
public data class FfiContactProfileResolution (
    /**
     * Profile owner.
     */
    val `publicKey`: kotlin.String,
    /**
     * Source that produced this profile.
     */
    val `source`: FfiContactProfileSource,
    /**
     * Normalized display name for app contact lists.
     */
    val `displayName`: kotlin.String?,
    /**
     * Normalized image pointer for app contact lists.
     */
    val `imageUri`: kotlin.String?,
    /**
     * Paykit Profile payload when the source is Paykit Profile.
     */
    val `paykitProfile`: FfiPaykitProfile?,
    /**
     * Pubky Profile payload when the source is Pubky Profile.
     */
    val `pubkyProfile`: FfiPubkyProfile?,
    /**
     * Local observation time as RFC3339 text.
     */
    val `fetchedAt`: kotlin.String
) {
    public companion object
}



/**
 * Local SDK contact record.
 */
@kotlinx.serialization.Serializable
public data class FfiContactRecord (
    /**
     * Contact public key.
     */
    val `publicKey`: kotlin.String,
    /**
     * Optional local display label.
     */
    val `label`: kotlin.String?,
    /**
     * Cached public profile, when fetched.
     */
    val `profile`: FfiPaykitProfile?,
    /**
     * Time the cached public profile was fetched as RFC3339 text.
     */
    val `profileFetchedAt`: kotlin.String?,
    /**
     * Time the contact was first saved locally as RFC3339 text.
     */
    val `createdAt`: kotlin.String,
    /**
     * Time the local contact record last changed as RFC3339 text.
     */
    val `updatedAt`: kotlin.String,
    /**
     * Public Contact Marker publication state.
     */
    val `publicContactMarkerStatus`: FfiPublicationStatus,
    /**
     * Time the contact was last published publicly as RFC3339 text.
     */
    val `publicContactPublishedAt`: kotlin.String?,
    /**
     * Time the public contact marker was last removed as RFC3339 text.
     */
    val `publicContactRemovedAt`: kotlin.String?,
    /**
     * Last public contact marker publication/removal error.
     */
    val `publicContactLastError`: kotlin.String?
) {
    public companion object
}



/**
 * Local SDK contact update.
 */
@kotlinx.serialization.Serializable
public data class FfiContactUpdate (
    /**
     * Contact public key.
     */
    val `publicKey`: kotlin.String,
    /**
     * Optional local display label.
     */
    val `label`: kotlin.String?
) {
    public companion object
}



/**
 * Public recovery marker state tracked for one Linked Peer.
 */

public data class FfiEncryptedLinkRecoveryMarkerReport (
    /**
     * Counterparty public key.
     */
    val `counterparty`: kotlin.String,
    /**
     * Current Linked Peer state.
     */
    val `state`: FfiLinkedPeerState,
    /**
     * Locally published recovery attempt id.
     */
    val `localAttemptId`: kotlin.String?,
    /**
     * Creation time for the local marker payload as RFC3339 text.
     */
    val `localMarkerCreatedAt`: kotlin.String?,
    /**
     * Last local marker publish/remove error, when available.
     */
    val `localMarkerLastError`: FfiPrivateOperationError?,
    /**
     * Latest observed counterparty recovery attempt id.
     */
    val `remoteAttemptId`: kotlin.String?,
    /**
     * Time the counterparty marker was observed as RFC3339 text.
     */
    val `remoteMarkerObservedAt`: kotlin.String?,
    /**
     * Whether this operation observed a new counterparty marker.
     */
    val `remoteMarkerChanged`: kotlin.Boolean
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`counterparty`,
            this.`state`,
            this.`localAttemptId`,
            this.`localMarkerCreatedAt`,
            this.`localMarkerLastError`,
            this.`remoteAttemptId`,
            this.`remoteMarkerObservedAt`,
            this.`remoteMarkerChanged`,
        )
    }
    public companion object
}



/**
 * One public endpoint changed during sync.
 */
@kotlinx.serialization.Serializable
public data class FfiEndpointSyncChange (
    /**
     * Payment Endpoint Identifier.
     */
    val `identifier`: kotlin.String,
    /**
     * Resulting local publication status.
     */
    val `status`: FfiPublicationStatus,
    /**
     * Error text for failed changes.
     */
    val `error`: kotlin.String?
) {
    public companion object
}



/**
 * Summary returned after public Payment Endpoint sync.
 */
@kotlinx.serialization.Serializable
public data class FfiEndpointSyncReport (
    /**
     * Endpoints successfully published or updated.
     */
    val `published`: List<FfiEndpointSyncChange>,
    /**
     * Endpoints successfully removed.
     */
    val `removed`: List<FfiEndpointSyncChange>,
    /**
     * Endpoints that failed to publish or remove.
     */
    val `failed`: List<FfiEndpointSyncChange>
) {
    public companion object
}



/**
 * Reused Event ID with a different payload.
 */
@kotlinx.serialization.Serializable
public data class FfiEventIdConflict (
    /**
     * Conflicting Event ID.
     */
    val `eventId`: kotlin.String,
    /**
     * First stream item that used this Event ID.
     */
    val `firstStreamItemId`: kotlin.ULong,
    /**
     * Stream item that reused this Event ID with a different payload.
     */
    val `conflictingStreamItemId`: kotlin.ULong
) {
    public companion object
}



/**
 * Current identity status returned to apps.
 */
@kotlinx.serialization.Serializable
public data class FfiIdentityStatus (
    /**
     * Current local public key, when signed in.
     */
    val `publicKey`: kotlin.String?,
    /**
     * Current Pubky capability.
     */
    val `capability`: FfiPubkyIdentityCapability,
    /**
     * Whether live Pubky session access is available for this identity.
     */
    val `liveSessionAvailable`: kotlin.Boolean,
    /**
     * Whether private Paykit workflows can run with the live session.
     */
    val `privateLinkCapable`: kotlin.Boolean
) {
    public companion object
}



/**
 * Initialization report returned after SDK startup.
 */
@kotlinx.serialization.Serializable
public data class FfiInitializationReport (
    /**
     * Last persisted identity status.
     */
    val `identity`: FfiIdentityStatus,
    /**
     * Whether live Pubky session access was available during startup.
     */
    val `liveSessionAvailable`: kotlin.Boolean
) {
    public companion object
}



/**
 * Result of starting or advancing an Encrypted Link Handshake.
 */
@kotlinx.serialization.Serializable
public data class FfiLinkedPeerHandshakeReport (
    /**
     * Counterparty public key.
     */
    val `counterparty`: kotlin.String,
    /**
     * Current Linked Peer state after the operation.
     */
    val `state`: FfiLinkedPeerState,
    /**
     * Current Encrypted Link state generation.
     */
    val `generation`: kotlin.ULong,
    /**
     * In-progress handshake role, when a handshake remains pending.
     */
    val `handshakeRole`: FfiEncryptedLinkHandshakeRole?
) {
    public companion object
}



/**
 * Locally tracked Linked Peer record.
 */

public data class FfiLinkedPeerRecord (
    /**
     * Counterparty public key.
     */
    val `counterparty`: kotlin.String,
    /**
     * Current local relationship/link state.
     */
    val `state`: FfiLinkedPeerState,
    /**
     * Last successful sync time as RFC3339 text.
     */
    val `lastSyncAt`: kotlin.String?,
    /**
     * Last private receive time as RFC3339 text.
     */
    val `lastPrivateReceiveAt`: kotlin.String?,
    /**
     * Consecutive failure count for recovery/retry policy.
     */
    val `failureCount`: kotlin.UInt,
    /**
     * Locally published Encrypted Link recovery attempt id.
     */
    val `localRecoveryAttemptId`: kotlin.String?,
    /**
     * Creation time for the local recovery marker payload as RFC3339 text.
     */
    val `localRecoveryMarkerCreatedAt`: kotlin.String?,
    /**
     * Last local marker publish/remove error, when available.
     */
    val `localRecoveryMarkerLastError`: FfiPrivateOperationError?,
    /**
     * Latest counterparty recovery attempt id already observed.
     */
    val `remoteRecoveryAttemptId`: kotlin.String?,
    /**
     * Time the counterparty recovery marker was observed as RFC3339 text.
     */
    val `remoteRecoveryMarkerObservedAt`: kotlin.String?
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`counterparty`,
            this.`state`,
            this.`lastSyncAt`,
            this.`lastPrivateReceiveAt`,
            this.`failureCount`,
            this.`localRecoveryAttemptId`,
            this.`localRecoveryMarkerCreatedAt`,
            this.`localRecoveryMarkerLastError`,
            this.`remoteRecoveryAttemptId`,
            this.`remoteRecoveryMarkerObservedAt`,
        )
    }
    public companion object
}



/**
 * Summary for processing outbound private messages for one counterparty.
 */

public data class FfiOutboundPrivateCounterpartySendReport (
    /**
     * Counterparty whose queue was processed.
     */
    val `counterparty`: kotlin.String,
    /**
     * Successful send report, when processing completed.
     */
    val `report`: FfiOutboundPrivateSendReport?,
    /**
     * Error text, when processing failed for this counterparty.
     */
    val `error`: FfiPrivateOperationError?
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`counterparty`,
            this.`report`,
            this.`error`,
        )
    }
    public companion object
}



/**
 * Failed outbound private send attempt.
 */

public data class FfiOutboundPrivateSendFailure (
    /**
     * Outbound message id.
     */
    val `outboundMessageId`: kotlin.ULong,
    /**
     * Error from the send attempt.
     */
    val `error`: FfiPrivateOperationError
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`outboundMessageId`,
            this.`error`,
        )
    }
    public companion object
}



/**
 * Summary returned after processing outbound private messages.
 */

public data class FfiOutboundPrivateSendReport (
    /**
     * Messages attempted in this run.
     */
    val `attempted`: List<kotlin.ULong>,
    /**
     * Messages marked sent in this run.
     */
    val `sent`: List<kotlin.ULong>,
    /**
     * Messages that failed in this run.
     */
    val `failed`: List<FfiOutboundPrivateSendFailure>,
    /**
     * Superseded reservation cleanup failures observed in this run.
     */
    val `reservationCleanupFailures`: List<FfiReservationCleanupFailure>,
    /**
     * Recovery marker publication failures observed after fail-closed recovery.
     */
    val `recoveryMarkerFailures`: List<FfiRecoveryMarkerPublishFailure>
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`attempted`,
            this.`sent`,
            this.`failed`,
            this.`reservationCleanupFailures`,
            this.`recoveryMarkerFailures`,
        )
    }
    public companion object
}



/**
 * Public blob published under the configured Paykit namespace.
 */
@kotlinx.serialization.Serializable
public data class FfiPaykitBlobRecord (
    /**
     * Blob owner.
     */
    val `publicKey`: kotlin.String,
    /**
     * Pubky path used for the blob.
     */
    val `path`: kotlin.String,
    /**
     * Canonical `pubky://` URI for the blob.
     */
    val `uri`: kotlin.String,
    /**
     * Blob size in bytes.
     */
    val `sizeBytes`: kotlin.ULong,
    /**
     * Local publication time as RFC3339 text.
     */
    val `updatedAt`: kotlin.String
) {
    public companion object
}



/**
 * Public Paykit-facing profile metadata.
 */
@kotlinx.serialization.Serializable
public data class FfiPaykitProfile (
    /**
     * Public display name.
     */
    val `displayName`: kotlin.String?,
    /**
     * Public image pointer such as a Pubky path or URL.
     */
    val `imageUri`: kotlin.String?,
    /**
     * App-specific public profile fields encoded as a JSON object.
     */
    val `extraJson`: kotlin.String?
) {
    public companion object
}



/**
 * Profile record fetched or published through the SDK.
 */
@kotlinx.serialization.Serializable
public data class FfiPaykitProfileRecord (
    /**
     * Profile owner.
     */
    val `publicKey`: kotlin.String,
    /**
     * Public profile metadata.
     */
    val `profile`: FfiPaykitProfile,
    /**
     * Pubky path used for the profile.
     */
    val `path`: kotlin.String,
    /**
     * Local observation/publication time as RFC3339 text.
     */
    val `updatedAt`: kotlin.String
) {
    public companion object
}



/**
 * Runtime configuration for Paykit SDK bindings.
 */
@kotlinx.serialization.Serializable
public data class FfiPaykitSdkConfig (
    /**
     * Namespace segment for SDK profile/contact public data under `/pub/`.
     */
    val `profileNamespace`: kotlin.String,
    /**
     * Public endpoint management scope.
     */
    val `endpointManagementScope`: FfiEndpointManagementScope,
    /**
     * Public recovery marker behavior.
     */
    val `encryptedLinkRecoveryMarkers`: FfiEncryptedLinkRecoveryMarkerPolicy,
    /**
     * Public contact marker behavior.
     */
    val `publicContactSharing`: FfiPublicContactSharingPolicy,
    /**
     * Peer link operation lease timeout in seconds.
     */
    val `peerLinkOperationLeaseTimeoutSecs`: kotlin.ULong,
    /**
     * Outbound private send lease timeout in seconds.
     */
    val `outboundPrivateSendLeaseTimeoutSecs`: kotlin.ULong,
    /**
     * Minimum delay before retrying a failed outbound private send in seconds.
     */
    val `outboundPrivateRetryBackoffSecs`: kotlin.ULong
) {
    public companion object
}



/**
 * Optional amount context for endpoint selection.
 */
@kotlinx.serialization.Serializable
public data class FfiPaymentAmountContext (
    /**
     * Decimal amount text.
     */
    val `value`: kotlin.String,
    /**
     * Asset code or unit.
     */
    val `asset`: kotlin.String
) {
    public companion object
}



/**
 * Candidate endpoint passed to the payment adapter.
 */

public data class FfiPaymentEndpointCandidate (
    /**
     * Opaque candidate id for this callback request.
     */
    val `candidateId`: kotlin.String,
    /**
     * Counterparty that published the endpoint.
     */
    val `counterparty`: kotlin.String,
    /**
     * Where the endpoint was discovered.
     */
    val `source`: FfiPaymentEndpointSource,
    /**
     * Payment Endpoint Identifier string.
     */
    val `identifier`: kotlin.String,
    /**
     * Serialized endpoint payload.
     */
    val `payload`: FfiPaymentPayload
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`candidateId`,
            this.`counterparty`,
            this.`source`,
            this.`identifier`,
            this.`payload`,
        )
    }
    public companion object
}



/**
 * Receiving detail reserved by the payment adapter.
 */

public data class FfiPaymentEndpointReservation (
    /**
     * Adapter-stable reservation id.
     */
    val `reservationId`: kotlin.String,
    /**
     * Reserved receiving detail.
     */
    val `receivingDetail`: FfiReceivingDetail,
    /**
     * Optional reservation expiry as RFC3339 text.
     */
    val `expiresAt`: kotlin.String?,
    /**
     * Adapter attribution metadata.
     */
    val `attribution`: FfiReservationAttribution
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`reservationId`,
            this.`receivingDetail`,
            this.`expiresAt`,
            this.`attribution`,
        )
    }
    public companion object
}



/**
 * Request passed to cancel a receiving-detail reservation.
 */

public data class FfiPaymentEndpointReservationCancellation (
    /**
     * Adapter-stable reservation id.
     */
    val `reservationId`: kotlin.String,
    /**
     * Counterparty the reservation was intended for.
     */
    val `counterparty`: kotlin.String,
    /**
     * Payment Endpoint Identifier.
     */
    val `identifier`: kotlin.String,
    /**
     * Hash of the reserved endpoint payload.
     */
    val `payloadHash`: kotlin.String,
    /**
     * Adapter attribution metadata from the reservation.
     */
    val `attribution`: FfiReservationAttribution
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`reservationId`,
            this.`counterparty`,
            this.`identifier`,
            this.`payloadHash`,
            this.`attribution`,
        )
    }
    public companion object
}



/**
 * Request passed to the payment adapter for payable endpoint ordering.
 */

public data class FfiPaymentEndpointSelectionRequest (
    /**
     * Counterparty being paid.
     */
    val `counterparty`: kotlin.String,
    /**
     * Optional amount context.
     */
    val `amount`: FfiPaymentAmountContext?,
    /**
     * Candidate endpoints in SDK preference order.
     */
    val `candidates`: List<FfiPaymentEndpointCandidate>
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`counterparty`,
            this.`amount`,
            this.`candidates`,
        )
    }
    public companion object
}



/**
 * Payment Proof captured in a derived Payment Request record.
 */

public data class FfiPaymentProofRecord (
    /**
     * Event ID.
     */
    val `eventId`: kotlin.String,
    /**
     * Outbound message id, when proof was sent locally.
     */
    val `outboundMessageId`: kotlin.ULong?,
    /**
     * Local outbound delivery status, when proof was queued locally.
     */
    val `outboundStatus`: FfiOutboundPrivateMessageStatus?,
    /**
     * Stream item id, when proof was received from the counterparty.
     */
    val `streamItemId`: kotlin.ULong?,
    /**
     * Payment Reference copied from the proof.
     */
    val `paymentReference`: FfiPaymentReference,
    /**
     * Optional Billing Period copied from the proof.
     */
    val `billingPeriod`: FfiBillingPeriod?,
    /**
     * Payment Endpoint Identifier used for payment.
     */
    val `paymentEndpointIdentifier`: kotlin.String,
    /**
     * Method-specific proof object encoded as JSON.
     */
    val `proofJson`: kotlin.String,
    /**
     * Local record time for this proof as RFC3339 text.
     */
    val `recordedAt`: kotlin.String
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`eventId`,
            this.`outboundMessageId`,
            this.`outboundStatus`,
            this.`streamItemId`,
            this.`paymentReference`,
            this.`billingPeriod`,
            this.`paymentEndpointIdentifier`,
            this.`proofJson`,
            this.`recordedAt`,
        )
    }
    public companion object
}



/**
 * Method-specific Payment Proof submission data.
 */
@kotlinx.serialization.Serializable
public data class FfiPaymentProofSubmission (
    /**
     * Billing Period for recurring Payment Requests.
     */
    val `billingPeriod`: FfiBillingPeriod?,
    /**
     * Payment Endpoint Identifier used for payment.
     */
    val `paymentEndpointIdentifier`: kotlin.String,
    /**
     * Method-specific proof object encoded as JSON.
     */
    val `proofJson`: kotlin.String
) {
    public companion object
}



/**
 * Payment Amount fields used by Payment Requests.
 */
@kotlinx.serialization.Serializable
public data class FfiPaymentRequestAmount (
    /**
     * Decimal amount text.
     */
    val `value`: kotlin.String,
    /**
     * Asset code or unit.
     */
    val `asset`: kotlin.String
) {
    public companion object
}



/**
 * Filter for listing Payment Requests.
 */
@kotlinx.serialization.Serializable
public data class FfiPaymentRequestFilter (
    /**
     * Restrict results to one counterparty.
     */
    val `counterparty`: kotlin.String?,
    /**
     * Restrict results to one local role.
     */
    val `localRole`: FfiPaymentRequestLocalRole?,
    /**
     * Restrict results to lifecycle states. Empty means all states.
     */
    val `states`: List<FfiPaymentRequestLifecycleState>,
    /**
     * Restrict results by whether the request has recurrence terms.
     */
    val `recurring`: kotlin.Boolean?,
    /**
     * Include only inbound Payment Requests received from counterparties.
     */
    val `receivedOnly`: kotlin.Boolean
) {
    public companion object
}



/**
 * SDK-derived Payment Request lifecycle record.
 */

public data class FfiPaymentRequestRecord (
    /**
     * Counterparty associated with the private stream.
     */
    val `counterparty`: kotlin.String,
    /**
     * Stable Payment Request ID.
     */
    val `paymentRequestId`: kotlin.String,
    /**
     * Local role, when known.
     */
    val `localRole`: FfiPaymentRequestLocalRole?,
    /**
     * Derived local lifecycle state.
     */
    val `state`: FfiPaymentRequestLifecycleState,
    /**
     * Stream item id of the proposal event.
     */
    val `proposalStreamItemId`: kotlin.ULong?,
    /**
     * Outbound message id of the proposal event.
     */
    val `proposalOutboundMessageId`: kotlin.ULong?,
    /**
     * Local outbound delivery status for the proposal event.
     */
    val `proposalOutboundStatus`: FfiOutboundPrivateMessageStatus?,
    /**
     * Proposal Event ID.
     */
    val `proposalEventId`: kotlin.String?,
    /**
     * Immutable terms from the proposal.
     */
    val `terms`: FfiPaymentRequestTerms?,
    /**
     * Acceptance Event ID.
     */
    val `acceptedEventId`: kotlin.String?,
    /**
     * Local outbound delivery status for an acceptance event.
     */
    val `acceptedOutboundStatus`: FfiOutboundPrivateMessageStatus?,
    /**
     * Rejection Event ID.
     */
    val `rejectedEventId`: kotlin.String?,
    /**
     * Local outbound delivery status for a rejection event.
     */
    val `rejectedOutboundStatus`: FfiOutboundPrivateMessageStatus?,
    /**
     * Cancellation Event ID.
     */
    val `canceledEventId`: kotlin.String?,
    /**
     * Local outbound delivery status for a cancellation event.
     */
    val `canceledOutboundStatus`: FfiOutboundPrivateMessageStatus?,
    /**
     * Payment Proof records in local record order.
     */
    val `paymentProofs`: List<FfiPaymentProofRecord>,
    /**
     * Last inbound stream item applied to this record.
     */
    val `lastStreamItemId`: kotlin.ULong?,
    /**
     * Last outbound message applied to this record.
     */
    val `lastOutboundMessageId`: kotlin.ULong?,
    /**
     * Local delivery status of the last outbound message applied to this record.
     */
    val `lastOutboundStatus`: FfiOutboundPrivateMessageStatus?,
    /**
     * Last event local record time as RFC3339 text.
     */
    val `lastEventAt`: kotlin.String?,
    /**
     * Invalid state reason, when available.
     */
    val `invalidReason`: kotlin.String?
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`counterparty`,
            this.`paymentRequestId`,
            this.`localRole`,
            this.`state`,
            this.`proposalStreamItemId`,
            this.`proposalOutboundMessageId`,
            this.`proposalOutboundStatus`,
            this.`proposalEventId`,
            this.`terms`,
            this.`acceptedEventId`,
            this.`acceptedOutboundStatus`,
            this.`rejectedEventId`,
            this.`rejectedOutboundStatus`,
            this.`canceledEventId`,
            this.`canceledOutboundStatus`,
            this.`paymentProofs`,
            this.`lastStreamItemId`,
            this.`lastOutboundMessageId`,
            this.`lastOutboundStatus`,
            this.`lastEventAt`,
            this.`invalidReason`,
        )
    }
    public companion object
}



/**
 * Recurrence fields for a recurring Payment Request.
 */
@kotlinx.serialization.Serializable
public data class FfiPaymentRequestRecurrence (
    /**
     * Positive interval count.
     */
    val `every`: kotlin.UInt,
    /**
     * Unit string: minute, hour, day, week, month, or year.
     */
    val `unit`: kotlin.String,
    /**
     * RFC3339 UTC timestamp using `Z`.
     */
    val `startsAt`: kotlin.String,
    /**
     * RFC3339 UTC timestamp using `Z`.
     */
    val `anchor`: kotlin.String,
    /**
     * Optional RFC3339 UTC timestamp using `Z`.
     */
    val `endsAt`: kotlin.String?
) {
    public companion object
}



/**
 * Immutable terms for a Payment Request proposal.
 */

public data class FfiPaymentRequestTerms (
    /**
     * Requested amount.
     */
    val `amount`: FfiPaymentRequestAmount,
    /**
     * Payee-provided payment correlation value.
     */
    val `paymentReference`: FfiPaymentReference,
    /**
     * Proposal expiry before acceptance.
     */
    val `proposalExpiresAt`: kotlin.String?,
    /**
     * Optional recurrence.
     */
    val `recurrence`: FfiPaymentRequestRecurrence?,
    /**
     * Accepted Payment Endpoint Identifier strings.
     */
    val `acceptedPaymentEndpointIdentifiers`: List<kotlin.String>,
    /**
     * Application-specific metadata encoded as a JSON object.
     */
    val `metadataJson`: kotlin.String
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`amount`,
            this.`paymentReference`,
            this.`proposalExpiresAt`,
            this.`recurrence`,
            this.`acceptedPaymentEndpointIdentifiers`,
            this.`metadataJson`,
        )
    }
    public companion object
}



/**
 * Payment-method-specific execution payload produced by the adapter.
 */

public data class FfiPaymentTarget (
    /**
     * Method-specific target payload.
     */
    val `payload`: FfiPaymentPayload
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`payload`,
        )
    }
    public companion object
}



/**
 * One endpoint in the latest Private Payment List view.
 */

public data class FfiPrivatePaymentListEndpoint (
    /**
     * Payment Endpoint Identifier string.
     */
    val `identifier`: kotlin.String,
    /**
     * Serialized endpoint payload.
     */
    val `payload`: FfiPaymentPayload
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`identifier`,
            this.`payload`,
        )
    }
    public companion object
}



/**
 * Latest valid Private Payment List view for one counterparty.
 */

public data class FfiPrivatePaymentListView (
    /**
     * Stream item id of the latest valid list.
     */
    val `latestStreamItemId`: kotlin.ULong?,
    /**
     * Current endpoint payloads sorted by identifier.
     */
    val `paymentEndpoints`: List<FfiPrivatePaymentListEndpoint>,
    /**
     * Receive time of the latest valid list as RFC3339 text.
     */
    val `lastRefreshAt`: kotlin.String?
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`latestStreamItemId`,
            this.`paymentEndpoints`,
            this.`lastRefreshAt`,
        )
    }
    public companion object
}



/**
 * Summary for receiving private messages from one counterparty.
 */

public data class FfiPrivateStreamCounterpartyIntakeReport (
    /**
     * Counterparty whose private stream was received.
     */
    val `counterparty`: kotlin.String,
    /**
     * Successful intake report, when receive completed.
     */
    val `report`: FfiPrivateStreamIntakeReport?,
    /**
     * Error text, when receive failed for this counterparty.
     */
    val `error`: FfiPrivateOperationError?
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`counterparty`,
            this.`report`,
            this.`error`,
        )
    }
    public companion object
}



/**
 * Summary of a persisted private stream batch.
 */
@kotlinx.serialization.Serializable
public data class FfiPrivateStreamIntakeReport (
    /**
     * Receive batch id assigned by storage.
     */
    val `receiveBatchId`: kotlin.ULong,
    /**
     * Stored stream item ids in input order.
     */
    val `streamItemIds`: List<kotlin.ULong>,
    /**
     * Event ID conflicts found while updating dedupe records.
     */
    val `eventConflicts`: List<FfiEventIdConflict>
) {
    public companion object
}



/**
 * Public details parsed from a Pubky auth deep link.
 */
@kotlinx.serialization.Serializable
public data class FfiPubkyAuthDetails (
    /**
     * Auth request kind.
     */
    val `kind`: FfiPubkyAuthRequestKind,
    /**
     * Requested capabilities as canonical Pubky capability text.
     */
    val `capabilities`: kotlin.String?,
    /**
     * Relay URL used by the auth flow.
     */
    val `relayUrl`: kotlin.String?,
    /**
     * Homeserver requested by a signup flow.
     */
    val `homeserverPublicKey`: kotlin.String?
) {
    public companion object
}



/**
 * Pubky client configuration owned by the binding layer.
 */
@kotlinx.serialization.Serializable
public data class FfiPubkyClientConfig (
    /**
     * Request timeout for Pubky HTTP operations in seconds.
     */
    val `requestTimeoutSecs`: kotlin.ULong
) {
    public companion object
}



/**
 * Public profile metadata from the Pubky app namespace.
 */
@kotlinx.serialization.Serializable
public data class FfiPubkyProfile (
    /**
     * Public display name.
     */
    val `name`: kotlin.String,
    /**
     * Optional profile bio.
     */
    val `bio`: kotlin.String?,
    /**
     * Optional public image pointer.
     */
    val `image`: kotlin.String?,
    /**
     * Public profile links.
     */
    val `links`: List<FfiPubkyProfileLink>,
    /**
     * Optional public status text.
     */
    val `status`: kotlin.String?
) {
    public companion object
}



/**
 * Public profile link from the Pubky app namespace.
 */
@kotlinx.serialization.Serializable
public data class FfiPubkyProfileLink (
    /**
     * Link title.
     */
    val `title`: kotlin.String,
    /**
     * Link URL.
     */
    val `url`: kotlin.String
) {
    public companion object
}



/**
 * Public profile record fetched from the Pubky app namespace.
 */
@kotlinx.serialization.Serializable
public data class FfiPubkyProfileRecord (
    /**
     * Profile owner.
     */
    val `publicKey`: kotlin.String,
    /**
     * Public profile metadata.
     */
    val `profile`: FfiPubkyProfile,
    /**
     * Pubky path used for the profile.
     */
    val `path`: kotlin.String,
    /**
     * Local observation time as RFC3339 text.
     */
    val `fetchedAt`: kotlin.String
) {
    public companion object
}



/**
 * Parsed Pubky resource with a normalized owner and path.
 */
@kotlinx.serialization.Serializable
public data class FfiPubkyResourceRef (
    /**
     * Resource owner public key.
     */
    val `publicKey`: kotlin.String,
    /**
     * Absolute resource path.
     */
    val `path`: kotlin.String,
    /**
     * Transport URL resolved by the Pubky client.
     */
    val `transportUrl`: kotlin.String
) {
    public companion object
}



/**
 * Result of creating or importing a Pubky session.
 */

public data class FfiPubkySessionBootstrapResult (
    /**
     * Session access material to persist in platform session storage.
     */
    val `sessionAccess`: FfiPubkySessionAccess,
    /**
     * Local Pubky public key.
     */
    val `publicKey`: kotlin.String,
    /**
     * Capability implied by the session and optional local secret key.
     */
    val `capability`: FfiPubkyIdentityCapability
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`sessionAccess`,
            this.`publicKey`,
            this.`capability`,
        )
    }
    public companion object
}



/**
 * Queued outbound private message summary.
 */

public data class FfiQueuedPrivateMessage (
    /**
     * Assigned outbound message id.
     */
    val `outboundMessageId`: kotlin.ULong,
    /**
     * Counterparty public key.
     */
    val `counterparty`: kotlin.String,
    /**
     * Private Message Kind string.
     */
    val `kind`: kotlin.String,
    /**
     * Delivery status.
     */
    val `status`: FfiOutboundPrivateMessageStatus,
    /**
     * Number of send attempts.
     */
    val `attemptCount`: kotlin.UInt,
    /**
     * Queue time as RFC3339 text.
     */
    val `createdAt`: kotlin.String,
    /**
     * Last status update time as RFC3339 text.
     */
    val `updatedAt`: kotlin.String,
    /**
     * Last send attempt time as RFC3339 text.
     */
    val `lastAttemptAt`: kotlin.String?,
    /**
     * Successful send time as RFC3339 text.
     */
    val `sentAt`: kotlin.String?,
    /**
     * Last send error, when available.
     */
    val `lastError`: FfiPrivateOperationError?
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`outboundMessageId`,
            this.`counterparty`,
            this.`kind`,
            this.`status`,
            this.`attemptCount`,
            this.`createdAt`,
            this.`updatedAt`,
            this.`lastAttemptAt`,
            this.`sentAt`,
            this.`lastError`,
        )
    }
    public companion object
}



/**
 * App-facing view of an indexed Receipt Access event.
 */

public data class FfiReceiptAccessView (
    /**
     * Counterparty that sent the Receipt Access event.
     */
    val `counterparty`: kotlin.String,
    /**
     * Receipt Access Event ID.
     */
    val `eventId`: kotlin.String,
    /**
     * Receipt ID.
     */
    val `receiptId`: kotlin.String,
    /**
     * Payment Reference copied from Receipt Access.
     */
    val `paymentReference`: FfiPaymentReference,
    /**
     * Optional Payment Request ID copied from Receipt Access.
     */
    val `paymentRequestId`: kotlin.String?,
    /**
     * Optional Billing Period copied from Receipt Access.
     */
    val `billingPeriod`: FfiBillingPeriod?,
    /**
     * Current retrieval state for the referenced receipt.
     */
    val `retrievalStatus`: FfiReceiptRetrievalStatus,
    /**
     * Last retrieval attempt time as RFC3339 text.
     */
    val `retrievalAttemptedAt`: kotlin.String?,
    /**
     * Successful retrieval/decryption time as RFC3339 text.
     */
    val `retrievedAt`: kotlin.String?,
    /**
     * Receive time of the indexed stream item as RFC3339 text.
     */
    val `receivedAt`: kotlin.String
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`counterparty`,
            this.`eventId`,
            this.`receiptId`,
            this.`paymentReference`,
            this.`paymentRequestId`,
            this.`billingPeriod`,
            this.`retrievalStatus`,
            this.`retrievalAttemptedAt`,
            this.`retrievedAt`,
            this.`receivedAt`,
        )
    }
    public companion object
}



/**
 * Payment Amount fields copied into receipts.
 */
@kotlinx.serialization.Serializable
public data class FfiReceiptAmount (
    /**
     * Decimal amount text.
     */
    val `value`: kotlin.String,
    /**
     * Asset code or unit.
     */
    val `asset`: kotlin.String
) {
    public companion object
}



/**
 * Caller-provided receipt fields.
 */

public data class FfiReceiptDraft (
    /**
     * Optional caller-stable Receipt ID.
     */
    val `receiptId`: kotlin.String?,
    /**
     * Payment Reference being receipted.
     */
    val `paymentReference`: FfiPaymentReference,
    /**
     * Optional Payment Request ID this receipt corresponds to.
     */
    val `paymentRequestId`: kotlin.String?,
    /**
     * Optional Billing Period for recurring Payment Request receipts.
     */
    val `billingPeriod`: FfiBillingPeriod?,
    /**
     * Optional Payment Endpoint Identifier used for the payment.
     */
    val `paymentEndpointIdentifier`: kotlin.String?,
    /**
     * Optional Payment Amount being receipted.
     */
    val `amount`: FfiReceiptAmount?,
    /**
     * Caller-defined Receipt Metadata encoded as a JSON object.
     */
    val `metadataJson`: kotlin.String
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`receiptId`,
            this.`paymentReference`,
            this.`paymentRequestId`,
            this.`billingPeriod`,
            this.`paymentEndpointIdentifier`,
            this.`amount`,
            this.`metadataJson`,
        )
    }
    public companion object
}



/**
 * App-facing view of local receipt issuance progress.
 */

public data class FfiReceiptIssuanceView (
    /**
     * Counterparty that should receive Receipt Access.
     */
    val `counterparty`: kotlin.String,
    /**
     * Receipt ID.
     */
    val `receiptId`: kotlin.String,
    /**
     * Receipt Access Event ID.
     */
    val `receiptAccessEventId`: kotlin.String,
    /**
     * Payment Reference copied from the Receipt.
     */
    val `paymentReference`: FfiPaymentReference,
    /**
     * Optional Payment Request ID copied from the Receipt.
     */
    val `paymentRequestId`: kotlin.String?,
    /**
     * Optional Billing Period copied from the Receipt.
     */
    val `billingPeriod`: FfiBillingPeriod?,
    /**
     * Optional Payment Endpoint Identifier copied from the Receipt.
     */
    val `paymentEndpointIdentifier`: kotlin.String?,
    /**
     * Optional Payment Amount copied from the Receipt.
     */
    val `amount`: FfiReceiptAmount?,
    /**
     * Current issuance status.
     */
    val `status`: FfiReceiptIssuanceStatus,
    /**
     * Outbound private message id that carries Receipt Access, once queued.
     */
    val `outboundMessageId`: kotlin.ULong?,
    /**
     * Creation time as RFC3339 text.
     */
    val `createdAt`: kotlin.String,
    /**
     * Last status update time as RFC3339 text.
     */
    val `updatedAt`: kotlin.String,
    /**
     * Time the Encrypted Receipt was stored as RFC3339 text.
     */
    val `storedAt`: kotlin.String?,
    /**
     * Time Receipt Access was queued for private delivery as RFC3339 text.
     */
    val `accessQueuedAt`: kotlin.String?
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`counterparty`,
            this.`receiptId`,
            this.`receiptAccessEventId`,
            this.`paymentReference`,
            this.`paymentRequestId`,
            this.`billingPeriod`,
            this.`paymentEndpointIdentifier`,
            this.`amount`,
            this.`status`,
            this.`outboundMessageId`,
            this.`createdAt`,
            this.`updatedAt`,
            this.`storedAt`,
            this.`accessQueuedAt`,
        )
    }
    public companion object
}



/**
 * Decrypted Receipt record stored by the SDK.
 */

public data class FfiReceiptRecord (
    /**
     * Counterparty that issued the Receipt Access event.
     */
    val `issuer`: kotlin.String,
    /**
     * Receipt Access Event ID used for retrieval.
     */
    val `receiptAccessEventId`: kotlin.String,
    /**
     * Receipt ID.
     */
    val `receiptId`: kotlin.String,
    /**
     * Payment Reference copied from the decrypted Receipt.
     */
    val `paymentReference`: FfiPaymentReference,
    /**
     * Optional Payment Request ID copied from the decrypted Receipt.
     */
    val `paymentRequestId`: kotlin.String?,
    /**
     * Optional Billing Period copied from the decrypted Receipt.
     */
    val `billingPeriod`: FfiBillingPeriod?,
    /**
     * Recipient public key from the decrypted Receipt.
     */
    val `recipientPublicKey`: kotlin.String,
    /**
     * Optional Payment Endpoint Identifier copied from the decrypted Receipt.
     */
    val `paymentEndpointIdentifier`: kotlin.String?,
    /**
     * Optional Payment Amount copied from the decrypted Receipt.
     */
    val `amount`: FfiReceiptAmount?,
    /**
     * Caller-defined Receipt Metadata encoded as a JSON object.
     */
    val `metadataJson`: kotlin.String,
    /**
     * Successful retrieval/decryption time as RFC3339 text.
     */
    val `retrievedAt`: kotlin.String
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`issuer`,
            this.`receiptAccessEventId`,
            this.`receiptId`,
            this.`paymentReference`,
            this.`paymentRequestId`,
            this.`billingPeriod`,
            this.`recipientPublicKey`,
            this.`paymentEndpointIdentifier`,
            this.`amount`,
            this.`metadataJson`,
            this.`retrievedAt`,
        )
    }
    public companion object
}



/**
 * Payment-method-specific receiving detail returned by the payment adapter.
 */

public data class FfiReceivingDetail (
    /**
     * Payment Endpoint Identifier string.
     */
    val `identifier`: kotlin.String,
    /**
     * Serialized endpoint payload.
     */
    val `payload`: FfiPaymentPayload
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`identifier`,
            this.`payload`,
        )
    }
    public companion object
}



/**
 * Receiving-detail request scope passed to the payment adapter.
 */
@kotlinx.serialization.Serializable
public data class FfiReceivingDetailScope (
    /**
     * Scope kind.
     */
    val `kind`: FfiReceivingDetailScopeKind,
    /**
     * Counterparty public key for private scopes.
     */
    val `counterparty`: kotlin.String?
) {
    public companion object
}



/**
 * Failed recovery marker publication during outbound private send recovery.
 */

public data class FfiRecoveryMarkerPublishFailure (
    /**
     * Outbound message id that triggered recovery, when available.
     */
    val `outboundMessageId`: kotlin.ULong?,
    /**
     * Recovery marker publication error.
     */
    val `error`: FfiPrivateOperationError
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`outboundMessageId`,
            this.`error`,
        )
    }
    public companion object
}



/**
 * Failed cleanup of a superseded Payment Endpoint Reservation.
 */

public data class FfiReservationCleanupFailure (
    /**
     * Reservation id, when the failure is tied to a specific reservation.
     */
    val `reservationId`: kotlin.String?,
    /**
     * Cleanup error.
     */
    val `error`: FfiPrivateOperationError
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`reservationId`,
            this.`error`,
        )
    }
    public companion object
}



/**
 * Payment Endpoint paired with the target needed to pay through it.
 */

public data class FfiResolvedPaymentEndpoint (
    /**
     * Counterparty that published the endpoint.
     */
    val `counterparty`: kotlin.String,
    /**
     * Where the endpoint was discovered.
     */
    val `source`: FfiPaymentEndpointSource,
    /**
     * Payment Endpoint Identifier string.
     */
    val `identifier`: kotlin.String,
    /**
     * Serialized endpoint payload.
     */
    val `payload`: FfiPaymentPayload,
    /**
     * Adapter-built target for executing payment through this endpoint.
     */
    val `target`: FfiPaymentTarget
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`counterparty`,
            this.`source`,
            this.`identifier`,
            this.`payload`,
            this.`target`,
        )
    }
    public companion object
}



/**
 * Report returned after restoring SDK-managed backup state.
 */
@kotlinx.serialization.Serializable
public data class FfiRestoreReport (
    /**
     * Restored backup schema version.
     */
    val `version`: kotlin.UInt,
    /**
     * Whether identity state was restored.
     */
    val `restoredIdentity`: kotlin.Boolean,
    /**
     * Number of restored Linked Peer records.
     */
    val `linkedPeers`: kotlin.ULong,
    /**
     * Number of restored local contact records.
     */
    val `contactRecords`: kotlin.ULong,
    /**
     * Number of restored public Payment Endpoint records.
     */
    val `publicEndpointRecords`: kotlin.ULong,
    /**
     * Number of restored Payment Endpoint Reservation records.
     */
    val `paymentEndpointReservations`: kotlin.ULong,
    /**
     * Number of restored Encrypted Link state records.
     */
    val `encryptedLinkStates`: kotlin.ULong,
    /**
     * Number of restored outbound Private Application Message records.
     */
    val `outboundPrivateMessages`: kotlin.ULong,
    /**
     * Number of restored private stream item records.
     */
    val `privateStreamItems`: kotlin.ULong,
    /**
     * Number of restored Event Message dedupe records.
     */
    val `eventDedupRecords`: kotlin.ULong,
    /**
     * Number of restored Receipt Access records.
     */
    val `receiptAccessRecords`: kotlin.ULong,
    /**
     * Number of restored decrypted Receipt records.
     */
    val `receiptRecords`: kotlin.ULong,
    /**
     * Number of restored local receipt issuance records.
     */
    val `receiptIssuanceRecords`: kotlin.ULong,
    /**
     * Counterparties restored as recovery-required.
     */
    val `recoveryRequiredPeers`: List<kotlin.String>
) {
    public companion object
}



/**
 * Current SDK state blob with its platform storage revision.
 */

public data class FfiSdkStateBlobSnapshot (
    /**
     * Encoded SDK state.
     */
    val `blob`: FfiSdkStateBlob,
    /**
     * Opaque platform storage revision for optimistic writes.
     */
    val `revision`: kotlin.String
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`blob`,
            this.`revision`,
        )
    }
    public companion object
}




/**
 * Private-payment state observed while resolving a contact payment.
 */

@kotlinx.serialization.Serializable
public enum class FfiContactPaymentResolutionPrivateState {

    /**
     * Private Payment List candidates were available for resolution.
     */
    AVAILABLE,
    /**
     * No Private Payment List candidate was available.
     */
    NO_PRIVATE_ENDPOINT,
    /**
     * Private payment state is blocked by link recovery.
     */
    RECOVERY_PENDING,
    /**
     * The local identity cannot establish private links.
     */
    PUBLIC_ONLY_SESSION,
    /**
     * SDK returned a value this binding version does not understand.
     */
    UNKNOWN;
    public companion object
}






/**
 * Result category for contact payment resolution.
 */

@kotlinx.serialization.Serializable
public enum class FfiContactPaymentResolutionStatus {

    /**
     * A payable endpoint was found.
     */
    PAYABLE,
    /**
     * No endpoint was found.
     */
    NO_ENDPOINT,
    /**
     * Endpoints exist but are unsupported.
     */
    UNSUPPORTED_ENDPOINT,
    /**
     * SDK returned a value this binding version does not understand.
     */
    UNKNOWN;
    public companion object
}






/**
 * Source used for a resolved contact profile.
 */

@kotlinx.serialization.Serializable
public enum class FfiContactProfileSource {

    /**
     * Resolved from the configured Paykit Profile path.
     */
    PAYKIT_PROFILE,
    /**
     * Resolved from the Pubky app profile path.
     */
    PUBKY_PROFILE,
    /**
     * SDK returned a value this binding version does not understand.
     */
    UNKNOWN;
    public companion object
}






/**
 * Local role for an in-progress Encrypted Link Handshake.
 */

@kotlinx.serialization.Serializable
public enum class FfiEncryptedLinkHandshakeRole {

    /**
     * Local peer initiated the handshake.
     */
    INITIATOR,
    /**
     * Local peer accepted a handshake initiated by the counterparty.
     */
    RESPONDER,
    /**
     * SDK returned a value this binding version does not understand.
     */
    UNKNOWN;
    public companion object
}






/**
 * SDK policy for public Encrypted Link recovery markers.
 */

@kotlinx.serialization.Serializable
public enum class FfiEncryptedLinkRecoveryMarkerPolicy {

    /**
     * Publish and observe recovery markers.
     */
    ENABLED,
    /**
     * Do not use recovery markers.
     */
    DISABLED,
    /**
     * SDK returned a value this binding version does not understand.
     */
    UNKNOWN;
    public companion object
}






/**
 * SDK policy for public Payment Endpoint cleanup.
 */

@kotlinx.serialization.Serializable
public enum class FfiEndpointManagementScope {

    /**
     * Manage only endpoints previously published by the SDK.
     */
    MANAGED_ONLY,
    /**
     * Manage the full local Paykit public namespace.
     */
    FULL_PAYKIT_NAMESPACE,
    /**
     * SDK returned a value this binding version does not understand.
     */
    UNKNOWN;
    public companion object
}






/**
 * Local relationship state for a counterparty.
 */

@kotlinx.serialization.Serializable
public enum class FfiLinkedPeerState {

    /**
     * The SDK tracks this counterparty, but no active Encrypted Link exists.
     */
    NOT_LINKED,
    /**
     * An Encrypted Link Handshake is in progress.
     */
    LINKING,
    /**
     * An Encrypted Link is established.
     */
    LINKED,
    /**
     * Local state cannot safely continue without recovery.
     */
    RECOVERY_REQUIRED,
    /**
     * Local policy blocks this peer.
     */
    BLOCKED,
    /**
     * SDK returned a value this binding version does not understand.
     */
    UNKNOWN;
    public companion object
}






/**
 * Delivery status for one queued outbound Private Application Message.
 */

@kotlinx.serialization.Serializable
public enum class FfiOutboundPrivateMessageStatus {

    /**
     * Message is queued and has not been sent.
     */
    PENDING,
    /**
     * A worker is sending this message.
     */
    SENDING,
    /**
     * Message was sent successfully.
     */
    SENT,
    /**
     * Last send attempt failed.
     */
    FAILED,
    /**
     * The stored payload is invalid and must not be retried automatically.
     */
    INVALID,
    /**
     * Automatic retry is blocked until local Encrypted Link state is recovered.
     */
    RECOVERY_REQUIRED,
    /**
     * Newer latest-state data made this message unnecessary to send.
     */
    SUPERSEDED,
    /**
     * SDK returned a value this binding version does not understand.
     */
    UNKNOWN;
    public companion object
}






/**
 * Source of a discovered Payment Endpoint candidate.
 */

@kotlinx.serialization.Serializable
public enum class FfiPaymentEndpointSource {

    /**
     * Endpoint came from a counterparty-specific Private Payment List.
     */
    PRIVATE_PAYMENT_LIST,
    /**
     * Endpoint came from a public Payment Endpoint.
     */
    PUBLIC_PAYMENT_ENDPOINT,
    /**
     * SDK returned a value this binding version does not understand.
     */
    UNKNOWN;
    public companion object
}






/**
 * SDK-derived Payment Request lifecycle state.
 */

@kotlinx.serialization.Serializable
public enum class FfiPaymentRequestLifecycleState {

    /**
     * Proposal is known locally and remains actionable.
     */
    PROPOSED,
    /**
     * Proposal is past its expiry.
     */
    PROPOSAL_EXPIRED,
    /**
     * Acceptance is present locally.
     */
    ACCEPTED,
    /**
     * Rejection is present locally.
     */
    REJECTED,
    /**
     * Cancellation is present locally.
     */
    CANCELED,
    /**
     * A one-time Payment Proof is present locally.
     */
    PROOF_SUBMITTED,
    /**
     * Recurring request acceptance is present locally.
     */
    ACTIVE_RECURRING,
    /**
     * A local outbound event may require private-link recovery.
     */
    RECOVERY_REQUIRED,
    /**
     * Event ordering, dedupe, or lifecycle validation found an invalid state.
     */
    INVALID_CONFLICT,
    /**
     * SDK returned a value this binding version does not understand.
     */
    UNKNOWN;
    public companion object
}






/**
 * Local role for one Payment Request.
 */

@kotlinx.serialization.Serializable
public enum class FfiPaymentRequestLocalRole {

    /**
     * Local identity is expected to pay.
     */
    PAYER,
    /**
     * Local identity expects to receive payment.
     */
    PAYEE,
    /**
     * SDK returned a value this binding version does not understand.
     */
    UNKNOWN;
    public companion object
}






/**
 * Kind of Pubky auth request represented by a deep link.
 */

@kotlinx.serialization.Serializable
public enum class FfiPubkyAuthRequestKind {

    /**
     * Sign in to an existing Pubky account.
     */
    SIGN_IN,
    /**
     * Sign up on a Pubky homeserver.
     */
    SIGN_UP,
    /**
     * Export a secret from a signer.
     */
    SECRET_EXPORT,
    /**
     * SDK returned a value this binding version does not understand.
     */
    UNKNOWN;
    public companion object
}






/**
 * Pubky capability state for one app-owned Paykit runtime.
 */

@kotlinx.serialization.Serializable
public enum class FfiPubkyIdentityCapability {

    /**
     * No Pubky identity is initialized, or explicit sign-out completed.
     */
    SIGNED_OUT,
    /**
     * Public Pubky operations may work, but private links cannot be established.
     */
    PUBLIC_ONLY,
    /**
     * Public operations and Encrypted Links can work.
     */
    PRIVATE_LINK_CAPABLE,
    /**
     * SDK returned a value this binding version does not understand.
     */
    UNKNOWN;
    public companion object
}






/**
 * SDK policy for public contact marker publication.
 */

@kotlinx.serialization.Serializable
public enum class FfiPublicContactSharingPolicy {

    /**
     * Keep saved contacts only in local SDK storage.
     */
    LOCAL_ONLY,
    /**
     * Allow explicit public contact marker publication in the configured namespace.
     */
    CONFIGURED_PUBLIC_NAMESPACE,
    /**
     * SDK returned a value this binding version does not understand.
     */
    UNKNOWN;
    public companion object
}






/**
 * Local publication state for SDK-managed public data.
 */

@kotlinx.serialization.Serializable
public enum class FfiPublicationStatus {

    /**
     * No publication is known to exist.
     */
    NOT_PUBLISHED,
    /**
     * Publication was recorded locally before the remote write.
     */
    PENDING_PUBLICATION,
    /**
     * Publication is known to exist.
     */
    PUBLISHED,
    /**
     * Removal was recorded locally before the remote delete.
     */
    PENDING_REMOVAL,
    /**
     * Publication is known to be removed.
     */
    REMOVED,
    /**
     * Last publication or removal attempt failed.
     */
    FAILED,
    /**
     * SDK returned a value this binding version does not understand.
     */
    UNKNOWN;
    public companion object
}






/**
 * Local receipt issuance state.
 */

@kotlinx.serialization.Serializable
public enum class FfiReceiptIssuanceStatus {

    /**
     * Encrypted Receipt has not been stored yet.
     */
    PENDING_STORAGE,
    /**
     * Encrypted Receipt was stored, but Receipt Access has not been queued yet.
     */
    STORED,
    /**
     * Receipt Access was queued for private delivery.
     */
    ACCESS_QUEUED,
    /**
     * Last storage or queueing attempt failed.
     */
    FAILED,
    /**
     * SDK returned a value this binding version does not understand.
     */
    UNKNOWN;
    public companion object
}






/**
 * Receipt retrieval state for an indexed Receipt Access event.
 */

@kotlinx.serialization.Serializable
public enum class FfiReceiptRetrievalStatus {

    /**
     * Receipt Access has been indexed, but retrieval has not succeeded yet.
     */
    PENDING,
    /**
     * Encrypted Receipt was fetched and decrypted.
     */
    RETRIEVED,
    /**
     * Receipt Location was missing on the issuer homeserver.
     */
    NOT_FOUND,
    /**
     * Retrieval or decryption failed.
     */
    FAILED,
    /**
     * SDK returned a value this binding version does not understand.
     */
    UNKNOWN;
    public companion object
}






/**
 * Scope used when asking a payment adapter for receiving details.
 */

@kotlinx.serialization.Serializable
public enum class FfiReceivingDetailScopeKind {

    /**
     * Details intended for public Payment Endpoints.
     */
    PUBLIC,
    /**
     * Details intended for one counterparty's Private Payment List.
     */
    PRIVATE,
    /**
     * SDK returned a value this binding version does not understand.
     */
    UNKNOWN;
    public companion object
}







/**
 * Error type exposed through generated bindings.
 */
public sealed class PaykitFfiException: kotlin.Exception() {

    /**
     * Durable storage failed.
     */
    public class Storage(
        public val `code`: kotlin.String,
        public val `context`: kotlin.String,
    ) : PaykitFfiException() {
        override val message: String
            get() = "code=${ `code` }, context=${ `context` }"
    }

    /**
     * Pubky identity, session, or key capability failed.
     */
    public class Identity(
        public val `code`: kotlin.String,
        public val `context`: kotlin.String,
    ) : PaykitFfiException() {
        override val message: String
            get() = "code=${ `code` }, context=${ `context` }"
    }

    /**
     * Pubky or Encrypted Link transport failed.
     */
    public class Transport(
        public val `code`: kotlin.String,
        public val `context`: kotlin.String,
    ) : PaykitFfiException() {
        override val message: String
            get() = "code=${ `code` }, context=${ `context` }"
    }

    /**
     * Requested Paykit or Pubky resource was not found.
     */
    public class NotFound(
        public val `code`: kotlin.String,
        public val `context`: kotlin.String,
    ) : PaykitFfiException() {
        override val message: String
            get() = "code=${ `code` }, context=${ `context` }"
    }

    /**
     * Paykit protocol data is invalid, conflicting, or unsupported.
     */
    public class Protocol(
        public val `code`: kotlin.String,
        public val `context`: kotlin.String,
    ) : PaykitFfiException() {
        override val message: String
            get() = "code=${ `code` }, context=${ `context` }"
    }

    /**
     * Operation is blocked by configured SDK policy.
     */
    public class Policy(
        public val `code`: kotlin.String,
        public val `context`: kotlin.String,
    ) : PaykitFfiException() {
        override val message: String
            get() = "code=${ `code` }, context=${ `context` }"
    }

    /**
     * Payment adapter failed.
     */
    public class PaymentAdapter(
        public val `code`: kotlin.String,
        public val `context`: kotlin.String,
    ) : PaykitFfiException() {
        override val message: String
            get() = "code=${ `code` }, context=${ `context` }"
    }

    /**
     * Local state needs explicit recovery before automation can continue.
     */
    public class RecoveryRequired(
        public val `code`: kotlin.String,
        public val `context`: kotlin.String,
    ) : PaykitFfiException() {
        override val message: String
            get() = "code=${ `code` }, context=${ `context` }"
    }

}
