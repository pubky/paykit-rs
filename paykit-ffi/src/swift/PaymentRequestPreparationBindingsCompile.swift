// Compile-only consumer fixture for preparation claims and returned request state.
private func compilePaymentRequestPreparationBindingsSurface(
    sdk: PaykitSdkProtocol,
    counterparty: String,
    requestId: String
) async throws {
    let record: PaymentRequestRecord = try await sdk.claimPaymentRequestForPreparation(
        counterparty: counterparty, paymentRequestId: requestId
    )
    let owner: String? = record.executionClaimAppId
    let terms: PaymentRequestTerms? = record.terms
    let proposal: UInt64? = record.proposalStreamItemId
    let proofs: [PaymentProofRecord] = record.paymentProofs
    _ = (owner, terms, proposal, proofs)
}
