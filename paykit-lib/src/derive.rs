//! Paykit's derivations (Paykit spec 7.1, 10; Paykit wire 1.5): message, request and terms
//! hashes, obligation IDs, natural keys and settlement IDs.
//!
//! They use the Switch's `hash()`, `derive()`, `lp()` and blinding (Switch wire 1.4, 1.5).
//! Paykit's own derivation, the obligation ID, uses the label `paykit/v1/obligation`, never a
//! `pubky-switch/v1/` one (Switch spec 1).
//!
//! Security: obligation IDs and record keys are blinded with the naming key of the identity
//! whose switch holds them, so the service can't tell which counterparty or request they
//! name. The obligation of a payment without a request is the one exception: it derives from
//! the attempt ID alone and names no payee.

use pubky_switch_core::enc::{derive, hash, lp, u64be};
use pubky_switch_core::ident::NamingKey;
use serde_json::{Map, Value};

use crate::identifiers::{AllowanceId, AttemptId, PaymentEndpointIdentifier};

/// The label of Paykit's obligation IDs (Paykit wire 1.5).
pub const OBLIGATION_LABEL: &str = "paykit/v1/obligation";

/// A message hash, as `refs` and request hashes name messages: `hash(message JSON bytes)`
/// (Paykit wire 1.5, Switch wire 5.1).
pub fn message_hash(json: &[u8]) -> [u8; 32] {
    pubky_switch_core::message::message_hash(json)
}

/// The request hash: the hash of the request message's exact JSON bytes (Paykit wire 1.5).
/// It identifies the request everywhere: obligations, activity records and `refs`.
pub fn request_hash(request_json: &[u8]) -> [u8; 32] {
    message_hash(request_json)
}

/// The key material of a terms hash: `lp(proposal message JSON bytes, acceptance message JSON
/// bytes)` (Paykit wire 1.5).
pub fn terms_hash_material(proposal_json: &[u8], acceptance_json: &[u8]) -> Vec<u8> {
    lp(&[proposal_json, acceptance_json])
}

/// The terms hash of an Allowance: `hash(lp(proposal JSON bytes, acceptance JSON bytes))`
/// (Paykit wire 1.5, 4.16).
pub fn terms_hash(proposal_json: &[u8], acceptance_json: &[u8]) -> [u8; 32] {
    hash(&terms_hash_material(proposal_json, acceptance_json))
}

/// The key material of a request's obligation: `lp("request", blind(payee identity),
/// blind(request hash), u64(period start in Unix seconds, 0 for a one-time request))`
/// (Paykit wire 1.5).
pub fn request_obligation_material(
    naming_key: &NamingKey,
    payee: &[u8; 32],
    request_hash: &[u8; 32],
    period_start_secs: u64,
) -> Vec<u8> {
    lp(&[
        b"request",
        &naming_key.blind(payee),
        &naming_key.blind(request_hash),
        &u64be(period_start_secs),
    ])
}

/// The obligation ID of a request, or of one Billing Period of a recurring request, used as
/// the claim key (Paykit spec 7.1, Paykit wire 1.5): `derive("paykit/v1/obligation",
/// "request", blind(payee identity), blind(request hash), u64(period start))`, blinded with
/// the payer's naming key, whose switch holds the claim.
pub fn request_obligation(
    naming_key: &NamingKey,
    payee: &[u8; 32],
    request_hash: &[u8; 32],
    period_start_secs: u64,
) -> [u8; 32] {
    request_obligation_blinded(
        &naming_key.blind(payee),
        &naming_key.blind(request_hash),
        period_start_secs,
    )
}

/// [`request_obligation`] from its inputs already blinded with the payer's naming key:
/// `blind(payee identity)` and `blind(request hash)` (Paykit wire 1.5). For an app that
/// blinds through its Switch client and never holds the naming key itself.
pub fn request_obligation_blinded(
    blinded_payee: &[u8; 32],
    blinded_request_hash: &[u8; 32],
    period_start_secs: u64,
) -> [u8; 32] {
    derive(
        OBLIGATION_LABEL,
        &[
            b"request",
            blinded_payee,
            blinded_request_hash,
            &u64be(period_start_secs),
        ],
    )
}

