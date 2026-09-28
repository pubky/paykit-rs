package com.synonym.paykit

// Compile-only consumer fixture for conversion quotes and payment deadlines.
@Suppress("UNUSED_VARIABLE")
internal suspend fun compilePaymentConversionBindingsSurface(
    sdk: PaykitSdkInterface,
    counterparty: String,
    receiverPath: String,
    requestId: String,
) {
    val rates = listOf(ConversionRate(asset = "usdt", value = "1"))
    val fixed = PaymentConversion.Fixed(rates)
    val recurring = PaymentConversion.PerPeriod
    val period = BillingPeriod(startsAt = "2026-10-01T00:00:00Z", endsAt = "2026-11-01T00:00:00Z")
    val deadline = paymentDeadlineAt(PaymentDeadline.PeriodStart(86400UL), period)
    val record = sdk.quotePaymentRequest(counterparty, receiverPath, requestId, period, rates, deadline)
    val quotes: List<PaymentConversionQuoteRecord> = record.conversionQuotes
    val proof = PaymentProofSubmission(
        billingPeriod = period, paymentEndpointIdentifier = "usdt-arbitrum-address",
        allowanceId = null, conversionQuoteId = quotes.firstOrNull()?.eventId,
        proof = PrivateJsonObject(text = "{}"),
    )
}
