import Foundation
import Paykit

/// A wallet with no recurring request can list its subscriptions (Paykit spec 8.2).
func checkPeriods(fixture: CheckoutFixture, wallet: NativeWallet) async throws {
    // Payer reads wait for the wallet's first full read (Paykit spec 6).
    try await wallet.finishReceive()
    let requests = try await wallet.recurringRequests()
    precondition(requests.isEmpty)
    let key = Data(repeating: 0, count: 32)
    let missing = RequestId(payee: fixture.payee(), requestHash: key)
    let period = BillingPeriod(startsAt: 0, endsAt: fixture.nowMs())
    let details = PeriodDetails(
        billingPeriod: period,
        paymentEndpoints: [ListEndpoint(identifier: "btc-lightning-bolt11", payload: "lnbc1period")],
        rates: nil, expiresAtMs: period.endsAt
    )
    func missingRecord(_ call: () async throws -> SentMessage) async throws {
        do {
            _ = try await call()
            preconditionFailure("sent a message for a missing record")
        } catch WalletError.Permanent(let failure) {
            precondition(failure.code() == "not_found")
        }
    }
    try await missingRecord { try await wallet.sendPeriodDetails(record: key, details: details) }
    try await missingRecord { try await wallet.acceptSubscription(id: missing, options: nil) }
    try await missingRecord {
        try await wallet.acceptSubscription(
            id: missing, options: AcceptOptions(possiblyCancelledConfirmed: nil, blockedConfirmed: nil)
        )
    }
    try await missingRecord { try await wallet.acceptSubscriptionUnderAllowance(id: missing, allowance: key) }

    func refused(_ result: PaymentResult) {
        guard case let .error(.Permanent(failure)) = result.decision else {
            preconditionFailure("expected a missing request")
        }
        precondition(failure.code() == "not_found" && result.publicFailures.isEmpty)
    }
    let options = PaymentOptions(
        endpoint: nil, listApp: nil, publicEndpointsConfirmed: false,
        unconfirmedIdentityConfirmed: false, confirmedRefusals: []
    )
    refused(try await wallet.payPeriod(id: missing, period: period, options: options))
    refused(try await wallet.payPeriodAutomatically(id: missing, period: period, price: .requestedAmount))
    refused(try await wallet.payPeriodUnderAllowance(id: missing, period: period))

    // Read a restored copy before its first listing (Switch spec 12.4).
    let host = fixture.hostForStore(store: EnrollmentStore())
    let restored = try await host.restore(appId: wallet.appId(), bytes: wallet.backup(), sourceDevice: .inPossession)
    do {
        _ = try await restored.recurringRequests()
        preconditionFailure("listed recurring requests before the restored records were read")
    } catch WalletError.NeedsUser(let failure) {
        precondition(failure.code() == "not_now")
    }
    try await restored.shutdown()
    let remaining = try await wallet.recurringRequests()
    precondition(remaining.isEmpty)

    // The shop's recurring checkout is listed, accepted once and cancelled again (Paykit spec 8.2).
    let id = try await fixture.sendRecurringCheckout()
    try await wallet.finishReceive()
    func listed() async throws -> RecurringRequest {
        guard let found = try await wallet.recurringRequests().first(where: { $0.id.requestHash == id.requestHash }) else {
            preconditionFailure("missing recurring request")
        }
        return found
    }
    let open = try await listed()
    precondition(open.status == .open && open.amount == Amount(asset: "btc", value: "7"))
    precondition(open.recurrence.every == 1 && open.recurrence.unit == .day && open.periods.isEmpty)
    precondition(open.paymentReference == "checkout-7" && open.record.count == 32 && !open.manualOnly)
    let acceptance = try await wallet.acceptSubscription(id: id, options: nil)
    let recorded = try await wallet.messageDelivery(msgId: acceptance.msgId)
    guard case .accepted = acceptance.delivery, UUID(uuidString: acceptance.msgId) != nil,
          recorded == acceptance.delivery else { preconditionFailure("acceptance was not delivered") }
    let subscribed = try await listed()
    precondition(subscribed.status == .subscribed)
    do {
        _ = try await wallet.acceptSubscription(id: id, options: nil)
        preconditionFailure("accepted a subscription twice")
    } catch WalletError.Permanent(let failure) {
        precondition(failure.code() == "subscribed")
    }
    _ = try await wallet.cancelRequest(id: id, reason: "test subscription")
    let cancelled = try await listed()
    precondition(cancelled.status == .cancelled)
}
