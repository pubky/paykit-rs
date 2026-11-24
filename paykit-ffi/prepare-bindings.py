"""Make the generated host callbacks answer every failure, and protect the default
descriptions of binding values that can contain wallet data."""
from pathlib import Path
import re
import sys

# The generated wrappers around a host callback: sync and async, with and without a typed error.
CALLBACK_WRAPPERS = (
    "uniffiTraitInterfaceCall",
    "uniffiTraitInterfaceCallWithError",
    "uniffiTraitInterfaceCallAsync",
    "uniffiTraitInterfaceCallAsyncWithError",
)
GENERATED_CATCH = "catch(e: kotlin.Exception)"
GENERATED_TEXT = "FfiConverterString.lower(e.toString())"
# Only this fixed text crosses to Rust for a failed callback, never the host's own.
FIXED_TEXT = 'FfiConverterString.lower("host callback failed")'
# What isn't an Exception, and a coroutine's cancellation, go on as before once Paykit has its
# answer: the platform still reports an Error, and cancellation still cancels.
RETHROW = ("if (e !is kotlin.Exception || "
           "e is kotlin.coroutines.cancellation.CancellationException) throw e")


def _patch_wrapper(body, name):
    """Patch one wrapper's text, checking each piece on its own."""
    if body.count(GENERATED_CATCH) != 1 or body.count(GENERATED_TEXT) != 1:
        raise ValueError(f"unexpected generated catch in {name}")
    body = body.replace(GENERATED_CATCH, "catch(e: kotlin.Throwable)")
    body = body.replace(GENERATED_TEXT, FIXED_TEXT)
    if "Async" in name:
        # The answer goes to Rust first; the async wrapper then leaves its coroutine.
        marker = "return@coroutineBlock"
        if body.count(marker) != 1:
            raise ValueError(f"unexpected generated return in {name}")
        indent = body[:body.index(marker)].rsplit("\n", 1)[1]
        return body.replace(marker, RETHROW + "\n" + indent + marker)
    # The sync wrapper writes the unexpected-failure status, then rethrows.
    status = "callStatus.error_buf = " + FIXED_TEXT
    if body.count(status) != 1:
        raise ValueError(f"unexpected generated status in {name}")
    at = body.index(status) + len(status)
    indent = body[:body.index(status)].rsplit("\n", 1)[1]
    return body[:at] + "\n" + indent + RETHROW + body[at:]


def catch_throwable(kotlin):
    """Make each generated callback wrapper answer every Throwable, not only Exception.

    A Kotlin Error (an AssertionError, an OutOfMemoryError) thrown by a host callback otherwise
    escapes the wrapper: a sync call then leaves its status at success with a zeroed answer,
    and an async call never answers. Caught, it answers as an unexpected failure, which the
    wallet reads as an unknown outcome, with a fixed text; then an Error, and a coroutine's
    cancellation, are thrown on, so the platform reports them as before. Each wrapper is
    checked on its own, and any change in the generated text fails here, so an upgrade of the
    generator can't drop this.
    """
    if kotlin.count(GENERATED_CATCH) != len(CALLBACK_WRAPPERS):
        raise ValueError("expected one generated catch in each callback wrapper")
    for name in CALLBACK_WRAPPERS:
        match = re.search(rf"^internal inline fun ?<[^>]*> {name}\(", kotlin, re.MULTILINE)
        if not match:
            raise ValueError(f"missing generated callback wrapper {name}")
        end = kotlin.find("\ninternal ", match.end())
        end = len(kotlin) if end < 0 else end
        kotlin = kotlin[:match.start()] + _patch_wrapper(kotlin[match.start():end], name) + kotlin[end:]
    return kotlin


def redact(kotlin_path, swift_path):
    kotlin = catch_throwable(kotlin_path.read_text())
    # UniFFI's Long buffer length already matches the JNA signature. Kotlin 2.3
    # diagnoses the redundant conversion when the consumer treats warnings as errors.
    length = "getByteBuffer(0, this.len.toLong())"
    if kotlin.count(length) != 1:
        raise ValueError("expected one generated JNA buffer method")
    kotlin = kotlin.replace(length, "getByteBuffer(0, this.len)")
    # Records and variants with fields can carry protected wallet or provider data.
    # EnrollmentInfo contains the public App ID/path; the two host errors expose
    # only failure classifications and byte counts. Fieldless enums have no payload.
    public_types = {"EnrollmentInfo", "HomeserverException", "StorageException"}
    types = re.findall(r"^(?:data|sealed) class (\w+)\b", kotlin, re.MULTILINE)
    converters = {
        target: wire for wire, target in re.findall(
            r"^public object FfiConverterType(\w+)\s*:\s*FfiConverterRustBuffer<(\w+)>",
            kotlin, re.MULTILINE,
        )
    }
    if not types or not public_types.issubset(types):
        raise ValueError("unexpected generated Kotlin value declarations")
    swift = []
    for kotlin_name in types:
        if kotlin_name in public_types:
            continue
        name = converters[kotlin_name]
        sealed = rf"^sealed class {kotlin_name}\b[^{{\n]*\{{\n"
        record = rf"^data class {kotlin_name} \(\n.*?^\)[^{{\n]*\{{\n"
        pattern = sealed if re.search(sealed, kotlin, re.MULTILINE) else record
        override = "final override" if pattern == sealed else "override"
        kotlin, count = re.subn(
            pattern,
            lambda m: m[0] + f'    {override} fun toString(): String = "{kotlin_name}(<redacted>)"\n',
            kotlin,
            flags=re.MULTILINE | re.DOTALL,
        )
        if count != 1:
            raise ValueError(f"expected one generated Kotlin declaration for {name}")
        if kotlin_name.endswith("Exception"):
            # UniFFI also interpolates variant fields into Throwable.message.
            start = kotlin.index(f"sealed class {kotlin_name}:")
            end = kotlin.index("\n}", start)
            body, count = re.subn(r'(override val message\n\s+get\(\) = )"[^\n]*"',
                                  r"\g<1>toString()", kotlin[start:end])
            if not count:
                raise ValueError(f"missing generated messages for {kotlin_name}")
            kotlin = kotlin[:start] + body + kotlin[end:]
        swift.append(f"""extension {name}: CustomStringConvertible, CustomDebugStringConvertible, CustomReflectable {{
    public var description: String {{ "{name}(<redacted>)" }}
    public var debugDescription: String {{ description }}
    public var customMirror: Mirror {{ Mirror(self, children: [:]) }}
}}
""")
    kotlin_path.write_text(kotlin)
    swift_path.write_text("\n".join(swift))


if __name__ == "__main__":
    redact(Path(sys.argv[1]), Path(sys.argv[2]))
