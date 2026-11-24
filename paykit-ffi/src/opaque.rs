//! Opaque JSON a host gives the bindings: metadata, extension bodies and payment proofs
//! (Paykit wire 1.1, 4.3).
//!
//! The SDK holds such JSON decoded and writes it back when it signs. A number the decoder
//! can't hold exactly, such as an integer above 2^64 or one with an exponent, would be written
//! back with other digits, so the signed message would not say what the host wrote. Every
//! number is checked against its original text, and one that would change is refused.

use serde_json::{Map, Value};

/// Decode `bytes` as a JSON object whose every number is written back exactly as given.
/// The caller has already checked the original text as the wire checks it.
pub(crate) fn object(bytes: &[u8]) -> Option<Map<String, Value>> {
    let value = serde_json::from_slice(bytes).ok()?;
    exact_numbers(bytes).then_some(value)
}

/// Whether every number in the JSON text `bytes` re-encodes to its own text once decoded.
/// `bytes` is valid JSON: the scan only needs to step over strings.
fn exact_numbers(bytes: &[u8]) -> bool {
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        match byte {
            b'"' => at = after_string(bytes, at + 1),
            b'-' | b'0'..=b'9' => {
                let start = at;
                while bytes
                    .get(at)
                    .is_some_and(|b| matches!(b, b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E'))
                {
                    at += 1;
                }
                let Some(token) = bytes.get(start..at) else {
                    return false;
                };
                if !written_back_as_given(token) {
                    return false;
                }
            }
            _ => at += 1,
        }
    }
    true
}

/// The index just past the string whose content starts at `at`.
fn after_string(bytes: &[u8], mut at: usize) -> usize {
    while let Some(&byte) = bytes.get(at) {
        match byte {
            b'\\' => at += 2,
            b'"' => return at + 1,
            _ => at += 1,
        }
    }
    at
}

fn written_back_as_given(token: &[u8]) -> bool {
    serde_json::from_slice::<Value>(token)
        .ok()
        .and_then(|value| serde_json::to_vec(&value).ok())
        .is_some_and(|written| written == token)
}
