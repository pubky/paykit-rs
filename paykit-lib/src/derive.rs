//! Paykit's derivations (Paykit spec 7.1, 10; Paykit wire 1.5): message, request and terms
//! hashes, obligation IDs, natural keys, settlement IDs and the tags of settlement marks.
//!
//! They use the Switch's `hash()`, `derive()`, `lp()` and blinding (Switch wire 1.4, 1.5).
//! Paykit's own derivations, the obligation ID and the settlement and occurrence tags, use the
//! labels `paykit/v1/obligation` and `paykit/v1/settlement`, never a `pubky-switch/v1/` one
//! (Switch spec 1).
//!
//! Security: obligation IDs, record keys and settlement and occurrence tags are blinded with
//! the naming key of the identity whose switch holds them, so the service can't tell which
//! counterparty, request or settlement they name, and no payer can aim a settlement at a
//! bucket or a tag. The obligation of a payment without a request blinds its attempt ID the
//! same way: one who holds only the attempt ID, such as the payee from the proof, can't
//! compute its claim key. It names no payee.

use pubky_switch_core::enc::{derive, hash, lp, u64be};
use pubky_switch_core::ident::NamingKey;
use serde_json::{Map, Value};

use crate::identifiers::{AllowanceId, AttemptId, PaymentEndpointIdentifier};

/// The label of Paykit's obligation IDs (Paykit wire 1.5).
pub const OBLIGATION_LABEL: &str = "paykit/v1/obligation";

/// The label of Paykit's settlement and occurrence tags (Paykit wire 1.5).
pub const SETTLEMENT_LABEL: &str = "paykit/v1/settlement";

/// How many buckets of settlement marks there are (Paykit wire 1.5, 5.7): a bucket is a number
/// from 0 to 1023.
pub const SETTLEMENT_BUCKETS: u16 = 1024;

/// How many bytes of a settlement's tag a mark stores: bytes 2 to 13 (Paykit wire 1.5, 5.7).
pub const SETTLEMENT_TAG_LEN: usize = 12;

/// How many bytes an occurrence's tag has: the first 8 of its derivation (Paykit wire 1.5).
pub const OCCURRENCE_TAG_LEN: usize = 8;

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

/// The key material of a payment without a request's obligation: `lp("payment",
/// blind(attempt ID))`, blinded with the payer's naming key (Paykit wire 1.5).
pub fn payment_obligation_material(naming_key: &NamingKey, attempt_id: &AttemptId) -> Vec<u8> {
    lp(&[b"payment", &naming_key.blind(&attempt_id.to_bytes())])
}

/// The obligation ID of a payment without a request (Paykit wire 1.5):
/// `derive("paykit/v1/obligation", "payment", blind(attempt ID))` under the payer's naming
/// key. It names no payee, so a holder of the naming key can't test guesses of the payee
/// against it.
pub fn payment_obligation(naming_key: &NamingKey, attempt_id: &AttemptId) -> [u8; 32] {
    payment_obligation_blinded(&naming_key.blind(&attempt_id.to_bytes()))
}

/// [`payment_obligation`] from the attempt ID already blinded with the payer's naming key.
pub fn payment_obligation_blinded(blinded_attempt_id: &[u8; 32]) -> [u8; 32] {
    derive(OBLIGATION_LABEL, &[b"payment", blinded_attempt_id])
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

    /// A `private_list_publications` record: `lp(publishing app ID (16 bytes), recipient
    /// identity, u64(generation))`, the counter of the lists that app sends that recipient
    /// in one generation of its identity (Paykit wire 5.8).
    pub fn list_publication(
        publisher: &[u8; 16],
        recipient: &[u8; 32],
        generation: u64,
    ) -> Vec<u8> {
        lp(&[publisher, recipient, &u64be(generation)])
    }

    /// The `block_floor` record: `lp(own identity)`, the one record of the collection.
    pub fn block_floor(own: &[u8; 32]) -> Vec<u8> {
        lp(&[own])
    }

    /// An `allowances` or `allowance_ledgers` record: `lp(counterparty identity, allowance_id
    /// text)`.
    pub fn allowance(counterparty: &[u8; 32], allowance_id: &AllowanceId) -> Vec<u8> {
        lp(&[counterparty, allowance_id.as_str().as_bytes()])
    }

    /// A `settlements` bucket record: `lp(u64(bucket))`, the bucket a number from 0 to 1023
    /// ([`super::settlement_bucket`]).
    pub fn settlement_bucket(bucket: u16) -> Vec<u8> {
        lp(&[&u64be(u64::from(bucket))])
    }
}

