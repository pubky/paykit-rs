

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
public interface PaykitSdkInterface {

    /**
     * Start an Encrypted Link Handshake as the responder.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `acceptLinkWithPeer`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): LinkedPeerHandshakeReport

    /**
     * Queue acceptance for a received Payment Request and return local derived state.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `acceptPaymentRequest`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `paymentRequestId`: kotlin.String): PaymentRequestRecord

    /**
     * Return received Payment Requests that need a local payer response.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `actionableReceivedPaymentRequests`(): List<PaymentRequestRecord>

    /**
     * Return accepted recurring Payment Requests across non-blocked counterparties.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `activeRecurringPaymentRequests`(): List<PaymentRequestRecord>

    /**
     * Advance the stored Encrypted Link Handshake for one counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `advanceLinkHandshake`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): LinkedPeerHandshakeReport

    /**
     * Block a counterparty for local Paykit private workflows.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `blockPeer`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): LinkedPeerRecord

    /**
     * Queue cancellation for a known non-terminal Payment Request.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `cancelPaymentRequest`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `paymentRequestId`: kotlin.String, `reason`: kotlin.String?): PaymentRequestRecord

    /**
     * Queue an empty Private Payment List for one counterparty receiver.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `clearPrivatePaymentList`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): QueuedPrivateMessage

    /**
     * Queue an empty Private Payment List and process that counterparty's queue.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `clearPrivatePaymentListAndProcessOutbound`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): PrivatePaymentListDeliveryReport

    /**
     * Return this runtime's configuration.
     */
    public fun `config`(): PaykitSdkConfig

    /**
     * Return one local Contact Record.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `contactRecord`(`publicKey`: kotlin.String): ContactRecord?

    /**
     * Return all local Contact Records.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `contactRecords`(): List<ContactRecord>

    /**
     * Return the latest valid Private Payment List view for a counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `currentPrivatePaymentList`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): PrivatePaymentListView?

    /**
     * Resolve this identity's public profile.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `currentProfile`(`allowPubkyProfileFallback`: kotlin.Boolean): ContactProfileResolution?

    /**
     * Delete a blob by `pubky://` URI or configured Paykit profile path.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `deletePaykitBlob`(`uriOrPath`: kotlin.String)

    /**
     * Delete this identity's Paykit Profile.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `deletePaykitProfile`()

    /**
     * Return tracked Encrypted Link recovery marker state for a counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `encryptedLinkRecoveryMarkerStatus`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): EncryptedLinkRecoveryMarkerReport?

    /**
     * Queue the current complete Private Payment List for one counterparty receiver.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `enqueuePrivatePaymentList`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): QueuedPrivateMessage

    /**
     * Queue an explicit complete Private Payment List for one counterparty receiver.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `enqueuePrivatePaymentListWithReceivingDetails`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `receivingDetails`: List<PrivateReceivingDetail>): QueuedPrivateMessage

    /**
     * Start or advance an Encrypted Link Handshake for one counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `ensureLinkWithPeer`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `maxAdvanceSteps`: kotlin.UInt): LinkedPeerHandshakeReport

    /**
     * Export SDK-managed backup state as an opaque blob.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `exportBackupState`(): SdkBackupBlob

    /**
     * Export SDK-managed backup state as a hex string.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `exportBackupString`(): kotlin.String

    /**
     * Fetch a public Paykit Profile.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `fetchPaykitProfile`(`publicKey`: kotlin.String, `receiverPath`: kotlin.String): PaykitProfileRecord?

    /**
     * Fetch public Pubky file bytes.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `fetchPubkyFile`(`uri`: kotlin.String): kotlin.ByteArray?

    /**
     * Fetch public Pubky app follows.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `fetchPubkyFollows`(`publicKey`: kotlin.String): List<kotlin.String>

    /**
     * Fetch a public Pubky app profile.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `fetchPubkyProfile`(`publicKey`: kotlin.String): PubkyProfileRecord?

    /**
     * Fetch a public Pubky UTF-8 text file.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `fetchPubkyText`(`uri`: kotlin.String): kotlin.String?

    /**
     * Return current identity status, when initialized.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `identityStatus`(): IdentityStatus?

    /**
     * Initialize durable SDK identity state.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `initialize`(): InitializationReport

    /**
     * Start an Encrypted Link Handshake as the initiator.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `initiateLinkWithPeer`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): LinkedPeerHandshakeReport

    /**
     * Prepare, store, and queue Receipt Access for private delivery.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `issueReceipt`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `draft`: ReceiptDraft): ReceiptIssuanceView

    /**
     * List issued receipts across non-blocked counterparties, newest first.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `issuedReceipts`(): List<ReceiptIssuanceView>

    /**
     * List issued receipts for one counterparty, newest first.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `issuedReceiptsTo`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): List<ReceiptIssuanceView>

    /**
     * List locally tracked Linked Peer records.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `linkedPeers`(): List<LinkedPeerRecord>

    /**
     * Return Payment Requests matching a local SDK filter.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `listPaymentRequests`(`filter`: PaymentRequestFilter): List<PaymentRequestRecord>

    /**
     * Observe a counterparty's public recovery marker.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `observeEncryptedLinkRecoveryMarker`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): EncryptedLinkRecoveryMarkerReport

    /**
     * Fetch one public Paykit receiver marker, if present.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `paykitReceiverMarker`(`publicKey`: kotlin.String, `receiverPath`: kotlin.String): PaykitReceiverMarker?

    /**
     * List public Paykit receiver paths for a Pubky identity.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `paykitReceiverPaths`(`publicKey`: kotlin.String): List<kotlin.String>

    /**
     * Return all Payment Requests across non-blocked counterparties.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `paymentRequests`(): List<PaymentRequestRecord>

    /**
     * Return Payment Requests involving one counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `paymentRequestsWith`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): List<PaymentRequestRecord>

    /**
     * List counterparties with queued private messages ready for retry.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `pendingOutboundPrivateCounterparties`(): List<CounterpartyReceiver>

    /**
     * Prepare private contact state, then resolve private endpoints.
     *
     * Pass the last consumed list version to require a newer Private Payment
     * List after private messages have been refreshed.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `prepareAndResolvePrivateContactPayment`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `amount`: PaymentAmountContext?, `afterPrivatePaymentListVersion`: kotlin.ULong?, `maxAdvanceSteps`: kotlin.UInt): PreparedPrivateContactPayment

    /**
     * Prepare a receipt issuance and persist it before network side effects.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `prepareReceiptIssuance`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `draft`: ReceiptDraft): ReceiptIssuanceView

    /**
     * Send queued outbound private messages for one counterparty in order.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `processOutboundPrivateMessages`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): OutboundPrivateSendReport

    /**
     * Process queued outbound private messages for every pending counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `processPendingPrivateMessages`(): List<OutboundPrivateCounterpartySendReport>

    /**
     * Continue storage and Receipt Access queueing for a prepared issuance.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `processReceiptIssuance`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `receiptId`: kotlin.String): ReceiptIssuanceView

    /**
     * Queue a new Payment Request proposal and return local derived state.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `proposePaymentRequest`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `terms`: PaymentRequestTerms): PaymentRequestRecord

    /**
     * Publish a minimal local recovery marker for a counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `publishEncryptedLinkRecoveryMarker`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): EncryptedLinkRecoveryMarkerReport

    /**
     * Publish a blob under this identity's Paykit profile namespace.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `publishPaykitBlob`(`blobName`: kotlin.String, `bytes`: kotlin.ByteArray): PaykitBlobRecord

    /**
     * Publish this identity's Paykit Profile.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `publishPaykitProfile`(`profile`: PaykitProfile): PaykitProfileRecord

    /**
     * Publish the configured local receiver marker.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `publishPaykitReceiverMarker`(`capabilities`: PaykitReceiverCapabilities): PaykitReceiverMarker

    /**
     * Publish a public Contact Marker for a local Contact Record.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `publishPublicContact`(`publicKey`: kotlin.String, `receiverPath`: kotlin.String): ContactRecord

    /**
     * List Receipt Access across non-blocked counterparties, newest first.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `receiptAccess`(): List<ReceiptAccessView>

    /**
     * List Receipt Access received from one counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `receiptAccessFrom`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): List<ReceiptAccessView>

    /**
     * List indexed Receipt Access records for one counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `receiptAccessRecords`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): List<ReceiptAccessView>

    /**
     * List local receipt issuance records for one counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `receiptIssuanceRecords`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): List<ReceiptIssuanceView>

    /**
     * List decrypted Receipt records for one issuer, newest first.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `receiptRecords`(`issuer`: kotlin.String, `issuerReceiverPath`: kotlin.String): List<ReceiptRecord>

    /**
     * List decrypted receipts across non-blocked issuers, newest first.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `receipts`(): List<ReceiptRecord>

    /**
     * List decrypted receipts from one issuer, newest first.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `receiptsFrom`(`issuer`: kotlin.String, `issuerReceiverPath`: kotlin.String): List<ReceiptRecord>

    /**
     * Receive and durably persist available private messages.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `receivePrivateMessages`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): PrivateStreamIntakeReport

    /**
     * Receive private messages from every locally linked counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `receivePrivateMessagesFromLinkedPeers`(): List<PrivateStreamCounterpartyIntakeReport>

    /**
     * Return inbound Payment Requests received from one counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `receivedPaymentRequestsFrom`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): List<PaymentRequestRecord>

    /**
     * Refresh the cached Paykit Profile for a local Contact Record.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `refreshContactPaykitProfile`(`publicKey`: kotlin.String, `receiverPath`: kotlin.String): ContactRecord?

    /**
     * Queue rejection for a received Payment Request and return local derived state.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `rejectPaymentRequest`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `paymentRequestId`: kotlin.String, `reason`: kotlin.String?): PaymentRequestRecord

    /**
     * Remove a local Contact Record when it has no public marker to clean up.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `removeContact`(`publicKey`: kotlin.String): ContactRecord?

    /**
     * Remove the local public recovery marker for a counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `removeEncryptedLinkRecoveryMarker`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): EncryptedLinkRecoveryMarkerReport

    /**
     * Remove the configured local receiver marker.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `removePaykitReceiverMarker`()

    /**
     * Remove a public Contact Marker.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `removePublicContact`(`publicKey`: kotlin.String, `receiverPath`: kotlin.String): ContactRecord?

    /**
     * Resolve display metadata for a contact.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `resolveContactProfile`(`publicKey`: kotlin.String, `receiverPath`: kotlin.String, `allowPubkyProfileFallback`: kotlin.Boolean): ContactProfileResolution?

    /**
     * Resolve payable private endpoints for one counterparty.
     *
     * Pass the last consumed list version to require a newer Private Payment
     * List. The returned version and endpoints come from the same local list
     * snapshot.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `resolvePrivateContactPayment`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `amount`: PaymentAmountContext?, `afterPrivatePaymentListVersion`: kotlin.ULong?): PrivateContactPaymentResolution

    /**
     * Resolve public profile metadata, preferring Paykit Profile.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `resolveProfile`(`publicKey`: kotlin.String, `receiverPath`: kotlin.String, `allowPubkyProfileFallback`: kotlin.Boolean): ContactProfileResolution?

    /**
     * Resolve payable public Payment Endpoints for one counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `resolvePublicContactPayment`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `amount`: PaymentAmountContext?): PublicContactPaymentResolution

    /**
     * Restore SDK-managed backup state from an opaque blob.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `restoreBackupState`(`backup`: SdkBackupBlob): RestoreReport

    /**
     * Restore SDK-managed backup state from a hex string.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `restoreBackupString`(`backup`: kotlin.String): RestoreReport

    /**
     * Fetch, decrypt, and store a receipt from an indexed Receipt Access event.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `retrieveReceipt`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `receiptId`: kotlin.String): ReceiptRecord

    /**
     * Save or update a local Contact Record.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `saveContact`(`update`: ContactUpdate): ContactRecord

    /**
     * Clear live Pubky session access and SDK-managed identity-scoped state.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `signOut`(): IdentityStatus

    /**
     * Return the current platform SDK state revision, when a state blob exists.
     */
    @Throws(PaykitException::class)
    public fun `stateRevision`(): kotlin.String?

