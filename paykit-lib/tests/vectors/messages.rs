//! `messages.json`: every kind as v0.2 sends it (Switch wire 4.5, Paykit wire 4), and
//! `label_rejections`.

use std::collections::HashMap;

use paykit_lib::bodies::{check_body, Body, ResendRequest};
use paykit_lib::collections::Collection;
use paykit_lib::derive::{natural_key, request_obligation};
use paykit_lib::identifiers::AllowanceId;
use paykit_lib::kinds::{check_kind_scopes, Kind, PaykitKindRules, OPAQUE_MEMBERS};
use pubky_switch_core::enc::b64;
use pubky_switch_core::message::{Message, MessagePlaintext, VerifiedSender};
use pubky_switch_core::receive::KindRules;
use pubky_switch_core::records::AppCert;

use crate::common::{arr, cert, hex, paykit, pubky, signed, st, Keys, ALICE, ALLOWANCE_ID, BOB};

/// One message of the file, checked and read.
struct Read {
    message: Message,
    body: Body,
    hash: [u8; 32],
}

#[test]
fn test_vectors_messages() {
    let keys = Keys::load();
    let file = paykit("messages.json");
    let mut read: HashMap<String, Read> = HashMap::new();
    let mut names_by_hash: HashMap<[u8; 32], String> = HashMap::new();
    for m in arr(&file, "messages") {
        let name = st(m, "name").to_owned();
        let json = st(m, "json").as_bytes().to_vec();
        let sender = keys.app_by_id(st(m, "sender_app"));
        assert_eq!(sender.identity, pubky(st(m, "sender_identity")), "{name}");
        let recipient = pubky(st(m, "recipient_identity"));
        // Signed by the sender app, deterministically, over the recipient and the JSON.
        let plaintext =
            MessagePlaintext::sign_json(&sender.keys, json.clone(), OPAQUE_MEMBERS).unwrap();
        assert_eq!(
            b64(&plaintext.signature()),
            st(m, "signature"),
            "{name}: signature"
        );
        assert_eq!(
            hex(&plaintext.encode()),
            st(m, "plaintext_hex"),
            "{name}: plaintext"
        );
        assert_eq!(b64(&plaintext.hash()), st(m, "hash"), "{name}: hash");
        // The recipient checks of Switch wire 5.1 that need the certificate (4 to 6).
        let sender_cert = cert(sender);
        let message = plaintext
            .check(
                &recipient,
                &sender.identity,
                &VerifiedSender::from_cert(&sender_cert),
                OPAQUE_MEMBERS,
            )
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(message.kind, st(m, "kind"));
        // Check 6's own-app rule, check 8 at some app of the recipient, and check 10.
        let rules_pass = keys
            .apps
            .iter()
            .filter(|a| a.identity == recipient)
            .any(|a| {
                PaykitKindRules::for_certificate(&cert(a))
                    .check_scopes(&message, &sender_cert)
                    .is_ok()
            });
        assert!(
            rules_pass,
            "{name}: no app of the recipient takes it at check 8"
        );
        assert_eq!(
            message.is_from_own_app(),
            PaykitKindRules::new(Vec::new()).sent_between_own_apps(&message.kind),
            "{name}: check 6"
        );
        let body = check_body(&message, &json).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(body.kind().as_str(), message.kind);
        // The body writes back to the same JSON, byte for byte.
        let rebuilt = Message {
            body: body.to_body(),
            ..message.clone()
        };
        assert_eq!(rebuilt.to_json().unwrap(), json, "{name}: rebuilt");
        // To another identity, the signature doesn't verify.
        let other = if recipient == pubky(ALICE) {
            pubky(BOB)
        } else {
            pubky(ALICE)
        };
        assert!(plaintext
            .check(
                &other,
                &sender.identity,
                &VerifiedSender::from_cert(&sender_cert),
                OPAQUE_MEMBERS
            )
            .is_err());
        names_by_hash.insert(plaintext.hash(), name.clone());
        read.insert(
            name,
            Read {
                message,
                body,
                hash: plaintext.hash(),
            },
        );
    }
    assert_eq!(read.len(), 23, "every kind as v0.2 sends it");
    check_refs(&read, &names_by_hash);
    check_contents(&keys, &read);
    check_resend(&keys, &read);
    check_label_rejections(&keys, &file);
}

