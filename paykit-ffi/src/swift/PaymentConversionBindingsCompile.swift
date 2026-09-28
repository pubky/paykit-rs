// Compile-only consumer fixture for conversion quotes and payment deadlines.
private func compilePaymentConversionBindingsSurface(
    sdk: PaykitSdkProtocol,
    counterparty: String,
    receiverPath: String,
    requestId: String
) async throws {
    let rates = [ConversionRate(asset: "usdt", value: "1")]
    let fixed = PaymentConversion.fixed(rates: rates)
    let recurring = PaymentConversion.perPeriod
    let period = BillingPeriod(startsAt: "2026-10-01T00:00:00Z", endsAt: "2026-11-01T00:00:00Z")
    let deadline = try paymentDeadlineAt(deadline: .periodStart(seconds: 86400), billingPeriod: period)
    let record = try await sdk.quotePaymentRequest(
        counterparty: counterparty, counterpartyReceiverPath: receiverPath,
        paymentRequestId: requestId, billingPeriod: period, rates: rates, expiresAt: deadline
    )
    let quotes: [PaymentConversionQuoteRecord] = record.conversionQuotes
    let proof = PaymentProofSubmission(
        billingPeriod: period, paymentEndpointIdentifier: "usdt-arbitrum-address",
        allowanceId: nil, conversionQuoteId: quotes.first?.eventId,
        proof: try PrivateJsonObject(text: "{}")
    )
    _ = (fixed, recurring, proof)
}