    /**
     * Queue a Payment Proof for an accepted Payment Request.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `submitPaymentProof`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `paymentRequestId`: kotlin.String, `proof`: PaymentProofSubmission): PaymentRequestRecord

    /**
     * Queue Private Payment List updates for saved local contacts.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `syncContactPrivatePaymentLists`(`clearUnlistedLinkedPeers`: kotlin.Boolean): PrivatePaymentListSyncReport

    /**
     * Queue contact Private Payment Lists and process pending private messages.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `syncContactPrivatePaymentListsAndProcessOutbound`(`clearUnlistedLinkedPeers`: kotlin.Boolean): PrivatePaymentListDeliveryReport

    /**
     * Queue reservation-backed Private Payment Lists and process their queues.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `syncPrivatePaymentListsWithReservationsAndProcessOutbound`(`updates`: List<PrivatePaymentListReservationUpdateInput>, `clearUnlistedLinkedPeers`: kotlin.Boolean): PrivatePaymentListDeliveryReport

    /**
     * Retry pending public Contact Marker publication/removal work.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `syncPublicContactMarkers`(): List<ContactRecord>

    /**
     * Publish current public receiving details and remove stale SDK-managed endpoints.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `syncPublicEndpoints`(): EndpointSyncReport

    /**
     * Publish explicit public receiving details and remove stale SDK-managed endpoints.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `syncPublicEndpointsWithReceivingDetails`(`receivingDetails`: List<PublicReceivingDetail>): EndpointSyncReport

    /**
     * Remove a local peer block and return the peer to NotLinked.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `unblockPeer`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): LinkedPeerRecord

    /**
     * Upload profile avatar bytes and return the published blob record.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `uploadProfileAvatar`(`bytes`: kotlin.ByteArray, `contentType`: kotlin.String): PaykitBlobRecord

    public companion object
}




/**
 * Payment adapter payload text with redacted debug output.
 */
public interface PaymentPayloadInterface {

    /**
     * Export the payload text for payment adapter execution.
     */
    public fun `exportText`(): kotlin.String

    public companion object
}




/**
 * Payment Reference text with redacted debug output.
 */
public interface PaymentReferenceInterface {

    /**
     * Export the reference text for explicit payment execution or display.
     */
    public fun `exportText`(): kotlin.String

    public companion object
}




/**
 * Private JSON object with redacted debug output.
 */
public interface PrivateJsonObjectInterface {

    /**
     * Export the JSON text for explicit app display, storage, or payment execution.
     */
    public fun `exportText`(): kotlin.String

    public companion object
}




/**
 * Private workflow error with redacted default context.
 */
public interface PrivateOperationErrorInterface {

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
public interface PubkyAuthRequestInterface {

    /**
     * Return the auth URL to show as a deeplink or QR code.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `authorizationUrl`(): kotlin.String

    /**
     * Wait for auth approval using the receiver's persisted Noise key.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `complete`(`localSecretKey`: PubkyLocalSecretKey?, `receiverNoiseSecretKey`: ReceiverNoiseSecretKey, `requiredCapabilities`: kotlin.String): PubkySessionBootstrapResult

    public companion object
}




/**
 * Local Pubky secret key bytes supplied by platform secure storage.
 */
public interface PubkyLocalSecretKeyInterface {

    /**
     * Export the raw bytes for platform secure storage.
     */
    public fun `exportBytes`(): kotlin.ByteArray

    public companion object
}




/**
 * Live Pubky access material supplied by platform session storage.
 */
public interface PubkySessionAccessInterface {

    /**
     * Export the local Pubky secret key, when available.
     */
    public fun `exportLocalSecretKey`(): PubkyLocalSecretKey?

    /**
     * Export the receiver Noise secret key for platform secure storage.
     */
    public fun `exportReceiverNoiseSecretKey`(): ReceiverNoiseSecretKey

    /**
     * Export the Pubky session bearer secret for platform secure storage.
     */
    public fun `exportSessionSecret`(): kotlin.String

    public companion object
}




/**
 * Pubky session bootstrap helper.
 */
public interface PubkySessionBootstrapInterface {