/// `refs` name messages by hash, in each kind's order (Paykit wire 4.3).
fn check_refs(read: &HashMap<String, Read>, names: &HashMap<[u8; 32], String>) {
    let refs = |n: &str| -> Vec<String> {
        read[n]
            .message
            .refs
            .iter()
            .map(|h| {
                names
                    .get(h)
                    .cloned()
                    .unwrap_or_else(|| format!("? {}", b64(h)))
            })
            .collect()
    };
    assert_eq!(refs("payment_request_acceptance"), ["payment_request"]);
    assert_eq!(
        refs("payment_request_rejection"),
        ["payment_request_required_app"]
    );
    assert_eq!(
        refs("payment_request_cancellation"),
        ["payment_request_recurring"]
    );
    assert_eq!(
        refs("payment_period_details"),
        ["payment_request_recurring"]
    );
    assert_eq!(refs("payment_proof"), ["payment_request"]);
    assert_eq!(
        refs("payment_proof_period"),
        ["payment_request_recurring", "payment_period_details"]
    );
    assert_eq!(
        refs("payment_receipt"),
        ["payment_proof", "payment_request"]
    );
    assert_eq!(refs("allowance_acceptance"), ["allowance_proposal"]);
    assert_eq!(
        refs("allowance_end"),
        ["allowance_proposal", "allowance_acceptance"]
    );
    assert_eq!(
        refs("payment_request_acceptance_labelled"),
        ["payment_request_labelled"]
    );
    assert_eq!(read["allowance_rejection"].message.refs.len(), 1);
    assert!(read["payment_proof_unrequested"].message.refs.is_empty());
}

/// What the bodies say, against the other files.
fn check_contents(keys: &Keys, read: &HashMap<String, Read>) {
    let (alice, bob) = (pubky(ALICE), pubky(BOB));
    let alice_nk = keys.identity(&alice).naming_key();
    let Body::PaymentRequest(recurring) = &read["payment_request_recurring"].body else {
        panic!("a request")
    };
    // The periods named are periods of the request, and the February period's obligation is
    // identifiers.json's.
    let want = arr(&paykit("identifiers.json"), "obligations")
        .iter()
        .find(|o| st(o, "name") == "recurring request, February period")
        .map(|o| st(o, "obligation").to_owned())
        .unwrap();
    for n in ["payment_period_details", "payment_proof_period"] {
        let named = read[n].body.billing_period().unwrap();
        let period = recurring.named_period(named).unwrap();
        let ob = request_obligation(
            &alice_nk,
            &bob,
            &read["payment_request_recurring"].hash,
            period.start_secs(),
        );
        assert_eq!(b64(&ob), want, "{n}");
    }
    let Body::PaymentPeriodDetails(details) = &read["payment_period_details"].body else {
        panic!("period details")
    };
    let period = details.check_against(recurring).unwrap();
    // Each proof names what its request needs: the February period, or none.
    let (Body::PaymentProof(period_proof), Body::PaymentProof(one_time_proof)) = (
        &read["payment_proof_period"].body,
        &read["payment_proof"].body,
    ) else {
        panic!("proofs")
    };
    assert_eq!(period_proof.check_against(recurring).unwrap(), Some(period));
    // The February period's deadline: its start plus a day, plus 1 ms.
    assert_eq!(
        recurring.attempt_deadline(Some(&period)).unwrap(),
        Some(period.starts_at + 86_400_000 + 1)
    );
    // The one-time request's attempt deadline: its proposal expiry, earlier than its payment
    // deadline, plus 1 ms.
    let Body::PaymentRequest(one_time) = &read["payment_request"].body else {
        panic!("a request")
    };
    assert_eq!(one_time_proof.check_against(one_time).unwrap(), None);
    assert!(period_proof.check_against(one_time).is_err());
    assert_eq!(
        one_time.attempt_deadline(None).unwrap(),
        Some(crate::common::time("2026-01-02T00:00:00.001Z"))
    );
    // The executor notice names the Allowance record's key, with alice's naming key.
    let Body::ExecutorNotice(notice) = &read["executor_notice"].body else {
        panic!("a notice")
    };
    let key = Collection::Allowances
        .record_key(
            &alice_nk,
            &natural_key::allowance(&bob, &AllowanceId::parse(ALLOWANCE_ID).unwrap()),
            None,
        )
        .unwrap();
    assert_eq!(notice.allowance_record, key);
    // The list's version is its publishing time, and it expires 30 days later.
    let Body::PrivatePaymentList(list) = &read["private_payment_list"].body else {
        panic!("a list")
    };
    assert_eq!(
        list.version,
        read["private_payment_list"].message.created_at_ms
    );
    assert_eq!(list.expires_at, list.version + 30 * 86_400_000);
    // The proof of a payment without a request names its settlement.
    let Body::PaymentProof(unrequested) = &read["payment_proof_unrequested"].body else {
        panic!("a proof")
    };
    assert_eq!(
        unrequested.settlement_id().unwrap(),
        b"4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b".to_vec()
    );
    // The receipt confirms the proof's attempt.
    let (Body::PaymentReceipt(receipt), Body::PaymentProof(proof)) =
        (&read["payment_receipt"].body, &read["payment_proof"].body)
    else {
        panic!("receipt and proof")
    };
    assert_eq!(receipt.attempt_id, proof.attempt_id);
    // The labelled acceptance copies the request's label.
    assert_eq!(
        read["payment_request_acceptance_labelled"].body.label(),
        read["payment_request_labelled"].body.label()
    );
}

