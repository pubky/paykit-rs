//! Opaque JSON a host gives the bindings: metadata, extension bodies and payment proofs
//! (Paykit wire 1.1, 4.3).
//!
//! The SDK holds such JSON decoded and writes it back when it signs. A number the decoder
//! can't hold exactly, such as an integer above 2^64 or one with an exponent, would be written
//! back with other digits, so the signed message would not say what the host wrote. In what
//! the host gives before anything is sent, every number is checked against its original text,
//! and one that would change is refused. A payment proof comes after the money moved, and is
//! only decoded.

use paykit_lib::kinds::OPAQUE_MEMBERS;
use pubky_dispatch_core::json::{check_json_as, Doc};
use serde_json::{Map, Value};

/// Where a host's opaque JSON goes in a message, which decides how the wire checks it.
#[derive(Clone, Copy)]
pub(crate) enum Place<'a> {
    /// A body's `metadata`, of any kind (Paykit wire 1.1).
    Metadata,
    /// The whole body of an extension message of `kind` (Paykit wire 4.3).
    ExtensionBody {
        /// The extension kind, already validated.
        kind: &'a str,
    },
    /// The `proof` of a `paykit.payment_proof` (Paykit wire 4.10), as the payment method
    /// reports it with the payment made.
    Proof,
}

/// Read `bytes` as the JSON object a message carries at `place`. The original text is first
/// checked in that message's context, as the wire checks it, before decoding can drop a
/// repeated member; then every number must be written back exactly as given.
///
/// A proof is only decoded. The method reports it once the money has moved, so refusing it
/// would leave a paid payment open with no proof for good: a later lookup would give the
/// same proof and meet the same refusal (Paykit spec 7.2). A method that needs a number
/// carried exactly writes it as a string.
pub(crate) fn object(bytes: &[u8], place: Place<'_>) -> Option<Map<String, Value>> {
    let message = match place {
        Place::Proof => return serde_json::from_slice(bytes).ok(),
        Place::Metadata => [br#"{"body":{"metadata":"#.as_slice(), bytes, b"}}"].concat(),
        Place::ExtensionBody { kind } => [
            br#"{"kind":""#.as_slice(),
            kind.as_bytes(),
            br#"","body":"#,
            bytes,
            b"}",
        ]
        .concat(),
    };
    check_json_as(&message, Doc::Message(OPAQUE_MEMBERS)).ok()?;
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
