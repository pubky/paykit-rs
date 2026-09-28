package com.synonym.paykit

// Compile-only consumer fixture for conversion quotes and payment deadlines.
@Suppress("UNUSED_VARIABLE")
internal suspend fun compilePaymentConversionBindingsSurface(
    sdk: PaykitSdkInterface,
    counterparty: String,
    paymentAppId: String,
    requestId: String,
) {
    val rates = listOf(ConversionRate(asset = "usdt", value = "1"))
    val fixed = PaymentConversion.Fixed(rates)
    val recurring = PaymentConversion.PerPeriod
    val period = BillingPeriod(startsAt = "2026-10-01T00:00:00Z", endsAt = "2026-11-01T00:00:00Z")
    val deadline = paymentDeadlineAt(PaymentDeadline.PeriodStart(86400UL), period)
    val record = sdk.quotePaymentRequest(counterparty, requestId, period, rates, deadline)
    val quotes: List<PaymentConversionQuoteRecord> = record.conversionQuotes
    val proofBody = PrivateJsonObject(
        text = """{"type":"erc20-transfer-eip712","receipt_log_index":"115792089237316195423570985008687907853269984665640564039457584007913129639935"}""",
    )
    val proof = PaymentProofSubmission(
        billingPeriod = period, paymentAppId = paymentAppId,
        paymentEndpointIdentifier = "usdt-arbitrum-address",
        allowanceId = null, conversionQuoteId = quotes.firstOrNull()?.eventId,
        proof = proofBody,
    )
}
