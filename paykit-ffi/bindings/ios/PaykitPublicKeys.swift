import Foundation

public enum PaykitPublicKeyFormatError: Error, Equatable {
    case invalidPubkyPublicKey
}

public enum PaykitPublicKeys {
    private static let prefix = "pubky"
    private static let rawLength = 52
    private static let allowedCharacters = Set("ybndrfg8ejkmcpqxot1uwisza345h769")

    public static func normalize(_ value: String) throws -> String {
        "\(prefix)\(try raw(value))"
    }

    public static func raw(_ value: String) throws -> String {
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        let rawValue: String
        if trimmed.hasPrefix(prefix), trimmed.count == prefix.count + rawLength {
            rawValue = String(trimmed.dropFirst(prefix.count))
        } else {
            rawValue = trimmed
        }
        guard rawValue.count == rawLength,
              rawValue.allSatisfy({ allowedCharacters.contains($0) }) else {
            throw PaykitPublicKeyFormatError.invalidPubkyPublicKey
        }
        return rawValue
    }

    public static func redacted(_ value: String) throws -> String {
        let normalized = try normalize(value)
        return "\(normalized.prefix(prefix.count + 6))...\(normalized.suffix(6))"
    }
}