/// The input blinded into a settlement's tag: `lp(settlement ID)` (Paykit wire 1.5).
pub fn settlement_tag_material(settlement_id: &[u8]) -> Vec<u8> {
    lp(&[settlement_id])
}

/// A settlement's tag (Paykit wire 1.5): `derive("paykit/v1/settlement", "tag",
/// blind(lp(settlement ID)))`, blinded with the naming key of the payee, whose switch holds
/// the marks. It derives from the settlement ID alone, whatever the endpoint, the counterparty
/// or the occurrence, so one settlement has one tag and one mark (Paykit spec 8.1 step 4).
pub fn settlement_tag(naming_key: &NamingKey, settlement_id: &[u8]) -> [u8; 32] {
    settlement_tag_blinded(&naming_key.blind(&settlement_tag_material(settlement_id)))
}

/// [`settlement_tag`] from its input already blinded with the payee's naming key,
/// `blind(lp(settlement ID))`. For an app that blinds through its Switch client and never
/// holds the naming key itself.
pub fn settlement_tag_blinded(blinded: &[u8; 32]) -> [u8; 32] {
    derive(SETTLEMENT_LABEL, &[b"tag", blinded])
}

/// A settlement's bucket (Paykit wire 1.5): `u16_be(tag[0..2]) >> 6`, a number from 0 to
/// 1023. Bits 10 to 15 of the tag are unused.
pub fn settlement_bucket(tag: &[u8; 32]) -> u16 {
    u16::from_be_bytes([tag[0], tag[1]]) >> 6
}

/// The part of a settlement's tag its mark stores: bytes 2 to 13 (Paykit wire 1.5, 5.7).
pub fn mark_settlement_tag(tag: &[u8; 32]) -> [u8; SETTLEMENT_TAG_LEN] {
    let mut out = [0; SETTLEMENT_TAG_LEN];
    out.copy_from_slice(&tag[2..2 + SETTLEMENT_TAG_LEN]);
    out
}

/// An occurrence's tag (Paykit wire 1.5): the first 8 bytes of `derive("paykit/v1/settlement",
/// "occurrence", blind(natural key))`, `natural key` being that of the occurrence's
/// `payee_activity` record ([`natural_key`]), which is the same under every label's policy.
pub fn occurrence_tag(naming_key: &NamingKey, natural_key: &[u8]) -> [u8; OCCURRENCE_TAG_LEN] {
    occurrence_tag_blinded(&naming_key.blind(natural_key))
}

/// [`occurrence_tag`] from the natural key already blinded with the payee's naming key.
pub fn occurrence_tag_blinded(blinded: &[u8; 32]) -> [u8; OCCURRENCE_TAG_LEN] {
    let tag = derive(SETTLEMENT_LABEL, &[b"occurrence", blinded]);
    let mut out = [0; OCCURRENCE_TAG_LEN];
    out.copy_from_slice(&tag[..OCCURRENCE_TAG_LEN]);
    out
}

/// A reader of the settlement ID of a payment proof (Paykit wire 1.5), from the Payment
/// Endpoint Identifier paid and the proof's `proof` object: the payment method's own reader,
/// from the rail the host supplies. Paykit defines no settlement ID and reads no proof
/// itself; the bytes are the method's, and `None` is a proof that names none, which isn't
/// credited as a payment without a request.
pub type SettlementIdReader<'a> =
    dyn Fn(&PaymentEndpointIdentifier, &Map<String, Value>) -> Option<Vec<u8>> + 'a;