/// `resend_request` selectors (Paykit wire 4.12), over the messages bob sent alice.
fn check_resend(keys: &Keys, read: &HashMap<String, Read>) {
    let file = paykit("messages.json");
    let bob_sent: Vec<&Read> = arr(&file, "messages")
        .iter()
        .filter(|m| st(m, "sender_identity") == BOB && st(m, "recipient_identity") == ALICE)
        .map(|m| &read[st(m, "name")])
        .collect();
    let name_of = |r: &Read| {
        read.iter()
            .find(|(_, x)| x.message.msg_id == r.message.msg_id)
            .map(|(n, _)| n.clone())
            .unwrap()
    };
    let select = |req: &ResendRequest| -> Vec<String> {
        bob_sent
            .iter()
            .filter(|r| {
                req.selects(
                    &r.message,
                    &r.hash,
                    &[r.message.created_at_ms],
                    r.body.billing_period(),
                )
            })
            .map(|r| name_of(r))
            .collect()
    };
    let Body::ResendRequest(by_period) = &read["resend_request_period"].body else {
        panic!("resend")
    };
    assert_eq!(
        select(by_period),
        ["payment_period_details"],
        "the period's messages, not the request"
    );
    let whole = ResendRequest {
        billing_period: None,
        ..by_period.clone()
    };
    let mut names = select(&whole);
    names.sort();
    assert_eq!(
        names,
        [
            "payment_period_details",
            "payment_request_cancellation",
            "payment_request_recurring"
        ]
    );
    let Body::ResendRequest(by_time) = &read["resend_request"].body else {
        panic!("resend")
    };
    assert_eq!(by_time.accepted_from, by_time.accepted_until);
    let Body::ResendRequest(by_ids) = &read["resend_request_ids"].body else {
        panic!("resend")
    };
    assert_eq!(
        by_ids.msg_ids,
        [read["payment_request_acceptance"].message.msg_id.clone()]
    );
    // A wallet with `pay` alone doesn't receive Allowance kinds: a resend answer leaves them
    // out (Paykit wire 4.12).
    let wallet = &keys.app("wallet").scopes;
    let proposal = &read["allowance_proposal"].message;
    let rules = PaykitKindRules::new(wallet.clone());
    let sender = cert(keys.app_by_id(&proposal.from.app_id_text()));
    assert!(rules.check_scopes(proposal, &sender).is_err());
}

/// `label_rejections` (Paykit wire 4.1, 4.5, 4.6): requests a payer drops for their label,
/// an acceptance whose label isn't valid, and acceptances a payee app drops at check 8.
fn check_label_rejections(keys: &Keys, file: &serde_json::Value) {
    let cases = arr(&file["label_rejections"], "cases");
    assert_eq!(cases.len(), 6);
    let wallet = keys.app("wallet");
    for case in cases {
        let name = st(case, "name");
        let json = st(case, "json").as_bytes().to_vec();
        let sender = keys.app_by_id(st(case, "sender_app"));
        let plaintext =
            MessagePlaintext::sign_json(&sender.keys, json.clone(), OPAQUE_MEMBERS).unwrap();
        assert_eq!(b64(&plaintext.signature()), st(case, "signature"), "{name}");
        let sender_cert = cert(sender);
        let recipient = if sender.identity == pubky(BOB) {
            pubky(ALICE)
        } else {
            pubky(BOB)
        };
        let message = plaintext
            .check(
                &recipient,
                &sender.identity,
                &VerifiedSender::from_cert(&sender_cert),
                OPAQUE_MEMBERS,
            )
            .unwrap();
        let own: AppCert = match case.get("recipient_cert") {
            Some(doc) => AppCert::verify(&signed(doc), &recipient).unwrap(),
            None if recipient == pubky(ALICE) => cert(wallet),
            None => cert(keys.app("shop")),
        };
        // Check 8, then check 10: the message is refused by one of them.
        let at_8 = PaykitKindRules::for_certificate(&own).check_scopes(&message, &sender_cert);
        let at_10 = check_body(&message, &json);
        assert!(at_8.is_err() || at_10.is_err(), "{name}: accepted");
        let error = at_8
            .err()
            .map(|e| e.to_string())
            .or_else(|| at_10.err().map(|e| e.to_string()))
            .unwrap();
        assert!(error.contains("label"), "{name}: {error}");
        if message.kind == Kind::PaymentRequestAcceptance.as_str()
            && case.get("recipient_cert").is_some()
        {
            assert!(check_kind_scopes(&message, sender_cert.scopes(), own.scopes()).is_err());
        }
    }
}
