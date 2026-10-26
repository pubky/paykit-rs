package com.synonym.paykit

// Compile-only consumer fixture for preparation claims and returned request state.
@Suppress("UNUSED_VARIABLE")
internal suspend fun compilePaymentRequestPreparationBindingsSurface(
    sdk: PaykitSdkInterface,
    counterparty: String,
    requestId: String
) {
    val record: PaymentRequestRecord = sdk.claimPaymentRequestForPreparation(counterparty, requestId)
    val owner: String? = record.executionClaimAppId
    val terms: PaymentRequestTerms? = record.terms
    val proposal: ULong? = record.proposalStreamItemId
    val proofs: List<PaymentProofRecord> = record.paymentProofs
}
