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