    /**
     * Approve a Pubky auth URL with this local secret key.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `approveAuth`(`authUrl`: kotlin.String, `expectedCapabilities`: kotlin.String, `localSecretKey`: PubkyLocalSecretKey)

    /**
     * Deliver a signed application-defined claim, then approve Pubky Auth.
     *
     * This high-level operation owns validation, request-bound signing,
     * channel derivation, encryption, relay delivery, and approval ordering.
     */
    @Throws(PubkyAuthCompanionClaimApprovalException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `approveAuthWithCompanionClaim`(`authUrl`: kotlin.String, `expectedCapabilities`: kotlin.String, `localSecretKey`: PubkyLocalSecretKey, `claim`: PubkyAuthCompanionClaim)

    /**
     * Import an exported Pubky session secret and its persisted receiver Noise key.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `importSession`(`sessionSecret`: kotlin.String, `localSecretKey`: PubkyLocalSecretKey?, `receiverNoiseSecretKey`: ReceiverNoiseSecretKey, `requiredCapabilities`: kotlin.String): PubkySessionBootstrapResult

    /**
     * Resume a short-lived auth flow from its authorization URL.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `resumeAuth`(`authorizationUrl`: kotlin.String, `expectedCapabilities`: kotlin.String): PubkyAuthRequest

    /**
     * Sign in with the receiver's persisted Noise key.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `signIn`(`localSecretKey`: PubkyLocalSecretKey, `receiverNoiseSecretKey`: ReceiverNoiseSecretKey, `requiredCapabilities`: kotlin.String): PubkySessionBootstrapResult

    /**
     * Sign up on a homeserver with the receiver-owned Noise key.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `signUp`(`localSecretKey`: PubkyLocalSecretKey, `receiverNoiseSecretKey`: ReceiverNoiseSecretKey, `homeserverPublicKey`: kotlin.String, `signupCode`: kotlin.String?, `requiredCapabilities`: kotlin.String): PubkySessionBootstrapResult

    /**
     * Start a sign-in auth flow for an external signer.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `startSignInAuth`(`capabilities`: kotlin.String): PubkyAuthRequest

    /**
     * Start a signup auth flow for an external signer.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `startSignUpAuth`(`capabilities`: kotlin.String, `homeserverPublicKey`: kotlin.String, `signupToken`: kotlin.String?): PubkyAuthRequest

    public companion object
}




/**
 * Receiver-scoped Noise secret key bytes supplied by platform secure storage.
 */
public interface ReceiverNoiseSecretKeyInterface {

    /**
     * Export the raw bytes for platform secure storage.
     */
    public fun `exportBytes`(): kotlin.ByteArray

    public companion object
}




/**
 * Reservation attribution metadata with redacted debug output.
 */
public interface ReservationAttributionInterface {

    /**
     * Export attribution fields for payment adapter cleanup.
     */
    public fun `exportFields`(): Map<kotlin.String, kotlin.String>

    public companion object
}




/**
 * SDK backup blob owned by the app.
 */
public interface SdkBackupBlobInterface {

    /**
     * Export the raw bytes for app-controlled backup storage.
     */
    public fun `exportBytes`(): kotlin.ByteArray

    public companion object
}




/**
 * Platform-owned, mode-specific payment adapter callbacks.
 *
 * Public callbacks never receive private values, and private callbacks never
 * receive public values.
 */
public interface SdkPaymentAdapter {

    /**
     * Return receiving details intended for public Payment Endpoints.
     */
    @Throws(PaykitException::class)
    public fun `currentPublicReceivingDetails`(): List<PublicReceivingDetail>

    /**
     * Return receiving details for one counterparty's Private Payment List.
     */
    @Throws(PaykitException::class)
    public fun `currentPrivateReceivingDetails`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): List<PrivateReceivingDetail>

    /**
     * Reserve receiving details for a counterparty's Private Payment List.
     */
    @Throws(PaykitException::class)
    public fun `reservePrivateReceivingDetails`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): PrivateReceivingDetailReservationResponse

    /**
     * Cancel a previously reserved receiving detail.
     */
    @Throws(PaykitException::class)
    public fun `cancelPrivateReceivingDetailReservation`(`cancellation`: PrivatePaymentEndpointReservationCancellation)

    /**
     * Return payable public candidate ids in adapter-preferred order.
     */
    @Throws(PaykitException::class)
    public fun `selectPublicPaymentEndpointIds`(`request`: PublicPaymentEndpointSelectionRequest): List<kotlin.String>

    /**
     * Build a payment target from a payable public endpoint.
     */
    @Throws(PaykitException::class)
    public fun `buildPublicPaymentTarget`(`endpoint`: PublicPaymentEndpointCandidate): PaymentTarget

    /**
     * Return payable private candidate ids in adapter-preferred order.
     */
    @Throws(PaykitException::class)
    public fun `selectPrivatePaymentEndpointIds`(`request`: PrivatePaymentEndpointSelectionRequest): List<kotlin.String>

    /**
     * Build a payment target from a payable private endpoint.
     */
    @Throws(PaykitException::class)
    public fun `buildPrivatePaymentTarget`(`endpoint`: PrivatePaymentEndpointCandidate): PaymentTarget

    public companion object
}




/**
 * Platform-owned Pubky session provider.
 */
public interface SdkPubkySessionProvider {

    /**
     * Load current live Pubky session access, when available.
     */
    @Throws(PaykitException::class)
    public fun `loadSessionAccess`(): PubkySessionAccess?

    /**
     * Report whether unauthenticated public Pubky storage can be used.
     */
    @Throws(PaykitException::class)
    public fun `publicStorageAvailable`(): kotlin.Boolean

    /**
     * Clear platform session access during explicit SDK sign-out.
     */
    @Throws(PaykitException::class)
    public fun `clearSessionAccess`()

    public companion object
}




/**
 * SDK state blob owned by platform storage.
 */
public interface SdkStateBlobInterface {

    /**
     * Export the raw bytes for platform storage.
     */
    public fun `exportBytes`(): kotlin.ByteArray

    public companion object
}




/**
 * Platform-owned durable blob store for SDK state.
 */
public interface SdkStateBlobStore {

    /**
     * Load the current SDK state blob, when one exists.
     */
    @Throws(PaykitException::class)
    public fun `loadStateBlob`(): SdkStateBlobSnapshot?

    /**
     * Atomically save a new SDK state blob.
     *
     * `expected_revision` is `None` when no previous blob was loaded. The
     * platform store should reject the write if the stored revision changed.
     */
    @Throws(PaykitException::class)
    public fun `saveStateBlobAtomically`(`blob`: SdkStateBlob, `expectedRevision`: kotlin.String?): kotlin.String

