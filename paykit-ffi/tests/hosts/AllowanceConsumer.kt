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

private suspend fun allowancesNotListed(call: suspend () -> Unit) {
    try {
        call()
        error("expected the Allowances to be unlisted")
    } catch (error: WalletException.RetryLater) {
        error.failure.use { check(it.code() == "records_not_listed") }
    }
}

private suspend fun pendingProposalsFull(call: suspend () -> Unit) {
    try {
        call()
        error("expected the pending proposals to be full")
    } catch (error: WalletException.NeedsUser) {
        error.failure.use { check(it.code() == "pending_proposals_full") }
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
                allowancesNotListed { restored.allowances() }
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
        pendingProposalsFull { wallet.proposeAllowance(peer, AllowanceRole.ALLOWEE, terms) }
        pendingProposalsFull { wallet.proposeExecutableAllowance(peer, terms, true) }

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

    // The shop proposes in each role. The wallet accepts each and ends it, then rejects a third
    // proposal (Paykit spec 8.3.1).
    suspend fun incoming(role: AllowanceRole): ByteArray {
        val id = fixture.proposeAllowance(role)
        wallet.finishReceive()
        val views = wallet.allowances()
        try {
            val view = views.single { it.allowanceId == id }
            check(!view.proposedHere && view.role == role && view.status == AllowanceStatus.PROPOSED)
            check(view.counterparty.contentEquals(peer) && view.terms == terms)
            return view.record
        } finally {
            views.forEach { it.destroy() }
        }
    }
    suspend fun status(record: ByteArray) = wallet.allowance(record).use { it.status }
    for (role in listOf(AllowanceRole.ALLOWEE, AllowanceRole.ALLOWER)) {
        val record = incoming(role)
        val accepted = if (role == AllowanceRole.ALLOWER) {
            wallet.acceptExecutableAllowance(record, true)
        } else {
            wallet.acceptAllowance(record)
        }
        check(UUID.fromString(accepted).version() == 4 && status(record) == AllowanceStatus.ACCEPTED)
        val ended = wallet.endAllowance(record)
        check(UUID.fromString(ended).version() == 4 && status(record) == AllowanceStatus.ENDED)
    }
    val declined = incoming(AllowanceRole.ALLOWER)
    // A rejected proposal is removed; a second rejection finds nothing (Paykit spec 8.3.1).
    check(UUID.fromString(wallet.rejectAllowance(declined)).version() == 4)
    allowanceNotFound { wallet.allowance(declined) }
    allowanceNotFound { wallet.rejectAllowance(declined) }
}