#[cfg(test)]
mod tests {
    use super::*;

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
            payment_obligation(&nk, &attempt),
            blake3_derive(&payment_obligation_material(&nk, &attempt))
        );
        // Another payer's key or another attempt names another claim, and the attempt ID
        // alone names none of them.
        let other = NamingKey::from_bytes([8; 32]);
        assert_ne!(
            payment_obligation(&nk, &attempt),
            payment_obligation(&other, &attempt)
        );
        assert_ne!(
            payment_obligation(&nk, &attempt),
            payment_obligation(&nk, &AttemptId::from_bytes([4; 16]))
        );
        assert_ne!(
            payment_obligation(&nk, &attempt),
            derive(OBLIGATION_LABEL, &[b"payment", &attempt.to_bytes()])
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

    /// A settlement ID as some payment method names it: Paykit reads only its bytes.
    const SETTLEMENT: &[u8] = b"an example settlement";

    #[test]
    fn test_settlement_tag_derives_from_the_settlement_id_alone() {
        let nk = NamingKey::from_bytes([7; 32]);
        let tag = settlement_tag(&nk, SETTLEMENT);
        assert_eq!(
            tag,
            derive(SETTLEMENT_LABEL, &[b"tag", &nk.blind(&lp(&[SETTLEMENT]))])
        );
        assert_eq!(
            settlement_tag_blinded(&nk.blind(&settlement_tag_material(SETTLEMENT))),
            tag
        );
        // Blinded under another identity's naming key, or unblinded, it is another tag.
        assert_ne!(
            settlement_tag(&NamingKey::from_bytes([8; 32]), SETTLEMENT),
            tag
        );
        assert_ne!(settlement_tag_blinded(&hash(SETTLEMENT)), tag);
        assert_ne!(settlement_tag(&nk, b"another settlement"), tag);
    }

    #[test]
    fn test_settlement_tag_is_one_across_endpoints() {
        // One settlement paid to two endpoints names two payee records but one settlement,
        // so its mark is one (Paykit wire 1.5).
        let id = SETTLEMENT.to_vec();
        let (a, b) = (
            PaymentEndpointIdentifier::parse("btc-bitcoin-p2wpkh").unwrap(),
            PaymentEndpointIdentifier::parse("btc-bitcoin-p2tr").unwrap(),
        );
        assert_ne!(
            natural_key::payee_payment_record(&a, &id),
            natural_key::payee_payment_record(&b, &id)
        );
    }

    #[test]
    fn test_settlement_bucket_is_the_first_ten_bits() {
        let mut tag = [0u8; 32];
        assert_eq!(settlement_bucket(&tag), 0);
        tag[0] = 0xff;
        tag[1] = 0xc0;
        assert_eq!(settlement_bucket(&tag), SETTLEMENT_BUCKETS - 1);
        // Bits 10 to 15 are unused.
        tag[1] = 0xff;
        assert_eq!(settlement_bucket(&tag), SETTLEMENT_BUCKETS - 1);
        tag[0] = 0x00;
        tag[1] = 0x40;
        assert_eq!(settlement_bucket(&tag), 1);
        tag[1] = 0x3f;
        assert_eq!(settlement_bucket(&tag), 0);
        let tag: [u8; 32] = std::array::from_fn(|i| i as u8);
        assert_eq!(
            mark_settlement_tag(&tag),
            [2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]
        );
    }

    #[test]
    fn test_settlement_bucket_record_natural_key() {
        assert_eq!(
            natural_key::settlement_bucket(1023),
            lp(&[&[0, 0, 0, 0, 0, 0, 3, 0xff]])
        );
        assert_ne!(
            natural_key::settlement_bucket(0),
            natural_key::settlement_bucket(1)
        );
    }

    #[test]
    fn test_occurrence_tag_is_the_first_eight_bytes() {
        let nk = NamingKey::from_bytes([7; 32]);
        let natural = natural_key::request_record(&[1; 32], &[2; 32]);
        let full = derive(SETTLEMENT_LABEL, &[b"occurrence", &nk.blind(&natural)]);
        let tag = occurrence_tag(&nk, &natural);
        assert_eq!(tag[..], full[..8]);
        assert_eq!(occurrence_tag_blinded(&nk.blind(&natural)), tag);
        assert_ne!(
            occurrence_tag(&nk, &natural_key::period_record(&[1; 32], &[2; 32], 0)),
            tag
        );
        // The settlement's tag of the same bytes is another derivation.
        assert_ne!(settlement_tag(&nk, &natural)[..8], tag[..]);
    }

    fn blake3_derive(material: &[u8]) -> [u8; 32] {
        let parts = pubky_switch_core::enc::split_lp(material).unwrap();
        let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
        derive(OBLIGATION_LABEL, &refs)
    }
}