    public companion object
}




/**
 * Time interval a recurring Payment Proof applies to.
 */
@kotlinx.serialization.Serializable
public data class BillingPeriod (
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
 * Contact display profile resolved by trying Paykit Profile first.
 */
@kotlinx.serialization.Serializable
public data class ContactProfileResolution (
    /**
     * Profile owner.
     */
    val `publicKey`: kotlin.String,
    /**
     * Source that produced this profile.
     */
    val `source`: ContactProfileSource,
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
    val `paykitProfile`: PaykitProfile?,
    /**
     * Pubky Profile payload when the source is Pubky Profile.
     */
    val `pubkyProfile`: PubkyProfile?,
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
public data class ContactRecord (
    /**
     * Contact public key.
     */
    val `publicKey`: kotlin.String,
    /**
     * Contact Paykit receiver paths.
     */
    val `receiverPaths`: List<kotlin.String>,
    /**
     * Optional local display label.
     */
    val `label`: kotlin.String?,
    /**
     * Cached public profile, when fetched.
     */
    val `profile`: PaykitProfile?,
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
    val `publicContactMarkerStatus`: PublicationStatus,
    /**
     * Receiver path for the current public contact marker state.
     */
    val `publicContactMarkerReceiverPath`: kotlin.String?,
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
public data class ContactUpdate (
    /**
     * Contact public key.
     */
    val `publicKey`: kotlin.String,
    /**
     * Contact Paykit receiver paths.
     */
    val `receiverPaths`: List<kotlin.String>,
    /**
     * Optional local display label.
     */
    val `label`: kotlin.String?
) {
    public companion object
}



/**
 * Counterparty plus the Paykit receiver path used for private workflows.
 */
@kotlinx.serialization.Serializable
public data class CounterpartyReceiver (
    /**
     * Counterparty public key.
     */
    val `counterparty`: kotlin.String,
    /**
     * Counterparty Paykit receiver path.
     */
    val `counterpartyReceiverPath`: kotlin.String
) {
    public companion object
}



/**
 * Public recovery marker state tracked for one Linked Peer.
 */

public data class EncryptedLinkRecoveryMarkerReport (
    /**
     * Counterparty public key.
     */
    val `counterparty`: kotlin.String,
    /**
     * Counterparty Paykit receiver path.
     */
    val `counterpartyReceiverPath`: kotlin.String,
    /**
     * Current Linked Peer state.
     */
    val `state`: LinkedPeerState,
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
    val `localMarkerLastError`: PrivateOperationError?,
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
            this.`counterpartyReceiverPath`,
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
public data class EndpointSyncChange (
    /**
     * Payment Endpoint Identifier.
     */
    val `identifier`: kotlin.String,
    /**
     * Resulting local publication status.
     */
    val `status`: PublicationStatus,
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
public data class EndpointSyncReport (
    /**
     * Endpoints successfully published or updated.
     */
    val `published`: List<EndpointSyncChange>,
    /**
     * Endpoints successfully removed.
     */
    val `removed`: List<EndpointSyncChange>,
    /**
     * Endpoints that failed to publish or remove.
     */
    val `failed`: List<EndpointSyncChange>
) {
    public companion object
}



/**
 * Reused Event ID with a different payload.
 */
@kotlinx.serialization.Serializable
public data class EventIdConflict (
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
public data class IdentityStatus (
    /**
     * Persisted local public key, or `None` after explicit sign-out.
     */
    val `publicKey`: kotlin.String?,
    /**
     * Whether live Pubky session access is available for this identity.
     */
    val `liveSessionAvailable`: kotlin.Boolean
) {
    public companion object
}



/**
 * Initialization report returned after SDK startup.
 */
@kotlinx.serialization.Serializable
public data class InitializationReport (
    /**
     * Last persisted identity status.
     */
    val `identity`: IdentityStatus
) {
    public companion object
}



/**
 * Result of starting or advancing an Encrypted Link Handshake.
 */
@kotlinx.serialization.Serializable
public data class LinkedPeerHandshakeReport (
    /**
     * Counterparty public key.
     */
    val `counterparty`: kotlin.String,
    /**
     * Counterparty Paykit receiver path.
     */
    val `counterpartyReceiverPath`: kotlin.String,
    /**
     * Current Linked Peer state after the operation.
     */
    val `state`: LinkedPeerState,
    /**
     * Current Encrypted Link state generation.
     */
    val `generation`: kotlin.ULong,
    /**
     * In-progress handshake role, when a handshake remains pending.
     */
    val `handshakeRole`: EncryptedLinkHandshakeRole?
) {
    public companion object
}



/**
 * Locally tracked Linked Peer record.
 */

public data class LinkedPeerRecord (
    /**
     * Counterparty public key.
     */
    val `counterparty`: kotlin.String,
    /**
     * Counterparty Paykit receiver path.
     */
    val `counterpartyReceiverPath`: kotlin.String,
    /**
     * Current local relationship/link state.
     */
    val `state`: LinkedPeerState,
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
    val `localRecoveryMarkerLastError`: PrivateOperationError?,
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
            this.`counterpartyReceiverPath`,
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

public data class OutboundPrivateCounterpartySendReport (
    /**
     * Counterparty whose queue was processed.
     */
    val `counterparty`: kotlin.String,
    /**
     * Counterparty Paykit receiver path.
     */
    val `counterpartyReceiverPath`: kotlin.String,
    /**
     * Successful send report, when processing completed.
     */
    val `report`: OutboundPrivateSendReport?,
    /**
     * Error text, when processing failed for this counterparty.
     */
    val `error`: PrivateOperationError?
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`counterparty`,
            this.`counterpartyReceiverPath`,
            this.`report`,
            this.`error`,
        )
    }
    public companion object
}



/**
 * Failed outbound private send attempt.
 */

public data class OutboundPrivateSendFailure (
    /**
     * Outbound message id.
     */
    val `outboundMessageId`: kotlin.ULong,
    /**
     * Error from the send attempt.
     */
    val `error`: PrivateOperationError
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

public data class OutboundPrivateSendReport (
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
    val `failed`: List<OutboundPrivateSendFailure>,
    /**
     * Superseded reservation cleanup failures observed in this run.
     */
    val `reservationCleanupFailures`: List<ReservationCleanupFailure>,
    /**
     * Recovery marker publication failures observed after fail-closed recovery.
     */
    val `recoveryMarkerFailures`: List<RecoveryMarkerPublishFailure>
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
public data class PaykitBlobRecord (
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
public data class PaykitProfile (
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
public data class PaykitProfileRecord (
    /**
     * Profile owner.
     */
    val `publicKey`: kotlin.String,
    /**
     * Public profile metadata.
     */
    val `profile`: PaykitProfile,
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
 * Public capabilities advertised by a Paykit receiver marker.
 */
@kotlinx.serialization.Serializable
public data class PaykitReceiverCapabilities (
    /**
     * Receiver can participate in private Paykit payment workflows.
     */
    val `privatePayments`: kotlin.Boolean,
    /**
     * Receiver can send or receive Payment Request messages.
     */
    val `paymentRequests`: kotlin.Boolean,
    /**
     * Receiver can issue or retrieve Paykit Receipts.
     */
    val `receipts`: kotlin.Boolean,
    /**
     * Receiver can execute outgoing payments itself.
     */
    val `outgoingPayments`: kotlin.Boolean
) {
    public companion object
}



/**
 * Lightweight public marker for one Paykit receiver path.
 */
@kotlinx.serialization.Serializable
public data class PaykitReceiverMarker (
    /**
     * Receiver path this marker belongs to.
     */
    val `receiverPath`: kotlin.String,
    /**
     * Public receiver capabilities.
     */
    val `capabilities`: PaykitReceiverCapabilities,
    /**
     * Receiver-scoped public key used for Encrypted Links.
     */
    val `noisePublicKey`: kotlin.String
) {
    public companion object
}



/**
 * Runtime configuration for Paykit SDK bindings.
 */
@kotlinx.serialization.Serializable
public data class PaykitSdkConfig (
    /**
     * Receiver folder for this app/runtime under `/pub/paykit/v0/{app}/{wallet|server}`.
     */
    val `receiverPath`: kotlin.String,
    /**
     * Namespace segment for SDK profile/contact public data under `/pub/`.
     */
    val `profileNamespace`: kotlin.String,
    /**
     * Public endpoint management scope.
     */
    val `endpointManagementScope`: EndpointManagementScope,
    /**
     * Public recovery marker behavior.
     */
    val `encryptedLinkRecoveryMarkers`: EncryptedLinkRecoveryMarkerPolicy,
    /**
     * Public contact marker behavior.
     */
    val `publicContactSharing`: PublicContactSharingPolicy,
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
public data class PaymentAmountContext (
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
 * Payment Proof captured in a derived Payment Request record.
 */

public data class PaymentProofRecord (
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
    val `outboundStatus`: OutboundPrivateMessageStatus?,
    /**
     * Stream item id, when proof was received from the counterparty.
     */
    val `streamItemId`: kotlin.ULong?,
    /**
     * Payment Reference copied from the proof.
     */
    val `paymentReference`: PaymentReference,
    /**
     * Optional Billing Period copied from the proof.
     */
    val `billingPeriod`: BillingPeriod?,
    /**
     * Payment Endpoint Identifier used for payment.
     */
    val `paymentEndpointIdentifier`: kotlin.String,
    /**
     * Method-specific proof object encoded as JSON.
     */
    val `proof`: PrivateJsonObject,
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
            this.`proof`,
            this.`recordedAt`,
        )
    }
    public companion object
}



/**
 * Method-specific Payment Proof submission data.
 */

public data class PaymentProofSubmission (
    /**
     * Billing Period for recurring Payment Requests.
     */
    val `billingPeriod`: BillingPeriod?,
    /**
     * Payment Endpoint Identifier used for payment.
     */
    val `paymentEndpointIdentifier`: kotlin.String,
    /**
     * Method-specific proof object encoded as JSON.
     */
    val `proof`: PrivateJsonObject
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`billingPeriod`,
            this.`paymentEndpointIdentifier`,
            this.`proof`,
        )
    }
    public companion object
}



/**
 * Payment Amount fields used by Payment Requests.
 */
@kotlinx.serialization.Serializable
public data class PaymentRequestAmount (
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
public data class PaymentRequestFilter (
    /**
     * Restrict results to one counterparty.
     */
    val `counterparty`: kotlin.String?,
    /**
     * Restrict results to one counterparty receiver/runtime folder.
     */
    val `counterpartyReceiverPath`: kotlin.String?,
    /**
     * Restrict results to one local role.
     */
    val `localRole`: PaymentRequestLocalRole?,
    /**
     * Restrict results to lifecycle states. Empty means all states.
     */
    val `states`: List<PaymentRequestLifecycleState>,
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

public data class PaymentRequestRecord (
    /**
     * Counterparty associated with the private stream.
     */
    val `counterparty`: kotlin.String,
    /**
     * Counterparty receiver/runtime folder associated with the private stream.
     */
    val `counterpartyReceiverPath`: kotlin.String,
    /**
     * Stable Payment Request ID.
     */
    val `paymentRequestId`: kotlin.String,
    /**
     * Local role, when known.
     */
    val `localRole`: PaymentRequestLocalRole?,
    /**
     * Derived local lifecycle state.
     */
    val `state`: PaymentRequestLifecycleState,
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
    val `proposalOutboundStatus`: OutboundPrivateMessageStatus?,
    /**
     * Proposal Event ID.
     */
    val `proposalEventId`: kotlin.String?,
    /**
     * Immutable terms from the proposal.
     */
    val `terms`: PaymentRequestTerms?,
    /**
     * Acceptance Event ID.
     */
    val `acceptedEventId`: kotlin.String?,
    /**
     * Local outbound delivery status for an acceptance event.
     */
    val `acceptedOutboundStatus`: OutboundPrivateMessageStatus?,
    /**
     * Rejection Event ID.
     */
    val `rejectedEventId`: kotlin.String?,
    /**
     * Local outbound delivery status for a rejection event.
     */
    val `rejectedOutboundStatus`: OutboundPrivateMessageStatus?,
    /**
     * Cancellation Event ID.
     */
    val `canceledEventId`: kotlin.String?,
    /**
     * Local outbound delivery status for a cancellation event.
     */
    val `canceledOutboundStatus`: OutboundPrivateMessageStatus?,
    /**
     * Payment Proof records in local record order.
     */
    val `paymentProofs`: List<PaymentProofRecord>,
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
    val `lastOutboundStatus`: OutboundPrivateMessageStatus?,
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
            this.`counterpartyReceiverPath`,
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
public data class PaymentRequestRecurrence (
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
     * Optional RFC3339 UTC timestamp using `Z`, after `starts_at` when
     * present.
     */
    val `endsAt`: kotlin.String?
) {
    public companion object
}



/**
 * Immutable terms for a Payment Request proposal.
 */

public data class PaymentRequestTerms (
    /**
     * Requested amount.
     */
    val `amount`: PaymentRequestAmount,
    /**
     * Payee-provided payment correlation value.
     */
    val `paymentReference`: PaymentReference,
    /**
     * Proposal expiry before acceptance.
     */
    val `proposalExpiresAt`: kotlin.String?,
    /**
     * Optional recurrence.
     */
    val `recurrence`: PaymentRequestRecurrence?,
    /**
     * Accepted Payment Endpoint Identifier strings.
     */
    val `acceptedPaymentEndpointIdentifiers`: List<kotlin.String>,
    /**
     * Application-specific metadata encoded as a JSON object.
     */
    val `metadata`: PrivateJsonObject
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`amount`,
            this.`paymentReference`,
            this.`proposalExpiresAt`,
            this.`recurrence`,
            this.`acceptedPaymentEndpointIdentifiers`,
            this.`metadata`,
        )
    }
    public companion object
}



/**
 * Payment-method-specific execution payload produced by the adapter.
 */

public data class PaymentTarget (
    /**
     * Method-specific target payload.
     */
    val `payload`: PaymentPayload
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`payload`,
        )
    }
    public companion object
}



/**
 * Result of preparing private contact state and resolving private endpoints.
 */

public data class PreparedPrivateContactPayment (
    /**
     * Private endpoint resolution after preparation.
     */
    val `resolution`: PrivateContactPaymentResolution,
    /**
     * Encrypted Link handshake/advance report, when setup was attempted.
     */
    val `linkReport`: LinkedPeerHandshakeReport?,
    /**
     * Private stream receive report, when messages were refreshed.
     */
    val `receiveReport`: PrivateStreamIntakeReport?,
    /**
     * Outbound private send report, when queued messages were processed.
     */
    val `outboundReport`: OutboundPrivateSendReport?
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`resolution`,
            this.`linkReport`,
            this.`receiveReport`,
            this.`outboundReport`,
        )
    }
    public companion object
}



/**
 * Result of resolving a Private Payment List for one counterparty.
 */

public data class PrivateContactPaymentResolution (
    /**
     * Private payment resolution outcome.
     */
    val `status`: PrivatePaymentResolutionStatus,
    /**
     * Encrypted Link and Private Payment List state observed during resolution.
     */
    val `state`: PrivatePaymentResolutionState,
    /**
     * Opaque freshness token for the Private Payment List used by this result.
     */
    val `privatePaymentListVersion`: kotlin.ULong?,
    /**
     * Payable private Payment Endpoints in adapter-preferred order.
     */
    val `payableEndpoints`: List<ResolvedPrivatePaymentEndpoint>
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`status`,
            this.`state`,
            this.`privatePaymentListVersion`,
            this.`payableEndpoints`,
        )
    }
    public companion object
}



/**
 * Private Payment Endpoint candidate passed to the payment adapter.
 */

public data class PrivatePaymentEndpointCandidate (
    /**
     * Opaque candidate id for this callback request.
     */
    val `candidateId`: kotlin.String,
    /**
     * Counterparty that privately shared the endpoint.
     */
    val `counterparty`: kotlin.String,
    /**
     * Counterparty Paykit receiver path.
     */
    val `counterpartyReceiverPath`: kotlin.String,
    /**
     * Payment Endpoint Identifier string.
     */
    val `identifier`: kotlin.String,
    /**
     * Serialized endpoint payload.
     */
    val `payload`: PaymentPayload
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`candidateId`,
            this.`counterparty`,
            this.`counterpartyReceiverPath`,
            this.`identifier`,
            this.`payload`,
        )
    }
    public companion object
}



/**
 * Private receiving detail reserved by the payment adapter.
 */

public data class PrivatePaymentEndpointReservation (
    /**
     * Adapter-stable reservation id.
     */
    val `reservationId`: kotlin.String,
    /**
     * Reserved receiving detail.
     */
    val `receivingDetail`: PrivateReceivingDetail,
    /**
     * Optional reservation expiry as RFC3339 text.
     */
    val `expiresAt`: kotlin.String?,
    /**
     * Adapter attribution metadata.
     */
    val `attribution`: ReservationAttribution
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

public data class PrivatePaymentEndpointReservationCancellation (
    /**
     * Adapter-stable reservation id.
     */
    val `reservationId`: kotlin.String,
    /**
     * Counterparty the reservation was intended for.
     */
    val `counterparty`: kotlin.String,
    /**
     * Counterparty Paykit receiver path.
     */
    val `counterpartyReceiverPath`: kotlin.String,
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
    val `attribution`: ReservationAttribution
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`reservationId`,
            this.`counterparty`,
            this.`counterpartyReceiverPath`,
            this.`identifier`,
            this.`payloadHash`,
            this.`attribution`,
        )
    }
    public companion object
}



/**
 * Plain reservation input for one Payment Endpoint.
 */
@kotlinx.serialization.Serializable
public data class PrivatePaymentEndpointReservationInput (
    /**
     * Adapter-stable reservation id.
     */
    val `reservationId`: kotlin.String,
    /**
     * Payment Endpoint Identifier string.
     */
    val `identifier`: kotlin.String,
    /**
     * Serialized endpoint payload.
     */
    val `payload`: kotlin.String,
    /**
     * Optional reservation expiry as RFC3339 text.
     */
    val `expiresAt`: kotlin.String?,
    /**
     * Adapter attribution metadata.
     */
    val `attribution`: Map<kotlin.String, kotlin.String>
) {
    public companion object
}



/**
 * Request passed to the payment adapter for private endpoint ordering.
 */

public data class PrivatePaymentEndpointSelectionRequest (
    /**
     * Counterparty being paid.
     */
    val `counterparty`: kotlin.String,
    /**
     * Counterparty Paykit receiver path.
     */
    val `counterpartyReceiverPath`: kotlin.String,
    /**
     * Optional amount context.
     */
    val `amount`: PaymentAmountContext?,
    /**
     * Private candidate endpoints in SDK preference order.
     */
    val `candidates`: List<PrivatePaymentEndpointCandidate>
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`counterparty`,
            this.`counterpartyReceiverPath`,
            this.`amount`,
            this.`candidates`,
        )
    }
    public companion object
}



/**
 * Failed delivery after a Private Payment List was queued.
 */

public data class PrivatePaymentListDeliveryFailure (
    /**
     * Counterparty whose outbound delivery failed.
     */
    val `counterparty`: kotlin.String,
    /**
     * Counterparty Paykit receiver path.
     */
    val `counterpartyReceiverPath`: kotlin.String,
    /**
     * Outbound message id, when the failure is tied to one message.
     */
    val `outboundMessageId`: kotlin.ULong?,
    /**
     * Reservation id, when the failure is tied to reservation cleanup.
     */
    val `reservationId`: kotlin.String?,
    /**
     * Delivery or cleanup error.
     */
    val `error`: PrivateOperationError
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`counterparty`,
            this.`counterpartyReceiverPath`,
            this.`outboundMessageId`,
            this.`reservationId`,
            this.`error`,
        )
    }
    public companion object
}



