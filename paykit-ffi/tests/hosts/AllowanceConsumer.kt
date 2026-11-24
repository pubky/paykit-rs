package com.synonym.paykit.tests

import com.synonym.paykit.*
import java.util.UUID

private suspend fun allowanceUsageRefusal(call: suspend () -> Unit) {
    try {
        call()
        error("expected an Allowance usage refusal")
    } catch (error: WalletException.NotSent) {
        error.failure.use { check(it.code() == "usage") }
    }
}

private suspend fun allowanceNotFound(call: suspend () -> Unit) {
    try {
        call()
        error("expected an absent Allowance")
    } catch (error: WalletException.Permanent) {
        error.failure.use { check(it.code() == "not_found") }
    }
}

private suspend fun allowanceNotNow(call: suspend () -> Unit) {
    try {
        call()
        error("expected an Allowance to be unavailable")
    } catch (error: WalletException.NeedsUser) {
        error.failure.use { check(it.code() == "not_now") }
    }
}

private suspend fun NativeWallet.allowanceIds() = allowances().map { view -> view.use { it.allowanceId } }

// Propose, inspect and withdraw the wallet's own pending Allowances (Paykit spec 8.3.1).
internal suspend fun checkAllowances(fixture: CheckoutFixture, wallet: NativeWallet) {
    val peer = fixture.payee()
    val terms = AllowanceTerms(
        asset = "btc", perPaymentAmount = PerPaymentRange(Amount("btc", "0.01"), Amount("btc", "2.00")),
        periodLimits = listOf(PeriodLimit(null, 3uL, LimitPeriod.Rolling(1uL, "week"))),
        lifetimeAmountLimit = Amount("btc", "10.0"), activeFrom = null, expiresAt = null,
        allowedPaymentEndpointIdentifiers = listOf("btc-lightning-bolt11"),
    )

    // Read the unlisted backup in a separate store, before its first receive (Paykit spec 6).
    fixture.hostForStore(EnrollmentStore()).use { host ->
        host.restore(wallet.appId(), wallet.backup(), BackupDevice.IN_POSSESSION).use { restored ->
            try {
                allowanceNotNow { restored.allowances() }
            } finally {
                restored.shutdown()
            }
        }
    }
    val before = wallet.allowanceIds()
    try {
        wallet.proposeExecutableAllowance(peer, terms, false)
        error("proposed automatic payments without confirmation")
    } catch (error: WalletException.InvalidInput) {
        check(error.field == "user_confirmed")
    }

    val proposals = mutableListOf<SentAllowance>()
    try {
        // Eight own pending proposals to one counterparty fill its limit (Paykit spec 8.3.1).
        repeat(8) { index ->
            val role = if (index == 0) AllowanceRole.ALLOWER else AllowanceRole.ALLOWEE
            val sent = if (role == AllowanceRole.ALLOWEE) {
                wallet.proposeAllowance(peer, role, terms)
            } else {
                wallet.proposeExecutableAllowance(peer, terms, true)
            }
            proposals.add(sent)
            check(sent.record.size == 32 && UUID.fromString(sent.allowanceId).version() == 4)
            check(UUID.fromString(sent.msgId).version() == 4)
            check(wallet.messageDelivery(sent.msgId) == sent.delivery)
            wallet.allowance(sent.record).use { view ->
                check(view.allowanceId == sent.allowanceId && view.record.contentEquals(sent.record))
                check(view.counterparty.contentEquals(peer) && view.proposedHere && view.role == role)
                check(view.status == AllowanceStatus.PROPOSED && view.terms == terms)
                check(!view.consented && !view.confirmationNeeded && view.executorAppId == null)
                check(view.blocked == false && view.hold.record().contentEquals(sent.record))
            }
        }
        val listed = wallet.allowanceIds().toSet()
        check(listed == (before + proposals.map { it.allowanceId }).toSet())
        allowanceNotNow { wallet.proposeAllowance(peer, AllowanceRole.ALLOWEE, terms) }
        allowanceNotNow { wallet.proposeExecutableAllowance(peer, terms, true) }

        val record = proposals.first().record
        allowanceUsageRefusal { wallet.acceptAllowance(record) }
        allowanceUsageRefusal { wallet.acceptExecutableAllowance(record, true) }
        try {
            wallet.acceptExecutableAllowance(record, false)
            error("accepted automatic payments without confirmation")
        } catch (error: WalletException.InvalidInput) {
            check(error.field == "user_confirmed")
        }
        allowanceUsageRefusal { wallet.rejectAllowance(record) }
        allowanceUsageRefusal { wallet.discardPendingAllowance(record) }
    } finally {
        for (sent in proposals) {
            val end = wallet.endAllowance(sent.record)
            check(end != sent.msgId && UUID.fromString(end).version() == 4)
        }
    }
    val withdrawn = proposals.first().record
    allowanceNotFound { wallet.endAllowance(withdrawn) }
    allowanceNotFound { wallet.allowance(withdrawn) }
    check(wallet.allowanceIds().toSet() == before.toSet())
}
