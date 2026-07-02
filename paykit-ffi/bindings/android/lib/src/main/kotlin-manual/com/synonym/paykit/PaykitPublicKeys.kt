package com.synonym.paykit

public object PaykitPublicKeys {
    private const val PREFIX: String = "pubky"
    private const val RAW_LENGTH: Int = 52
    private val allowedCharacters: Set<Char> =
        "ybndrfg8ejkmcpqxot1uwisza345h769".toSet()

    @JvmStatic
    public fun normalize(value: String): String = "$PREFIX${raw(value)}"

    @JvmStatic
    public fun raw(value: String): String {
        val trimmed = value.trim()
        val rawValue = if (
            trimmed.startsWith(PREFIX) &&
            trimmed.length == PREFIX.length + RAW_LENGTH
        ) {
            trimmed.substring(PREFIX.length)
        } else {
            trimmed
        }
        require(rawValue.length == RAW_LENGTH && rawValue.all { it in allowedCharacters }) {
            "invalid Pubky public key"
        }
        return rawValue
    }

    @JvmStatic
    public fun redacted(value: String): String {
        val normalized = normalize(value)
        return "${normalized.take(PREFIX.length + 6)}...${normalized.takeLast(6)}"
    }
}
