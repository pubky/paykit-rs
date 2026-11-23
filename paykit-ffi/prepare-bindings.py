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
