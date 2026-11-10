//! `messages.json`: every kind as v0.2 sends it (Switch wire 4.5, Paykit wire 4),
//! `label_rejections`, `multi_protocol_senders`, `cancellation_roles` and `conversion_terms`.

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
use pubky_switch_core::scopes::Permission;
use serde_json::{json, Value};

use crate::common::{
    arr, cert, generated, hex, message, message_json, paykit, pubky, quoted, reissued, signed,
    signed_json, st, Keys, Place, ALICE, ALLOWANCE_ID, BOB,
};

/// One message of the file, checked and read.
struct Read {
    message: Message,
    body: Body,
    hash: [u8; 32],
}

#[test]
fn test_vectors_messages() {
    let keys = Keys::load();
    generate(&keys);
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
            body: body.to_body(message.refs.len()).unwrap(),
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
    check_multi_protocol_senders(&keys, &file);
    check_cancellation_roles(&keys, &file);
    check_conversion_terms(&keys, &file);
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

/// The top-level description, which names every member.
const DESCRIPTION: &str = "Every message kind as sent in v0.2 (Switch wire 4.5, wire 4): the \
exact JSON bytes, their BLAKE3 hash (refs, request hash), the sender app's signature over \
lp(\"pubky-switch/v1/msg\", recipient identity, JSON), and the signed plaintext layout \
sender app ID (16) | signature (64) | u32_be(JSON length) | JSON, before zero padding. \
payment_request_labelled is sent by bob's hosted app under its label, and alice's acceptance \
of it copies the label; label_rejections holds requests a payer drops for their label; \
multi_protocol_senders holds messages from apps whose certificates hold another protocol's \
scopes; cancellation_roles holds a cancellation between certificates of each pair of roles; \
conversion_terms holds the shop's period details and requests with their conversion terms \
changed, which a payer takes or drops.";

const MULTI_PROTOCOL_DESCRIPTION: &str = "Wire 4.1 check 8, spec 3.2, 3.3, Switch wire 3.1 \
\"Several protocols\": messages.json's payment_request and resend_request_ids, as the shop \
signs them, each presented with a certificate of the shop's keys that holds another \
protocol's scope, chat (sender_cert, documents.json multi_protocol_certificates), at alice's \
wallet (recipient_app), whose certificate is the Switch set's. Another protocol's scopes, \
and the switch permissions they bring, authorize no Paykit message: each kind still needs \
its Paykit scope, and a resend_request needs a Paykit App, one holding at least one Paykit \
scope. accepted are messages the wallet takes at check 8, from the shop holding request \
beside chat; refused are those it drops there, from the shop holding chat alone, a \
certificate Paykit's verifiers take. reason is paykit-lib's wording, of the first check \
that refuses it.";

/// The certificates of `multi_protocol_senders`: request beside chat, and chat alone.
fn sender_cert(keys: &Keys, chat_only: bool) -> AppCert {
    if chat_only {
        reissued(keys, "shop", &["chat"], &[Permission::ListsRead])
    } else {
        reissued(
            keys,
            "shop",
            &["request", "chat"],
            &[Permission::ListsRead, Permission::ConversationAdd],
        )
    }
}

/// Check 8 at alice's wallet for a message the shop signed, presented with `sender`: the
/// recipient checks that need the certificate first (Switch wire 5.1 checks 4 to 6).
fn at_wallet(keys: &Keys, json: &[u8], sender: &AppCert) -> pubky_switch_core::Result<()> {
    let shop = keys.app("shop");
    let plaintext = MessagePlaintext::sign_json(&shop.keys, json.to_vec(), OPAQUE_MEMBERS).unwrap();
    let message = plaintext
        .check(
            &pubky(ALICE),
            &shop.identity,
            &VerifiedSender::from_cert(sender),
            OPAQUE_MEMBERS,
        )
        .unwrap();
    PaykitKindRules::for_certificate(&cert(keys.app("wallet"))).check_scopes(&message, sender)
}

/// The cases paykit-lib generates in `messages.json` (README "How the files were
/// generated"), compared with the file, and written into it with `PAYKIT_WRITE_VECTORS`.
fn generate(keys: &Keys) {
    let wallet = keys.app("wallet");
    let case = |name: &str, base: &str, chat_only: bool| -> Value {
        let m = message(base);
        assert_eq!(st(&m, "sender_app"), keys.app("shop").app_id, "{base}");
        let json = message_json(base);
        let sender = sender_cert(keys, chat_only);
        let plaintext =
            MessagePlaintext::sign_json(&keys.app("shop").keys, json.clone(), OPAQUE_MEMBERS)
                .unwrap();
        let mut case = json!({
            "json": String::from_utf8(json.clone()).unwrap(),
            "name": name,
            "recipient_app": wallet.app_id,
            "sender_app": st(&m, "sender_app"),
            "sender_cert": signed_json(sender.signed()),
            "signature": b64(&plaintext.signature()),
        });
        match at_wallet(keys, &json, &sender) {
            Ok(()) => assert!(!chat_only, "{name}: taken"),
            Err(error) => {
                assert!(chat_only, "{name}: {error}");
                case["reason"] = json!(quoted(&error.to_string()));
            }
        }
        case
    };
    let member = json!({
        "accepted": [
            case("a payment_request from an app holding request and chat", "payment_request", false),
            case("a resend_request from an app holding request and chat", "resend_request_ids", false),
        ],
        "description": MULTI_PROTOCOL_DESCRIPTION,
        "refused": [
            case("a payment_request from an app holding only chat", "payment_request", true),
            case("a resend_request from an app holding only chat", "resend_request_ids", true),
        ],
    });
    generated(
        "messages.json",
        &[
            (Place::Member("/description"), json!(DESCRIPTION)),
            (Place::Member("/multi_protocol_senders"), member),
            (
                Place::Member("/cancellation_roles"),
                cancellation_roles(keys),
            ),
            (Place::Member("/conversion_terms"), conversion_terms(keys)),
        ],
    );
}

const CANCELLATION_DESCRIPTION: &str = "Wire 4.1 check 8, 4.3, 4.8, spec 3.3: messages.json's \
payment_request_cancellation, as the shop signs it, without a label, presented with a \
certificate of the shop's keys (sender_cert) at a certificate of alice's wallet's keys \
(recipient_cert), each with its scopes changed, its permissions those Paykit's scopes need, \
and signed again. A cancellation's recipient holds the other role than its sender: the \
payer's goes from pay to request, the payee's from request to pay. accepted are the two \
pairings the recipient takes at check 8; refused are pay to pay and request to request, \
which it drops there. reason is paykit-lib's wording, of the first check that refuses it.";

/// The certificate of `app` with `scopes` and the permissions Paykit's scopes need.
fn with_scopes(keys: &Keys, app: &str, scopes: &[&str]) -> AppCert {
    let owned: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
    reissued(
        keys,
        app,
        scopes,
        &paykit_lib::scopes::permissions_for(&owned),
    )
}

/// Check 8 for the shop's cancellation, presented with `sender`, at `recipient`.
fn cancellation_at(
    keys: &Keys,
    json: &[u8],
    sender: &AppCert,
    recipient: &AppCert,
) -> pubky_switch_core::Result<()> {
    let shop = keys.app("shop");
    let plaintext = MessagePlaintext::sign_json(&shop.keys, json.to_vec(), OPAQUE_MEMBERS).unwrap();
    let message = plaintext
        .check(
            &pubky(ALICE),
            &shop.identity,
            &VerifiedSender::from_cert(sender),
            OPAQUE_MEMBERS,
        )
        .unwrap();
    PaykitKindRules::for_certificate(recipient).check_scopes(&message, sender)
}

/// `cancellation_roles`, as paykit-lib generates it.
fn cancellation_roles(keys: &Keys) -> Value {
    let base = "payment_request_cancellation";
    let m = message(base);
    assert_eq!(st(&m, "sender_app"), keys.app("shop").app_id, "{base}");
    let json = message_json(base);
    let case = |name: &str, from: &[&str], to: &[&str], taken: bool| -> Value {
        let sender = with_scopes(keys, "shop", from);
        let recipient = with_scopes(keys, "wallet", to);
        let plaintext =
            MessagePlaintext::sign_json(&keys.app("shop").keys, json.clone(), OPAQUE_MEMBERS)
                .unwrap();
        let mut case = json!({
            "json": String::from_utf8(json.clone()).unwrap(),
            "name": name,
            "recipient_cert": signed_json(recipient.signed()),
            "sender_app": st(&m, "sender_app"),
            "sender_cert": signed_json(sender.signed()),
            "signature": b64(&plaintext.signature()),
        });
        match cancellation_at(keys, &json, &sender, &recipient) {
            Ok(()) => assert!(taken, "{name}: taken"),
            Err(error) => {
                assert!(!taken, "{name}: {error}");
                case["reason"] = json!(quoted(&error.to_string()));
            }
        }
        case
    };
    json!({
        "accepted": [
            case("the payee's cancellation, from request to pay", &["request"], &["pay"], true),
            case("the payer's cancellation, from pay to request", &["pay"], &["request"], true),
        ],
        "description": CANCELLATION_DESCRIPTION,
        "refused": [
            case("a cancellation from pay to pay", &["pay"], &["pay"], false),
            case("a cancellation from request to request", &["request"], &["request"], false),
        ],
    })
}

/// `cancellation_roles` (Paykit wire 4.1 check 8, 4.8, spec 3.3): the shop's signature,
/// certificates Paykit's verifiers take, and check 8 taking each pairing of opposite roles
/// and dropping each pairing of one role.
fn check_cancellation_roles(keys: &Keys, file: &Value) {
    let m = &file["cancellation_roles"];
    let (shop, alice, bob) = (keys.app("shop"), pubky(ALICE), pubky(BOB));
    assert_eq!(arr(m, "accepted").len(), 2);
    assert_eq!(arr(m, "refused").len(), 2);
    for (list, accepted) in [("accepted", true), ("refused", false)] {
        for case in arr(m, list) {
            let name = st(case, "name");
            let json = st(case, "json").as_bytes().to_vec();
            assert_eq!(st(case, "sender_app"), shop.app_id, "{name}");
            let plaintext =
                MessagePlaintext::sign_json(&shop.keys, json.clone(), OPAQUE_MEMBERS).unwrap();
            assert_eq!(b64(&plaintext.signature()), st(case, "signature"), "{name}");
            let verify = |doc: &Value, identity: &[u8; 32]| {
                paykit_lib::scopes::verify_certificate(&signed(doc), identity)
                    .unwrap_or_else(|e| panic!("{name}: Paykit's verifiers take it: {e}"))
            };
            let sender = verify(&case["sender_cert"], &bob);
            let recipient = verify(&case["recipient_cert"], &alice);
            let verdict = cancellation_at(keys, &json, &sender, &recipient);
            assert_eq!(verdict.is_ok(), accepted, "{name}: {verdict:?}");
            if let Err(error) = verdict {
                assert_eq!(quoted(&error.to_string()), st(case, "reason"), "{name}");
            }
        }
    }
}

/// `multi_protocol_senders` (Paykit wire 4.1 check 8, spec 3.3): the shop's signatures,
/// certificates Paykit's verifiers take, and check 8 at alice's wallet taking the messages
/// from request beside chat and dropping those from chat alone.
fn check_multi_protocol_senders(keys: &Keys, file: &Value) {
    let m = &file["multi_protocol_senders"];
    let (shop, bob) = (keys.app("shop"), pubky(BOB));
    assert_eq!(arr(m, "accepted").len(), 2);
    assert_eq!(arr(m, "refused").len(), 2);
    for (list, accepted) in [("accepted", true), ("refused", false)] {
        for case in arr(m, list) {
            let name = st(case, "name");
            let json = st(case, "json").as_bytes().to_vec();
            assert_eq!(st(case, "sender_app"), shop.app_id, "{name}");
            assert_eq!(
                st(case, "recipient_app"),
                keys.app("wallet").app_id,
                "{name}"
            );
            let plaintext =
                MessagePlaintext::sign_json(&shop.keys, json.clone(), OPAQUE_MEMBERS).unwrap();
            assert_eq!(b64(&plaintext.signature()), st(case, "signature"), "{name}");
            let sender =
                paykit_lib::scopes::verify_certificate(&signed(&case["sender_cert"]), &bob)
                    .unwrap_or_else(|e| {
                        panic!("{name}: Paykit's verifiers take the certificate: {e}")
                    });
            let verdict = at_wallet(keys, &json, &sender);
            assert_eq!(verdict.is_ok(), accepted, "{name}: {verdict:?}");
            if let Err(error) = verdict {
                assert_eq!(quoted(&error.to_string()), st(case, "reason"), "{name}");
            }
        }
    }
}

const CONVERSION_DESCRIPTION: &str = "Wire 4.4, 4.5, 4.9, specs/payment-conversion.md: the \
shop's messages with their conversion terms changed, signed again by the shop, each from \
messages.json's message base with body replaced by the body given. Requests are \
payment_request_required_app, which accepts btc-lightning-bolt11 only, with fixed rates: with \
conversion terms the requested asset is lower-case letters and digits, each rate's asset is \
<asset> or <asset>-<rail> of such segments, and it matches the asset, and the rail when it \
names one, of an accepted endpoint. Period details name \
payment_request_recurring (request), a usd request with per_period conversion: their rates \
are cross-asset only, so a bare or rail-qualified rate for the requested asset (usd, \
usd-polygon) makes them invalid against the request. accepted are messages alice takes, \
at check 10 and, for period details, against their request; refused are those she drops. \
reason is paykit-lib's wording, of the first check that refuses it.";

/// One case of `conversion_terms`: `base` with its body replaced by `body`, as the shop
/// signs it.
struct ConversionCase {
    name: &'static str,
    base: &'static str,
    body: Value,
    request: Option<&'static str>,
}

/// The cases of `conversion_terms`, accepted then refused.
fn conversion_cases() -> (Vec<ConversionCase>, Vec<ConversionCase>) {
    let details = |name, rates: Value| ConversionCase {
        name,
        base: "payment_period_details",
        body: json!({
            "billing_period": { "ends_at": "2026-03-31T00:00:00.000Z", "starts_at": "2026-02-28T00:00:00.000Z" },
            "expires_at": "2026-03-01T00:00:00.000Z",
            "rates": rates,
        }),
        request: Some("payment_request_recurring"),
    };
    // payment_request_required_app, a usd request accepting btc-lightning-bolt11 only, with
    // fixed rates `rates` and its asset `asset`.
    let request = |name, asset: &str, rates: Value| {
        let base = Message::parse(
            &message_json("payment_request_required_app"),
            OPAQUE_MEMBERS,
        )
        .unwrap();
        let mut body = Value::Object(base.body);
        body["amount"]["asset"] = json!(asset);
        body["conversion"] = json!({ "rates": rates, "type": "fixed" });
        ConversionCase {
            name,
            base: "payment_request_required_app",
            body,
            request: None,
        }
    };
    let rate = |selector: &str| json!([{ "asset": selector, "value": "0.00001234" }]);
    let accepted = vec![
        details(
            "period details with a rail-qualified rate for another asset",
            json!([{ "asset": "btc-lightning", "value": "0.00001234" }]),
        ),
        request("a request with an asset-wide rate", "usd", rate("btc")),
        request(
            "a request with an asset-rail rate",
            "usd",
            rate("btc-lightning"),
        ),
    ];
    let refused = vec![
        request(
            "a request with conversion terms in an asset in upper case",
            "USD",
            rate("btc"),
        ),
        request("a rate selector in upper case", "usd", rate("BTC")),
        request(
            "a rate selector of three segments",
            "usd",
            rate("btc-lightning-bolt11"),
        ),
        request(
            "a rate selector whose asset matches no accepted endpoint",
            "usd",
            rate("usdt"),
        ),
        request(
            "a rate selector whose rail matches no accepted endpoint",
            "usd",
            rate("btc-bitcoin"),
        ),
        details(
            "period details with a bare rate for the requested asset",
            json!([{ "asset": "btc", "value": "0.00001234" }, { "asset": "usd", "value": "1" }]),
        ),
        details(
            "period details with a rail-qualified rate for the requested asset",
            json!([{ "asset": "btc", "value": "0.00001234" }, { "asset": "usd-polygon", "value": "1.008" }]),
        ),
    ];
    (accepted, refused)
}

/// The JSON of a case: its base's message with the case's body, written as messages are.
fn conversion_json(case: &ConversionCase) -> Vec<u8> {
    let mut m = Message::parse(&message_json(case.base), OPAQUE_MEMBERS).unwrap();
    let Value::Object(body) = case.body.clone() else {
        panic!("{}: a body", case.name)
    };
    m.body = body;
    m.to_json().unwrap()
}

/// Alice's checks of a case the shop signed: check 10, then, for period details, the checks
/// against their request (Paykit wire 4.9).
fn conversion_verdict(keys: &Keys, json: &[u8], request: Option<&str>) -> paykit_lib::Result<()> {
    let shop = keys.app("shop");
    let plaintext = MessagePlaintext::sign_json(&shop.keys, json.to_vec(), OPAQUE_MEMBERS).unwrap();
    let message = plaintext
        .check(
            &pubky(ALICE),
            &shop.identity,
            &VerifiedSender::from_cert(&cert(shop)),
            OPAQUE_MEMBERS,
        )
        .unwrap();
    let body = check_body(&message, json)?;
    if let (Some(request), Body::PaymentPeriodDetails(details)) = (request, &body) {
        let Body::PaymentRequest(request) =
            Body::parse(&Message::parse(&message_json(request), OPAQUE_MEMBERS).unwrap())?
        else {
            panic!("{request}: a request")
        };
        details.check_against(&request)?;
    }
    Ok(())
}

/// `conversion_terms`, as paykit-lib generates it.
fn conversion_terms(keys: &Keys) -> Value {
    let shop = keys.app("shop");
    let write = |case: &ConversionCase, taken: bool| -> Value {
        assert_eq!(
            st(&message(case.base), "sender_app"),
            shop.app_id,
            "{}",
            case.name
        );
        let json = conversion_json(case);
        let plaintext =
            MessagePlaintext::sign_json(&shop.keys, json.clone(), OPAQUE_MEMBERS).unwrap();
        let mut out = json!({
            "base": case.base,
            "json": String::from_utf8(json.clone()).unwrap(),
            "name": case.name,
            "sender_app": shop.app_id,
            "signature": b64(&plaintext.signature()),
        });
        if let Some(request) = case.request {
            out["request"] = json!(request);
        }
        match conversion_verdict(keys, &json, case.request) {
            Ok(()) => assert!(taken, "{}: taken", case.name),
            Err(error) => {
                assert!(!taken, "{}: {error}", case.name);
                out["reason"] = json!(quoted(&error.to_string()));
            }
        }
        out
    };
    let (accepted, refused) = conversion_cases();
    json!({
        "accepted": accepted.iter().map(|c| write(c, true)).collect::<Vec<_>>(),
        "description": CONVERSION_DESCRIPTION,
        "refused": refused.iter().map(|c| write(c, false)).collect::<Vec<_>>(),
    })
}

/// `conversion_terms` (Paykit wire 4.4, 4.5, 4.9): the shop's signatures, each message its
/// base with the body given, and alice taking or dropping each as its list says.
fn check_conversion_terms(keys: &Keys, file: &Value) {
    let m = &file["conversion_terms"];
    let shop = keys.app("shop");
    let (accepted, refused) = conversion_cases();
    assert_eq!(arr(m, "accepted").len(), accepted.len());
    assert_eq!(arr(m, "refused").len(), refused.len());
    for (list, taken) in [("accepted", true), ("refused", false)] {
        for case in arr(m, list) {
            let name = st(case, "name");
            let json = st(case, "json").as_bytes().to_vec();
            assert_eq!(st(case, "sender_app"), shop.app_id, "{name}");
            let plaintext =
                MessagePlaintext::sign_json(&shop.keys, json.clone(), OPAQUE_MEMBERS).unwrap();
            assert_eq!(b64(&plaintext.signature()), st(case, "signature"), "{name}");
            // The message is its base with only the body changed.
            let mut base = Message::parse(&message_json(st(case, "base")), OPAQUE_MEMBERS).unwrap();
            let read = Message::parse(&json, OPAQUE_MEMBERS).unwrap();
            base.body = read.body.clone();
            assert_eq!(base.to_json().unwrap(), json, "{name}: its base");
            let verdict = conversion_verdict(keys, &json, case["request"].as_str());
            assert_eq!(verdict.is_ok(), taken, "{name}: {verdict:?}");
            if let Err(error) = verdict {
                assert_eq!(quoted(&error.to_string()), st(case, "reason"), "{name}");
            }
        }
    }
}
