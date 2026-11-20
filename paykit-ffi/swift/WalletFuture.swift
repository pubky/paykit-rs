import Foundation

// UniFFI's native cancel wakes a pending poll. Keep cancel and free ordered even when
// task cancellation arrives concurrently with completion on a different executor.
final class WalletFuture: @unchecked Sendable {
    private let lock = NSLock()
    private var handle: UInt64?
    private let cancelNative: (UInt64) -> Void

    init(_ handle: UInt64, cancel: @escaping (UInt64) -> Void) {
        self.handle = handle
        self.cancelNative = cancel
    }

    func cancel() {
        lock.lock()
        defer { lock.unlock() }
        if let handle { cancelNative(handle) }
    }

    func finish(_ free: (UInt64) -> Void) {
        lock.lock()
        let handle = self.handle
        self.handle = nil
        lock.unlock()
        if let handle { free(handle) }
    }
}

// A Swift cooperative executor can have less stack than a native wallet read needs.
// Poll on the wallet worker, then let UniFFI wake the original Swift continuation.
// A suspended network or host callback returns its worker to the next ready poll.
final class WalletPoller: @unchecked Sendable {
    static let shared = WalletPoller()

    private let ready = NSCondition()
    private var pending: [() -> Void] = []

    private init() {
        let worker = Thread { [self] in
            while true {
                ready.lock()
                while pending.isEmpty { ready.wait() }
                let batch = pending
                pending.removeAll(keepingCapacity: false)
                ready.unlock()
                for poll in batch { autoreleasepool(invoking: poll) }
            }
        }
        worker.name = "Paykit native wallet"
        worker.stackSize = 1_835_008
        worker.start()
    }

    func schedule(_ poll: @escaping () -> Void) {
        ready.lock()
        pending.append(poll)
        ready.signal()
        ready.unlock()
    }
}