/// The key material of a payment without a request's obligation: `lp("payment", attempt
/// ID)` (Paykit wire 1.5).
pub fn payment_obligation_material(attempt_id: &AttemptId) -> Vec<u8> {
    lp(&[b"payment", &attempt_id.to_bytes()])
}

/// The obligation ID of a payment without a request (Paykit wire 1.5):
/// `derive("paykit/v1/obligation", "payment", attempt ID)`. It names no payee and isn't
/// blinded, so a former holder of the naming key can't test guesses of the payee against it.
pub fn payment_obligation(attempt_id: &AttemptId) -> [u8; 32] {
    derive(OBLIGATION_LABEL, &[b"payment", &attempt_id.to_bytes()])
}

/// Paykit's natural keys (Paykit wire 1.5, Switch spec 13.3): the inputs, blinded with the
/// naming key, that name what a record is about.
pub mod natural_key {
    use super::*;

    /// A request record in `payer_activity` or `payee_activity`: `lp(payee identity, request
    /// hash)`.
    pub fn request_record(payee: &[u8; 32], request_hash: &[u8; 32]) -> Vec<u8> {
        lp(&[payee, request_hash])
    }

    /// A Billing Period record: `lp(payee identity, request hash, u64(period start in Unix
    /// seconds))`.
    pub fn period_record(payee: &[u8; 32], request_hash: &[u8; 32], start_secs: u64) -> Vec<u8> {
        lp(&[payee, request_hash, &u64be(start_secs)])
    }

    /// The payer's record of a payment without a request: `lp(own identity, attempt ID)`.
    pub fn payer_payment_record(own: &[u8; 32], attempt_id: &AttemptId) -> Vec<u8> {
        lp(&[own, &attempt_id.to_bytes()])
    }

    /// The payee's record of a payment without a request: `lp(Payment Endpoint Identifier
    /// text, settlement ID)`, so every proof of one settlement names one record.
    pub fn payee_payment_record(
        endpoint: &PaymentEndpointIdentifier,
        settlement_id: &[u8],
    ) -> Vec<u8> {
        lp(&[endpoint.as_str().as_bytes(), settlement_id])
    }

    /// A `payment_lists` record: `lp(payee identity, publishing app ID (16 bytes))`.
    pub fn payment_list(payee: &[u8; 32], app_id: &[u8; 16]) -> Vec<u8> {
        lp(&[payee, app_id])
    }

    /// An `allowances` or `allowance_ledgers` record: `lp(counterparty identity, allowance_id
    /// text)`.
    pub fn allowance(counterparty: &[u8; 32], allowance_id: &AllowanceId) -> Vec<u8> {
        lp(&[counterparty, allowance_id.as_str().as_bytes()])
    }
}

/// A reader of a payment proof's `proof` object that gives its settlement ID (Paykit wire
/// 1.5), as [`settlement_id`] does for the methods this crate knows: a payment method's
/// own reader names the settlements of its own proofs.
pub type SettlementIdReader<'a> = dyn Fn(&Map<String, Value>) -> Option<Vec<u8>> + 'a;