/**
 * Report from queueing and delivering Private Payment Lists.
 */

public data class PrivatePaymentListDeliveryReport (
    /**
     * Counterparty receivers that had a non-empty Private Payment List queued.
     */
    val `queued`: List<PrivatePaymentListSyncChange>,
    /**
     * Counterparty receivers that had an empty Private Payment List queued.
     */
    val `cleared`: List<PrivatePaymentListSyncChange>,
    /**
     * Counterparty receivers that could not be queued or cleared.
     */
    val `failedToQueue`: List<PrivatePaymentListSyncChange>,
    /**
     * Counterparty receivers queued successfully but failed during outbound delivery.
     */
    val `failedToDeliver`: List<PrivatePaymentListDeliveryFailure>
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`queued`,
            this.`cleared`,
            this.`failedToQueue`,
            this.`failedToDeliver`,
        )
    }
    public companion object
}



/**
 * One endpoint in the latest Private Payment List view.
 */

public data class PrivatePaymentListEndpoint (
    /**
     * Payment Endpoint Identifier string.
     */
    val `identifier`: kotlin.String,
    /**
     * Serialized endpoint payload.
     */
    val `payload`: PaymentPayload
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
 * Reservation-backed Private Payment List input for one counterparty receiver.
 */
@kotlinx.serialization.Serializable
public data class PrivatePaymentListReservationUpdateInput (
    /**
     * Counterparty that should receive the Private Payment List.
     */
    val `counterparty`: kotlin.String,
    /**
     * Counterparty Paykit receiver path.
     */
    val `counterpartyReceiverPath`: kotlin.String,
    /**
     * Complete reserved receiving details to share with this counterparty.
     *
     * An empty list queues an empty Private Payment List for this counterparty.
     */
    val `reservations`: List<PrivatePaymentEndpointReservationInput>
) {
    public companion object
}



/**
 * One counterparty receiver result from a Private Payment List sync.
 */
@kotlinx.serialization.Serializable
public data class PrivatePaymentListSyncChange (
    /**
     * Counterparty affected by the sync.
     */
    val `counterparty`: kotlin.String,
    /**
     * Counterparty Paykit receiver path.
     */
    val `counterpartyReceiverPath`: kotlin.String,
    /**
     * Queued outbound message id, when queueing succeeded.
     */
    val `outboundMessageId`: kotlin.ULong?,
    /**
     * Error text, when queueing failed.
     */
    val `error`: kotlin.String?
) {
    public companion object
}



/**
 * Report from syncing Private Payment Lists for local contact receivers.
 */
@kotlinx.serialization.Serializable
public data class PrivatePaymentListSyncReport (
    /**
     * Counterparty receivers that had a current Private Payment List queued.
     */
    val `queued`: List<PrivatePaymentListSyncChange>,
    /**
     * Counterparty receivers that had an empty Private Payment List queued.
     */
    val `cleared`: List<PrivatePaymentListSyncChange>,
    /**
     * Counterparty receivers that could not be queued or cleared.
     */
    val `failed`: List<PrivatePaymentListSyncChange>
) {
    public companion object
}



/**
 * Latest valid Private Payment List view for one counterparty receiver.
 */

public data class PrivatePaymentListView (
    /**
     * Stream item id of the latest valid list.
     */
    val `latestStreamItemId`: kotlin.ULong?,
    /**
     * Current endpoint payloads sorted by identifier.
     */
    val `paymentEndpoints`: List<PrivatePaymentListEndpoint>,
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
 * Payment-method-specific receiving detail for a Private Payment List.
 */

public data class PrivateReceivingDetail (
    /**
     * Payment Endpoint Identifier string.
     */
    val `identifier`: kotlin.String,
    /**
     * Serialized endpoint payload.
     */
    val `payload`: PaymentPayload
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
 * Explicit result for private receiving-detail reservation callbacks.
 */

public data class PrivateReceivingDetailReservationResponse (
    /**
     * Response kind.
     */
    val `kind`: PrivateReceivingDetailReservationResponseKind,
    /**
     * Reserved details when `kind` is `Reservations`.
     */
    val `reservations`: List<PrivatePaymentEndpointReservation>
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`kind`,
            this.`reservations`,
        )
    }
    public companion object
}



/**
 * Summary for receiving private messages from one counterparty.
 */

public data class PrivateStreamCounterpartyIntakeReport (
    /**
     * Counterparty whose private stream was received.
     */
    val `counterparty`: kotlin.String,
    /**
     * Counterparty Paykit receiver path.
     */
    val `counterpartyReceiverPath`: kotlin.String,
    /**
     * Successful intake report, when receive completed.
     */
    val `report`: PrivateStreamIntakeReport?,
    /**
     * Error text, when receive failed for this counterparty.
     */
    val `error`: PrivateOperationError?
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`counterparty`,
            this.`counterpartyReceiverPath`,
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
public data class PrivateStreamIntakeReport (
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
    val `eventConflicts`: List<EventIdConflict>
) {
    public companion object
}



/**
 * Application-defined input for a Pubky Auth companion claim.
 *
 * The application serializes its protocol-specific unsigned payload. Paykit
 * validates the identifiers, creates the request-bound identity signature,
 * encrypts the signed payload, and delivers it before normal Pubky Auth.
 *
 * Generated platform record descriptions may include the raw payload. Apps
 * must not log, interpolate, or otherwise stringify this record.
 */
@kotlinx.serialization.Serializable
public data class PubkyAuthCompanionClaim (
    /**
     * Auth URL query parameter that announces the claim.
     */
    val `queryParameter`: kotlin.String,
    /**
     * Protocol-specific claim type used for URL validation and relay derivation.
     */
    val `claimType`: kotlin.String,
    /**
     * Protocol-specific unsigned binary payload. Do not log this value.
     */
    val `unsignedPayload`: kotlin.ByteArray
) {
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (other == null || this::class != other::class) return false

        other as PubkyAuthCompanionClaim
        if (`queryParameter` != other.`queryParameter`) return false
        if (`claimType` != other.`claimType`) return false
        if (!`unsignedPayload`.contentEquals(other.`unsignedPayload`)) return false

        return true
    }
    override fun hashCode(): Int {
        var result = `queryParameter`.hashCode()
        result = 31 * result + `claimType`.hashCode()
        result = 31 * result + `unsignedPayload`.contentHashCode()
        return result
    }
    public companion object
}



/**
 * Public details parsed from a Pubky auth deep link.
 */
@kotlinx.serialization.Serializable
public data class PubkyAuthDetails (
    /**
     * Auth request kind.
     */
    val `kind`: PubkyAuthRequestKind,
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
public data class PubkyClientConfig (
    /**
     * Request timeout for Pubky HTTP operations in seconds.
     */
    val `requestTimeoutSecs`: kotlin.ULong,
    /**
     * Pubky network environment used by the client.
     */
    val `environment`: PubkyClientEnvironment,
    /**
     * DNS hostname or IPv4 address running local testnet services, or `None` for localhost.
     */
    val `testnetHost`: kotlin.String?
) {
    public companion object
}



/**
 * Public profile metadata from the Pubky app namespace.
 */
@kotlinx.serialization.Serializable
public data class PubkyProfile (
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
    val `links`: List<PubkyProfileLink>,
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
public data class PubkyProfileLink (
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
public data class PubkyProfileRecord (
    /**
     * Profile owner.
     */
    val `publicKey`: kotlin.String,
    /**
     * Public profile metadata.
     */
    val `profile`: PubkyProfile,
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
public data class PubkyResourceRef (
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

public data class PubkySessionBootstrapResult (
    /**
     * Session access material to persist in platform session storage.
     */
    val `sessionAccess`: PubkySessionAccess,
    /**
     * Local Pubky public key.
     */
    val `publicKey`: kotlin.String
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`sessionAccess`,
            this.`publicKey`,
        )
    }
    public companion object
}



/**
 * Result of resolving public Payment Endpoints for one counterparty.
 */

public data class PublicContactPaymentResolution (
    /**
     * Public payment resolution outcome.
     */
    val `status`: PublicPaymentResolutionStatus,
    /**
     * Payable public Payment Endpoints in adapter-preferred order.
     */
    val `payableEndpoints`: List<ResolvedPublicPaymentEndpoint>
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`status`,
            this.`payableEndpoints`,
        )
    }
    public companion object
}



/**
 * Public Payment Endpoint candidate passed to the payment adapter.
 */

public data class PublicPaymentEndpointCandidate (
    /**
     * Opaque candidate id for this callback request.
     */
    val `candidateId`: kotlin.String,
    /**
     * Counterparty that published the endpoint.
     */
    val `counterparty`: kotlin.String,
    /**
     * Counterparty Paykit receiver path.
     */
    val `counterpartyReceiverPath`: kotlin.String,
    /**
     * Payment Endpoint Identifier string.
     */
    val `identifier`: kotlin.String,
    /**
     * Serialized endpoint payload.
     */
    val `payload`: PaymentPayload
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`candidateId`,
            this.`counterparty`,
            this.`counterpartyReceiverPath`,
            this.`identifier`,
            this.`payload`,
        )
    }
    public companion object
}



