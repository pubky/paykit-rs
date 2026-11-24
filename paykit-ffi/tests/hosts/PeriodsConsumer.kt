package com.synonym.paykit.tests

import com.synonym.paykit.*

// A wallet with no recurring request can list its subscriptions (Paykit spec 8.2).
internal suspend fun checkPeriods(fixture: CheckoutFixture, wallet: NativeWallet) {
    // Payer reads wait for the wallet's first full read (Paykit spec 6).
    wallet.finishReceive()
    check(wallet.recurringRequests().isEmpty())
    val key = ByteArray(32)
    val missing = RequestId(payee = fixture.payee(), requestHash = key)
    val period = BillingPeriod(startsAt = 0uL, endsAt = fixture.nowMs())
    val details = PeriodDetails(
        billingPeriod = period,
        paymentEndpoints = listOf(ListEndpoint("btc-lightning-bolt11", "lnbc1period")),
        rates = null, expiresAtMs = period.endsAt,
    )
    suspend fun missingRecord(call: suspend () -> Any) {
        try {
            call()
            error("sent a message for a missing record")
        } catch (error: WalletException.Permanent) {
            error.failure.use { check(it.code() == "not_found") }
        }
    }
    missingRecord { wallet.sendPeriodDetails(key, details) }
    missingRecord { wallet.acceptSubscription(missing, null) }
    missingRecord { wallet.acceptSubscription(missing, AcceptOptions(null, null)) }
    missingRecord { wallet.acceptSubscriptionUnderAllowance(missing, key) }

    fun refused(result: PaymentResult) {
        check(result.publicFailures.isEmpty())
        val decision = result.decision as PaymentDecision.Error
        (decision.error as WalletException.Permanent).failure.use { check(it.code() == "not_found") }
    }
    refused(wallet.payPeriod(missing, period, PaymentOptions(null, null, false, false, emptyList())))
    refused(wallet.payPeriodAutomatically(missing, period, PeriodPriceChoice.RequestedAmount))
    refused(wallet.payPeriodUnderAllowance(missing, period))

    // Reading a restored copy before its first listing leaves the active wallet alone (Switch spec 12.4).
    fixture.hostForStore(EnrollmentStore()).use { host ->
        host.restore(wallet.appId(), wallet.backup(), BackupDevice.IN_POSSESSION).use { restored ->
            try {
                restored.recurringRequests()
                error("listed recurring requests before the restored records were read")
            } catch (error: WalletException.RetryLater) {
                error.failure.use { check(it.code() == "records_not_listed") }
            } finally {
                restored.shutdown()
            }
        }
    }

    // The shop's recurring checkout is listed, accepted once and cancelled again (Paykit spec 8.2).
    val id = fixture.sendRecurringCheckout()
    wallet.finishReceive()
    val listed = wallet.recurringRequests().single { it.id.requestHash.contentEquals(id.requestHash) }
    check(listed.status == RecurringRequestStatus.OPEN && listed.amount == Amount("btc", "7"))
    check(listed.recurrence.every == 1uL && listed.recurrence.unit == RecurrenceUnit.DAY && listed.periods.isEmpty())
    check(listed.paymentReference == "checkout-7" && listed.record.size == 32 && !listed.manualOnly)
    val acceptance = wallet.acceptSubscription(id, null)
    check(java.util.UUID.fromString(acceptance.msgId).version() == 4)
    check(acceptance.delivery is MessageDelivery.Accepted && wallet.messageDelivery(acceptance.msgId) == acceptance.delivery)
    check(wallet.recurringRequests().single { it.id.requestHash.contentEquals(id.requestHash) }.status ==
        RecurringRequestStatus.SUBSCRIBED)
    try {
        wallet.acceptSubscription(id, null)
        error("accepted a subscription twice")
    } catch (error: WalletException.Permanent) {
        error.failure.use { check(it.code() == "subscribed") }
    }
    wallet.cancelRequest(id, "test subscription")
    check(wallet.recurringRequests().single { it.id.requestHash.contentEquals(id.requestHash) }.status ==
        RecurringRequestStatus.CANCELLED)
}
