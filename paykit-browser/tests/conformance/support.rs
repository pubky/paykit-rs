//! Reading Paykit's vector files, and the encodings their cases use.

use pubky_switch_core::enc::{hash, pubky_key, unb64_array};
use serde_json::Value;

/// A file of Paykit's set, `specs/v0.2/vectors/`, built into the test binary: a browser has no
/// file system to read it from.
pub fn vector_file(name: &str) -> Value {
    let text = match name {
        "documents.json" => include_str!("../../../specs/v0.2/vectors/documents.json"),
        "encoding.json" => include_str!("../../../specs/v0.2/vectors/encoding.json"),
        "identifiers.json" => include_str!("../../../specs/v0.2/vectors/identifiers.json"),
        "keys.json" => include_str!("../../../specs/v0.2/vectors/keys.json"),
        "messages.json" => include_str!("../../../specs/v0.2/vectors/messages.json"),
        "signatures.json" => include_str!("../../../specs/v0.2/vectors/signatures.json"),
        other => panic!("{other} is not built into these tests"),
    };
    serde_json::from_str(text).unwrap()
}

/// A string member.
pub fn st<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} isn't a string"))
}

/// An array member.
pub fn arr<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v[key]
        .as_array()
        .unwrap_or_else(|| panic!("{key} isn't an array"))
}

/// An unsigned integer member.
pub fn num(v: &Value, key: &str) -> u64 {
    v[key]
        .as_u64()
        .unwrap_or_else(|| panic!("{key} isn't an unsigned integer"))
}

/// Lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Bytes from lower-case hex.
pub fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

/// Base64url of `N` bytes.
pub fn b64n<const N: usize>(text: &str) -> [u8; N] {
    unb64_array::<N>(text).unwrap()
}

/// A Pubky ID's key.
pub fn pubky(id: &str) -> [u8; 32] {
    pubky_key(id).unwrap()
}

/// The 32 secret bytes of a seed label of the set: `BLAKE3("paykit/v1/test-vectors/" +
/// label)` (`keys.json`).
pub fn seed(label: &str) -> [u8; 32] {
    hash(format!("paykit/v1/test-vectors/{label}").as_bytes())
}

/// The exact JSON bytes of the message `name` of `messages.json`.
pub fn message_json(messages: &Value, name: &str) -> Vec<u8> {
    let message = arr(messages, "messages")
        .iter()
        .find(|m| st(m, "name") == name)
        .unwrap_or_else(|| panic!("no message {name}"));
    st(message, "json").as_bytes().to_vec()
}