/**
 * Request passed to the payment adapter for public endpoint ordering.
 */

public data class PublicPaymentEndpointSelectionRequest (
    /**
     * Counterparty being paid.
     */
    val `counterparty`: kotlin.String,
    /**
     * Counterparty Paykit receiver path.
     */
    val `counterpartyReceiverPath`: kotlin.String,
    /**
     * Optional amount context.
     */
    val `amount`: PaymentAmountContext?,
    /**
     * Public candidate endpoints in SDK preference order.
     */
    val `candidates`: List<PublicPaymentEndpointCandidate>
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`counterparty`,
            this.`counterpartyReceiverPath`,
            this.`amount`,
            this.`candidates`,
        )
    }
    public companion object
}



/**
 * Payment-method-specific receiving detail for public publication.
 */

public data class PublicReceivingDetail (
    /**
     * Payment Endpoint Identifier string.
     */
    val `identifier`: kotlin.String,
    /**
     * Serialized endpoint payload.
     */
    val `payload`: PaymentPayload
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
 * Queued outbound private message summary.
 */

public data class QueuedPrivateMessage (
    /**
     * Assigned outbound message id.
     */
    val `outboundMessageId`: kotlin.ULong,
    /**
     * Counterparty public key.
     */
    val `counterparty`: kotlin.String,
    /**
     * Counterparty Paykit receiver path.
     */
    val `counterpartyReceiverPath`: kotlin.String,
    /**
     * Private Message Kind string.
     */
    val `kind`: kotlin.String,
    /**
     * Delivery status.
     */
    val `status`: OutboundPrivateMessageStatus,
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
    val `lastError`: PrivateOperationError?
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`outboundMessageId`,
            this.`counterparty`,
            this.`counterpartyReceiverPath`,
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

public data class ReceiptAccessView (
    /**
     * Counterparty that sent the Receipt Access event.
     */
    val `counterparty`: kotlin.String,
    /**
     * Counterparty Paykit receiver path.
     */
    val `counterpartyReceiverPath`: kotlin.String,
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
    val `paymentReference`: PaymentReference,
    /**
     * Optional Payment Request ID copied from Receipt Access.
     */
    val `paymentRequestId`: kotlin.String?,
    /**
     * Optional Billing Period copied from Receipt Access.
     */
    val `billingPeriod`: BillingPeriod?,
    /**
     * Current retrieval state for the referenced receipt.
     */
    val `retrievalStatus`: ReceiptRetrievalStatus,
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
            this.`counterpartyReceiverPath`,
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
public data class ReceiptAmount (
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

public data class ReceiptDraft (
    /**
     * Optional caller-stable Receipt ID.
     */
    val `receiptId`: kotlin.String?,
    /**
     * Payment Reference being receipted.
     */
    val `paymentReference`: PaymentReference,
    /**
     * Optional Payment Request ID this receipt corresponds to.
     */
    val `paymentRequestId`: kotlin.String?,
    /**
     * Optional Billing Period for recurring Payment Request receipts.
     */
    val `billingPeriod`: BillingPeriod?,
    /**
     * Optional Payment Endpoint Identifier used for the payment.
     */
    val `paymentEndpointIdentifier`: kotlin.String?,
    /**
     * Optional Payment Amount being receipted.
     */
    val `amount`: ReceiptAmount?,
    /**
     * Caller-defined Receipt Metadata encoded as a JSON object.
     */
    val `metadata`: PrivateJsonObject
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`receiptId`,
            this.`paymentReference`,
            this.`paymentRequestId`,
            this.`billingPeriod`,
            this.`paymentEndpointIdentifier`,
            this.`amount`,
            this.`metadata`,
        )
    }
    public companion object
}



/**
 * App-facing view of local receipt issuance progress.
 */

public data class ReceiptIssuanceView (
    /**
     * Counterparty that should receive Receipt Access.
     */
    val `counterparty`: kotlin.String,
    /**
     * Counterparty Paykit receiver path.
     */
    val `counterpartyReceiverPath`: kotlin.String,
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
    val `paymentReference`: PaymentReference,
    /**
     * Optional Payment Request ID copied from the Receipt.
     */
    val `paymentRequestId`: kotlin.String?,
    /**
     * Optional Billing Period copied from the Receipt.
     */
    val `billingPeriod`: BillingPeriod?,
    /**
     * Optional Payment Endpoint Identifier copied from the Receipt.
     */
    val `paymentEndpointIdentifier`: kotlin.String?,
    /**
     * Optional Payment Amount copied from the Receipt.
     */
    val `amount`: ReceiptAmount?,
    /**
     * Current issuance status.
     */
    val `status`: ReceiptIssuanceStatus,
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
            this.`counterpartyReceiverPath`,
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

public data class ReceiptRecord (
    /**
     * Counterparty that issued the Receipt Access event.
     */
    val `issuer`: kotlin.String,
    /**
     * Issuer Paykit receiver path.
     */
    val `issuerReceiverPath`: kotlin.String,
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
    val `paymentReference`: PaymentReference,
    /**
     * Optional Payment Request ID copied from the decrypted Receipt.
     */
    val `paymentRequestId`: kotlin.String?,
    /**
     * Optional Billing Period copied from the decrypted Receipt.
     */
    val `billingPeriod`: BillingPeriod?,
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
    val `amount`: ReceiptAmount?,
    /**
     * Caller-defined Receipt Metadata encoded as a JSON object.
     */
    val `metadata`: PrivateJsonObject,
    /**
     * Successful retrieval/decryption time as RFC3339 text.
     */
    val `retrievedAt`: kotlin.String
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`issuer`,
            this.`issuerReceiverPath`,
            this.`receiptAccessEventId`,
            this.`receiptId`,
            this.`paymentReference`,
            this.`paymentRequestId`,
            this.`billingPeriod`,
            this.`recipientPublicKey`,
            this.`paymentEndpointIdentifier`,
            this.`amount`,
            this.`metadata`,
            this.`retrievedAt`,
        )
    }
    public companion object
}



/**
 * Failed recovery marker publication during outbound private send recovery.
 */

public data class RecoveryMarkerPublishFailure (
    /**
     * Outbound message id that triggered recovery, when available.
     */
    val `outboundMessageId`: kotlin.ULong?,
    /**
     * Recovery marker publication error.
     */
    val `error`: PrivateOperationError
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

public data class ReservationCleanupFailure (
    /**
     * Reservation id, when the failure is tied to a specific reservation.
     */
    val `reservationId`: kotlin.String?,
    /**
     * Cleanup error.
     */
    val `error`: PrivateOperationError
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
 * Private Payment Endpoint paired with its adapter-built payment target.
 */

public data class ResolvedPrivatePaymentEndpoint (
    /**
     * Counterparty that privately shared the endpoint.
     */
    val `counterparty`: kotlin.String,
    /**
     * Counterparty Paykit receiver path.
     */
    val `counterpartyReceiverPath`: kotlin.String,
    /**
     * Payment Endpoint Identifier string.
     */
    val `identifier`: kotlin.String,
    /**
     * Serialized endpoint payload.
     */
    val `payload`: PaymentPayload,
    /**
     * Adapter-built target for executing payment through this endpoint.
     */
    val `target`: PaymentTarget
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`counterparty`,
            this.`counterpartyReceiverPath`,
            this.`identifier`,
            this.`payload`,
            this.`target`,
        )
    }
    public companion object
}



/**
 * Public Payment Endpoint paired with its adapter-built payment target.
 */

public data class ResolvedPublicPaymentEndpoint (
    /**
     * Counterparty that published the endpoint.
     */
    val `counterparty`: kotlin.String,
    /**
     * Counterparty Paykit receiver path.
     */
    val `counterpartyReceiverPath`: kotlin.String,
    /**
     * Payment Endpoint Identifier string.
     */
    val `identifier`: kotlin.String,
    /**
     * Serialized endpoint payload.
     */
    val `payload`: PaymentPayload,
    /**
     * Adapter-built target for executing payment through this endpoint.
     */
    val `target`: PaymentTarget
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`counterparty`,
            this.`counterpartyReceiverPath`,
            this.`identifier`,
            this.`payload`,
            this.`target`,
        )
    }
    public companion object
}



