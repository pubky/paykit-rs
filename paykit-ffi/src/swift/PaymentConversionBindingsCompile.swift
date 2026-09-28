// Compile-only consumer fixture for conversion quotes and payment deadlines.
private func compilePaymentConversionBindingsSurface(
    sdk: PaykitSdkProtocol,
    counterparty: String,
    paymentAppId: String,
    requestId: String
) async throws {
    let rates = [ConversionRate(asset: "usdt", value: "1")]
    let fixed = PaymentConversion.fixed(rates: rates)
    let recurring = PaymentConversion.perPeriod
    let period = BillingPeriod(startsAt: "2026-10-01T00:00:00Z", endsAt: "2026-11-01T00:00:00Z")
    let deadline = try paymentDeadlineAt(deadline: .periodStart(seconds: 86400), billingPeriod: period)
    let record = try await sdk.quotePaymentRequest(
        counterparty: counterparty,
        paymentRequestId: requestId, billingPeriod: period, rates: rates, expiresAt: deadline
    )
    let quotes: [PaymentConversionQuoteRecord] = record.conversionQuotes
    let proofBody = try PrivateJsonObject(
        text: #"{"type":"erc20-transfer-eip712","receipt_log_index":"115792089237316195423570985008687907853269984665640564039457584007913129639935"}"#
    )
    let proof = PaymentProofSubmission(
        billingPeriod: period, paymentAppId: paymentAppId,
        paymentEndpointIdentifier: "usdt-arbitrum-address",
        allowanceId: nil, conversionQuoteId: quotes.first?.eventId,
        proof: proofBody
    )
    _ = (fixed, recurring, proof)
}
