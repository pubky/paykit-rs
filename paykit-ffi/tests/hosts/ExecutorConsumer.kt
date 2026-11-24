package com.synonym.paykit.tests

import com.synonym.paykit.*

// This account has never dispatched a payment (Paykit spec 8.3).
private class EmptyExecutorAccount : ReceiptReader() {
    override suspend fun allowanceHistory(payer: ByteArray): List<MethodDispatch> = emptyList()
}

private suspend fun checkExecutorCheckout(
    fixture: CheckoutFixture, wallet: NativeWallet, account: EmptyExecutorAccount?,
) {
    wallet.finishReceive()
    check(wallet.ledgerAlarm() == null)
    val id = fixture.proposeAllowance(AllowanceRole.ALLOWER)
    wallet.finishReceive()
    val views = wallet.allowances()
    val record = try {
        val offer = views.single { it.allowanceId == id }
        check(offer.role == AllowanceRole.ALLOWER && !offer.proposedHere && offer.status == AllowanceStatus.PROPOSED)
        offer.record
    } finally {
        views.forEach { it.destroy() }
    }
    wallet.acceptExecutableAllowance(record, true)
    try {
        wallet.resumeAllowanceExecutor(record)
        wallet.allowance(record).use { view ->
            check(view.status == AllowanceStatus.ACCEPTED && view.consented)
            check(view.executorAppId?.contentEquals(wallet.appId()) == true)
            try {
                wallet.confirmAllowanceExecutor(view.hold, false)
                error("renewed executor consent without confirmation")
            } catch (error: WalletException.InvalidInput) {
                check(error.field == "user_confirmed")
            }
            wallet.confirmAllowanceExecutor(view.hold, true)
        }
        val request = fixture.sendCheckout()
        wallet.finishReceive()
        val result = wallet.payUnderAllowance(request, record)
        check(result.publicFailures.isEmpty())
        val refused = (result.decision as PaymentDecision.Error).error
        if (account == null) {
            (refused as WalletException.NeedsUser).failure.use { check(it.code() == "allowance_execution_held") }
        } else {
            (refused as WalletException.Permanent).failure.use { check(it.code() == "not_automatic") }
            check(account.transfers == 0)
        }
        check(wallet.payments().none { it.requestHash?.contentEquals(request.requestHash) == true })
    } finally {
        wallet.endAllowance(record)
    }
    wallet.allowance(record).use { check(it.status == AllowanceStatus.ENDED) }
}

// The shared wallet lacks account-wide history. A fresh account can verify its empty ledger
// and refuse the same checkout at the per-payment limit (Paykit spec 8.3).
internal suspend fun checkExecutor(fixture: CheckoutFixture, wallet: NativeWallet) {
    checkExecutorCheckout(fixture, wallet, null)
    val account = EmptyExecutorAccount()
    val store = EnrollmentStore()
    enrolledTestCheckout(store, account, testDeclaration, testEndpoints).use { checkout ->
        val reference = ByteArray(16) { 31 }
        val secret = ByteArray(32) { 9 }
        val scopes = WalletScopes(pay = true, allowance = true, allowanceExecute = true, manageContacts = true)
        WalletEnrollment(store).use { owner ->
            val info = owner.beginEnrollment(reference)
            val original = owner.prepareEnrollment(reference, "wallet.example", ByteArray(32) { 8 },
                info.appRecordPath + ":rw", secret, scopes)
            owner.acceptApproval(reference, checkout.identity(), checkout.approve(original.signedRequest, secret))
        }
        checkout.host().use { host ->
            host.completeEnrollment(reference).use { payer ->
                try {
                    payer.allowConnect(checkout.payee())
                    checkExecutorCheckout(checkout, payer, account)
                } finally {
                    payer.shutdown()
                }
            }
        }
        checkout.finish()
    }
}