/**
 * Receiver-scoped peer restored as recovery-required.
 */
@kotlinx.serialization.Serializable
public data class RestoreRecoveryRequiredPeer (
    /**
     * Counterparty app public key.
     */
    val `counterparty`: kotlin.String,
    /**
     * Counterparty receiver/runtime folder.
     */
    val `counterpartyReceiverPath`: kotlin.String
) {
    public companion object
}



/**
 * Report returned after restoring SDK-managed backup state.
 */
@kotlinx.serialization.Serializable
public data class RestoreReport (
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
     * Receiver-scoped peers restored as recovery-required.
     */
    val `recoveryRequiredPeers`: List<RestoreRecoveryRequiredPeer>
) {
    public companion object
}



/**
 * Current SDK state blob with its platform storage revision.
 */

public data class SdkStateBlobSnapshot (
    /**
     * Encoded SDK state.
     */
    val `blob`: SdkStateBlob,
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
 * Source used for a resolved contact profile.
 */

@kotlinx.serialization.Serializable
public enum class ContactProfileSource {

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
public enum class EncryptedLinkHandshakeRole {

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
public enum class EncryptedLinkRecoveryMarkerPolicy {

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
public enum class EndpointManagementScope {

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
public enum class LinkedPeerState {

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
public enum class OutboundPrivateMessageStatus {

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
 * SDK-derived Payment Request lifecycle state.
 */

@kotlinx.serialization.Serializable
public enum class PaymentRequestLifecycleState {

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
public enum class PaymentRequestLocalRole {

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
 * Encrypted Link and Private Payment List state observed during resolution.
 */

@kotlinx.serialization.Serializable
public enum class PrivatePaymentResolutionState {

    /**
     * Private Payment List candidates were available for resolution.
     */
    AVAILABLE,
    /**
     * No Private Payment List candidate was available.
     */
    NO_PRIVATE_ENDPOINT,
    /**
     * Private payment state is blocked by Encrypted Link recovery.
     */
    RECOVERY_PENDING,
    /**
     * SDK returned a value this binding version does not understand.
     */
    UNKNOWN;
    public companion object
}






/**
 * Result category for private Payment Endpoint resolution.
 */

@kotlinx.serialization.Serializable
public enum class PrivatePaymentResolutionStatus {

    /**
     * A payable private Payment Endpoint was found.
     */
    PAYABLE,
    /**
     * No private Payment Endpoint was found.
     */
    NO_ENDPOINT,
    /**
     * Private Payment Endpoints exist but are unsupported.
     */
    UNSUPPORTED_ENDPOINT,
    /**
     * No Private Payment List newer than the caller's consumed version is available.
     */
    WAITING_FOR_UPDATED_PAYMENT_LIST,
    /**
     * SDK returned a value this binding version does not understand.
     */
    UNKNOWN;
    public companion object
}






/**
 * Reservation callback result kind.
 */

@kotlinx.serialization.Serializable
public enum class PrivateReceivingDetailReservationResponseKind {

    /**
     * Use `current_private_receiving_details` for this private list.
     */
    USE_CURRENT_RECEIVING_DETAILS,
    /**
     * Use the reservations carried by this response, including an empty list.
     */
    RESERVATIONS,
    /**
     * Reserved invalid response kind.
     */
    UNKNOWN;
    public companion object
}







/**
 * Failure returned while approving Pubky Auth with a companion claim.
 */
public sealed class PubkyAuthCompanionClaimApprovalException: kotlin.Exception() {

    /**
     * The URL, claim type, secret, relay, or capability request is invalid.
     */
    public class InvalidAuthUrl(
        public val `reason`: kotlin.String,
    ) : PubkyAuthCompanionClaimApprovalException() {
        override val message: String
            get() = "reason=${ `reason` }"
    }

    /**
     * The companion claim description is invalid.
     */
    public class InvalidClaim(
        public val `reason`: kotlin.String,
    ) : PubkyAuthCompanionClaimApprovalException() {
        override val message: String
            get() = "reason=${ `reason` }"
    }

    /**
     * The supplied local Pubky identity key is invalid.
     */
    public class InvalidLocalSecretKey(
        public val `reason`: kotlin.String,
    ) : PubkyAuthCompanionClaimApprovalException() {
        override val message: String
            get() = "reason=${ `reason` }"
    }

    /**
     * XSalsa20-Poly1305 encryption failed before relay delivery.
     */
    public class EncryptionFailure(
        public val `reason`: kotlin.String,
    ) : PubkyAuthCompanionClaimApprovalException() {
        override val message: String
            get() = "reason=${ `reason` }"
    }

    /**
     * The encrypted companion claim could not be delivered to its relay channel.
     */
    public class RelayDeliveryFailure(
        public val `reason`: kotlin.String,
    ) : PubkyAuthCompanionClaimApprovalException() {
        override val message: String
            get() = "reason=${ `reason` }"
    }

    /**
     * Normal Pubky Auth approval failed after companion delivery succeeded.
     */
    public class AuthorizationFailure(
        public val `reason`: kotlin.String,
    ) : PubkyAuthCompanionClaimApprovalException() {
        override val message: String
            get() = "reason=${ `reason` }"
    }

    /**
     * An unknown SDK failure occurred; no claim-delivery state is implied.
     */
    public class Unexpected(
        public val `reason`: kotlin.String,
    ) : PubkyAuthCompanionClaimApprovalException() {
        override val message: String
            get() = "reason=${ `reason` }"
    }

}




/**
 * Kind of Pubky auth request represented by a deep link.
 */

@kotlinx.serialization.Serializable
public enum class PubkyAuthRequestKind {

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
 * Pubky network environment used by binding-layer clients.
 */

@kotlinx.serialization.Serializable
public enum class PubkyClientEnvironment {

    /**
     * Use the public Pubky network.
     */
    PRODUCTION,
    /**
     * Use standard Pubky testnet ports, on localhost unless a host is configured.
     */
    LOCAL_TESTNET,
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
public enum class PublicContactSharingPolicy {

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
 * Result category for public Payment Endpoint resolution.
 */

@kotlinx.serialization.Serializable
public enum class PublicPaymentResolutionStatus {

    /**
     * A payable public Payment Endpoint was found.
     */
    PAYABLE,
    /**
     * No public Payment Endpoint was found.
     */
    NO_ENDPOINT,
    /**
     * Public Payment Endpoints exist but are unsupported.
     */
    UNSUPPORTED_ENDPOINT,
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
public enum class PublicationStatus {

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
public enum class ReceiptIssuanceStatus {

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
public enum class ReceiptRetrievalStatus {

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
 * Error type exposed through generated bindings.
 */
public sealed class PaykitException: kotlin.Exception() {

    /**
     * Durable storage failed.
     */
    public class Storage(
        public val `code`: kotlin.String,
        public val `context`: kotlin.String,
    ) : PaykitException() {
        override val message: String
            get() = "code=${ `code` }, context=${ `context` }"
    }

    /**
     * Pubky identity, session, or key capability failed.
     */
    public class Identity(
        public val `code`: kotlin.String,
        public val `context`: kotlin.String,
    ) : PaykitException() {
        override val message: String
            get() = "code=${ `code` }, context=${ `context` }"
    }

    /**
     * Pubky or Encrypted Link transport failed.
     */
    public class Transport(
        public val `code`: kotlin.String,
        public val `context`: kotlin.String,
    ) : PaykitException() {
        override val message: String
            get() = "code=${ `code` }, context=${ `context` }"
    }

    /**
     * Requested Paykit or Pubky resource was not found.
     */
    public class NotFound(
        public val `code`: kotlin.String,
        public val `context`: kotlin.String,
    ) : PaykitException() {
        override val message: String
            get() = "code=${ `code` }, context=${ `context` }"
    }

    /**
     * Paykit protocol data is invalid, conflicting, or unsupported.
     */
    public class Protocol(
        public val `code`: kotlin.String,
        public val `context`: kotlin.String,
    ) : PaykitException() {
        override val message: String
            get() = "code=${ `code` }, context=${ `context` }"
    }

    /**
     * Operation is blocked by configured SDK policy.
     */
    public class Policy(
        public val `code`: kotlin.String,
        public val `context`: kotlin.String,
    ) : PaykitException() {
        override val message: String
            get() = "code=${ `code` }, context=${ `context` }"
    }

    /**
     * Payment adapter failed.
     */
    public class PaymentAdapter(
        public val `code`: kotlin.String,
        public val `context`: kotlin.String,
    ) : PaykitException() {
        override val message: String
            get() = "code=${ `code` }, context=${ `context` }"
    }

    /**
     * Local state needs explicit recovery before automation can continue.
     */
    public class RecoveryRequired(
        public val `code`: kotlin.String,
        public val `context`: kotlin.String,
    ) : PaykitException() {
        override val message: String
            get() = "code=${ `code` }, context=${ `context` }"
    }

}