/// The settlement ID of a payment proof's `proof` object (Paykit wire 1.5): the bytes its
/// payment method defines to name one settlement, in that method's one spelling.
///
/// The methods the wire format fixes are `bitcoin-txid`, whose settlement ID is its `txid`,
/// 64 lower-case hex characters, as its UTF-8 text; and the prototype's fake rail,
/// `{ "rail": "fake", "tx": ... }` without a `type`, whose settlement ID is its `tx` text,
/// not empty. `None` for a proof in another spelling, or of a method this crate doesn't
/// know: such a proof isn't credited as a payment without a request.
pub fn settlement_id(proof: &Map<String, Value>) -> Option<Vec<u8>> {
    // A `null` type reads as a missing one (Paykit wire, Conventions).
    let typed = proof.get("type").filter(|t| !t.is_null());
    if typed.is_none() && proof.get("rail").and_then(Value::as_str) == Some("fake") {
        let tx = proof.get("tx")?.as_str()?;
        return (!tx.is_empty()).then(|| tx.as_bytes().to_vec());
    }
    match typed?.as_str()? {
        "bitcoin-txid" => {
            let txid = proof.get("txid")?.as_str()?;
            let spelled = txid.len() == 64
                && txid
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            spelled.then(|| txid.as_bytes().to_vec())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn proof(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn test_settlement_id_of_a_bitcoin_txid() {
        let txid = "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";
        let id = settlement_id(&proof(
            json!({ "type": "bitcoin-txid", "txid": txid, "vout": 1 }),
        ));
        assert_eq!(id, Some(txid.as_bytes().to_vec()));
    }

    #[test]
    fn test_settlement_id_of_the_fake_rail() {
        let id = settlement_id(&proof(json!({ "rail": "fake", "tx": "t1" })));
        assert_eq!(id, Some(b"t1".to_vec()));
        // A null type reads as none.
        let id = settlement_id(&proof(json!({ "type": null, "rail": "fake", "tx": "t1" })));
        assert_eq!(id, Some(b"t1".to_vec()));
        for bad in [
            json!({ "rail": "fake" }),
            json!({ "rail": "fake", "tx": "" }),
            json!({ "rail": "fake", "tx": 1 }),
            json!({ "rail": "fake", "tx": null }),
            json!({ "rail": "other", "tx": "t1" }),
        ] {
            assert_eq!(settlement_id(&proof(bad.clone())), None, "{bad}");
        }
    }

    #[test]
    fn test_settlement_id_refuses_other_spellings_and_methods() {
        let upper = "4A5E1E4BAAB89F3A32518A88C31BC87F618F76673E2CC77AB2127B7AFDEDA33B";
        for bad in [
            json!({ "type": "bitcoin-txid", "txid": upper }),
            json!({ "type": "bitcoin-txid", "txid": "4a5e" }),
            json!({ "type": "bitcoin-txid" }),
            json!({ "type": "bitcoin-bolt11-preimage", "data": "00" }),
            json!({ "rail": "fake", "tx": "t1", "type": "bitcoin-txid" }),
        ] {
            assert_eq!(settlement_id(&proof(bad.clone())), None, "{bad}");
        }
    }

    #[test]
    fn test_obligations_differ_by_period_and_by_kind() {
        let nk = NamingKey::from_bytes([7; 32]);
        let one = request_obligation(&nk, &[1; 32], &[2; 32], 0);
        assert_ne!(one, request_obligation(&nk, &[1; 32], &[2; 32], 1));
        assert_ne!(
            one,
            request_obligation(&NamingKey::from_bytes([8; 32]), &[1; 32], &[2; 32], 0)
        );
        let attempt = AttemptId::from_bytes([3; 16]);
        assert_eq!(
            payment_obligation(&attempt),
            blake3_derive(&payment_obligation_material(&attempt))
        );
    }

    #[test]
    fn test_obligation_from_blinded_inputs_is_the_same() {
        let nk = NamingKey::from_bytes([7; 32]);
        let blinded = request_obligation_blinded(&nk.blind(&[1; 32]), &nk.blind(&[2; 32]), 5);
        assert_eq!(blinded, request_obligation(&nk, &[1; 32], &[2; 32], 5));
        // Unblinded inputs give another obligation.
        assert_ne!(request_obligation_blinded(&[1; 32], &[2; 32], 5), blinded);
    }

    fn blake3_derive(material: &[u8]) -> [u8; 32] {
        let parts = pubky_switch_core::enc::split_lp(material).unwrap();
        let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
        derive(OBLIGATION_LABEL, &refs)
    }
}
