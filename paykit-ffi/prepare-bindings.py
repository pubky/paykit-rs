"""Protect the default descriptions of binding values that can contain wallet data."""
from pathlib import Path
import re
import sys


def redact(kotlin_path, swift_path):
    kotlin = kotlin_path.read_text()
    # UniFFI's Long buffer length already matches the JNA signature. Kotlin 2.3
    # diagnoses the redundant conversion when the consumer treats warnings as errors.
    length = "getByteBuffer(0, this.len.toLong())"
    if kotlin.count(length) != 1:
        raise ValueError("expected one generated JNA buffer method")
    kotlin = kotlin.replace(length, "getByteBuffer(0, this.len)")
    types = ("StoreEntry", "StoreWrite", "EnrollmentRequest", "Receipt", "MethodProof", "MethodPayment", "MethodOutcome", "EndpointCandidate")
    swift = []
    for name in types:
        sealed = rf"^sealed class {name}\b[^{{\n]*\{{\n"
        record = rf"^data class {name} \(\n.*?^\)[^{{\n]*\{{\n"
        pattern = sealed if re.search(sealed, kotlin, re.MULTILINE) else record
        override = "final override" if pattern == sealed else "override"
        kotlin, count = re.subn(
            pattern,
            lambda m: m[0] + f'    {override} fun toString(): String = "{name}(<redacted>)"\n',
            kotlin,
            flags=re.MULTILINE | re.DOTALL,
        )
        if count != 1:
            raise ValueError(f"expected one generated Kotlin declaration for {name}")
        swift.append(f"""extension {name}: CustomStringConvertible, CustomDebugStringConvertible, CustomReflectable {{
    public var description: String {{ "{name}(<redacted>)" }}
    public var debugDescription: String {{ description }}
    public var customMirror: Mirror {{ Mirror(self, children: [:]) }}
}}
""")
    kotlin_path.write_text(kotlin)
    swift_path.write_text("\n".join(swift))


def swift_cancellation(source):
    # Throwing calls can report native cancellation. Nonthrowing callbacks keep
    # their generated completion semantics.
    swift = source.read_text()

    def wire(match):
        call = match[0]
        poll = re.search(r"pollFunc: (ffi_\w+_rust_future_poll_\w+),", call)
        if not poll:
            raise ValueError("missing native Swift poll")
        cancel = poll[1].replace("_poll_", "_cancel_")
        return call.replace(poll[0], poll[0] + "\n            cancelFunc: " + cancel + ",")

    swift, count = re.subn(r"try\s+await uniffiRustCallAsync\(.*?\n        \)", wire, swift, flags=re.DOTALL)
    if not count:
        raise ValueError("missing throwing Swift functions")

    def replace_once(original, replacement):
        nonlocal swift
        if swift.count(original) != 1:
            raise ValueError("unexpected UniFFI Swift template: " + original)
        swift = swift.replace(original, replacement)

    replace_once("    completeFunc: (UInt64,", "    cancelFunc: ((UInt64) -> Void)? = nil,\n    completeFunc: (UInt64,")
    replace_once("    let rustFuture = rustFutureFunc()", """    if cancelFunc != nil { try Task.checkCancellation() }
    let rustFuture = rustFutureFunc()
    let lifetime = cancelFunc.map { WalletFuture(rustFuture, cancel: $0) }""")
    replace_once("        freeFunc(rustFuture)", """        if let lifetime { lifetime.finish(freeFunc) } else { freeFunc(rustFuture) }""")
    start = swift.index("    var pollResult: Int8;")
    end = swift.index("\n}", start)
    body = "\n".join("    " + line if line else "" for line in swift[start:end].splitlines())
    swift = swift[:start] + "    func result() async throws -> T {\n" + body + """
    }
    guard let lifetime else { return try await result() }
    return try await withTaskCancellationHandler {
        try await result()
    } onCancel: {
        lifetime.cancel()
    }""" + swift[end:]
    replace_once('fatalError("Cancellation not supported yet")', 'throw CancellationError()')
    source.write_text(swift)


if __name__ == "__main__":
    redact(Path(sys.argv[1]), Path(sys.argv[2]))
    swift_cancellation(Path(sys.argv[3]))
