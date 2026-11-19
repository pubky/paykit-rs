//! `messages.json`: every kind as v0.2 sends it (Switch wire 4.5, Paykit wire 4),
//! `label_rejections`, `multi_protocol_senders`, `cancellation_roles`, `conversion_terms`,
//! `conversion_amounts`, `proof_members`, `receipt_members` and `list_versions`.

use std::collections::HashMap;

use paykit_lib::amount::{Asset, Decimal, PaymentAmount, Rate};
use paykit_lib::bodies::{check_body, Body, PaymentRequest, ResendRequest};
use paykit_lib::collections::Collection;
use paykit_lib::derive::{natural_key, request_obligation};
use paykit_lib::identifiers::{AllowanceId, PaymentEndpointIdentifier};
use paykit_lib::kinds::{check_kind_scopes, Kind, PaykitKindRules, OPAQUE_MEMBERS};
use paykit_lib::{amount_for_endpoint, EndpointAmount};
use pubky_switch_core::enc::b64;
use pubky_switch_core::message::{Message, MessagePlaintext, VerifiedSender};
use pubky_switch_core::receive::KindRules;
use pubky_switch_core::records::AppCert;
use serde_json::{json, Value};

use crate::common::{
    arr, beyond_paykit, cert, generated, hex, message, message_json, paykit, pubky, quoted,
    reissued, signed, signed_json, st, writing, Keys, Place, ALICE, ALLOWANCE_ID, BOB,
};
use crate::documents::MULTI_PROTOCOL_ACCEPTED;

/// One message of the file, checked and read.
struct Read {
    message: Message,
    body: Body,
    hash: [u8; 32],
}

#[test]
fn test_vectors_messages() {
    if !writing() {
        check();
    }
}

/// `messages.json` against paykit-lib: its generated cases, then every check of its cases. With
/// [`WRITE_VECTORS`](crate::common::WRITE_VECTORS) set, the regeneration driver runs it once it
/// has written the set.
pub(crate) fn check() {
    let keys = Keys::from_seeds();
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
    check_conversion_amounts(&file);
    check_proof_members(&keys, &file);
    check_receipt_members(&keys, &file);
    check_list_versions(&keys, &file);
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
    // The list's version is a pair, not a time; the list expires 30 days after publishing.
    let Body::PrivatePaymentList(list) = &read["private_payment_list"].body else {
        panic!("a list")
    };
    assert_eq!(list.version, paykit_lib::ListVersion::new(1, 12).unwrap());
    assert_eq!(
        list.expires_at,
        read["private_payment_list"].message.created_at_ms + 30 * 86_400_000
    );
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
    // A wallet with `paykit_pay` alone doesn't receive Allowance kinds: a resend answer leaves them
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
changed, which a payer takes or drops; conversion_amounts holds the amount a payer pays to \
an endpoint under a request's or period details' rates; proof_members holds alice's proofs \
with an extra or missing member, which a payee drops; receipt_members holds bob's receipts \
with the payee's metadata and reference, which a payer takes or drops; list_versions holds \
the shop's Private Payment Lists with a malformed version, which a payer drops.";

const MULTI_PROTOCOL_DESCRIPTION: &str = "Wire 4.1 check 8, spec 3.2, 3.3, Switch wire 3.1 \
\"Several protocols\": messages.json's payment_request and resend_request_ids, as the shop \
signs them, each presented with a certificate of the shop's keys that holds another \
protocol's scope, chat (sender_cert, documents.json multi_protocol_certificates), at alice's \
wallet (recipient_app), whose certificate is its certificate of documents.json \
certificates. Another protocol's scopes, \
and the switch permissions they bring, authorize no Paykit message: each kind still needs \
its Paykit scope, and a resend_request needs a Paykit App, one holding at least one Paykit \
scope. accepted are messages the wallet takes at check 8, from the shop holding paykit_request, \
with the paykit_settlements and paykit_contacts it requires, beside chat; refused are those it drops there, from the shop holding chat alone, a \
certificate Paykit's verifiers take. reason is paykit-lib's wording, of the first check \
that refuses it.";

/// The certificates of `multi_protocol_senders`: those `multi_protocol_certificates`
/// accepts (`documents.json`), request beside chat, and chat alone.
fn sender_cert(keys: &Keys, chat_only: bool) -> AppCert {
    let (_, app, scopes, permissions) = MULTI_PROTOCOL_ACCEPTED[usize::from(chat_only)];
    reissued(keys, app, scopes, permissions)
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
/// generated"), compared with the file, and written into the set by the
/// regeneration driver (`regenerate.rs`).
pub(crate) fn generate(keys: &Keys) {
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
            case("a payment_request from an app holding paykit_request and chat", "payment_request", false),
            case("a resend_request from an app holding paykit_request and chat", "resend_request_ids", false),
        ],
        "description": MULTI_PROTOCOL_DESCRIPTION,
        "refused": [
            case("a payment_request from an app holding only chat", "payment_request", true),
            case("a resend_request from an app holding only chat", "resend_request_ids", true),
        ],
    });
    let mut cases = vec![
        (Place::Member("/description"), json!(DESCRIPTION)),
        (Place::Member("/multi_protocol_senders"), member),
        (
            Place::Member("/cancellation_roles"),
            cancellation_roles(keys),
        ),
        (Place::Member("/conversion_terms"), conversion_terms(keys)),
        (Place::Member("/conversion_amounts"), conversion_amounts()),
        (Place::Member("/proof_members"), proof_members(keys)),
        (Place::Member("/receipt_members"), receipt_members(keys)),
        (Place::Member("/list_versions"), list_versions(keys)),
    ];
    cases.extend(signed_messages(keys));
    generated("messages.json", &cases);
}

/// The executor notice's JSON with the record key of the Allowance record it names, under
/// the `allowances` policy (Paykit wire 4.17, 5.1).
fn executor_notice_json(keys: &Keys, json: &str) -> String {
    let message = Message::parse(json.as_bytes(), OPAQUE_MEMBERS).unwrap();
    let Body::ExecutorNotice(notice) = check_body(&message, json.as_bytes()).unwrap() else {
        panic!("a notice")
    };
    let key = Collection::Allowances
        .record_key(
            &keys.identity(&pubky(ALICE)).naming_key(),
            &natural_key::allowance(&pubky(BOB), &AllowanceId::parse(ALLOWANCE_ID).unwrap()),
            None,
        )
        .unwrap();
    let held = format!("\"allowance_record\":\"{}\"", b64(&notice.allowance_record));
    assert!(json.contains(&held));
    json.replace(&held, &format!("\"allowance_record\":\"{}\"", b64(&key)))
}

/// What the sender app signs of each message of `messages` and `label_rejections`, from its
/// authored JSON: the signature, and for `messages` the plaintext and its hash; the executor
/// notice's record key; and the certificates `label_rejections` presents its acceptances at.
fn signed_messages(keys: &Keys) -> Vec<(Place<'static>, Value)> {
    let file = paykit("messages.json");
    let mut out = Vec::new();
    let mut hashes = HashMap::new();
    for (i, m) in arr(&file, "messages").iter().enumerate() {
        let mut json = st(m, "json").to_owned();
        let message = Message::parse(json.as_bytes(), OPAQUE_MEMBERS).unwrap();
        let refs: Vec<String> = message.refs.iter().map(|r| b64(r)).collect();
        let updated: Vec<String> = refs
            .iter()
            .map(|r| hashes.get(r).cloned().unwrap_or_else(|| r.clone()))
            .collect();
        if refs != updated {
            let from = format!("\"refs\":{}", serde_json::to_string(&refs).unwrap());
            let to = format!("\"refs\":{}", serde_json::to_string(&updated).unwrap());
            assert!(json.contains(&from));
            json = json.replacen(&from, &to, 1);
        }
        if st(m, "name") == "executor_notice" {
            json = executor_notice_json(keys, &json);
        }
        if json != st(m, "json") {
            out.push((Place::At(format!("/messages/{i}/json")), json!(json)));
        }
        let sender = keys.app_by_id(st(m, "sender_app"));
        let plaintext =
            MessagePlaintext::sign_json(&sender.keys, json.into_bytes(), OPAQUE_MEMBERS).unwrap();
        hashes.insert(st(m, "hash").to_owned(), b64(&plaintext.hash()));
        for (member, value) in [
            ("hash", b64(&plaintext.hash())),
            ("plaintext_hex", hex(&plaintext.encode())),
            ("signature", b64(&plaintext.signature())),
        ] {
            out.push((Place::At(format!("/messages/{i}/{member}")), json!(value)));
        }
    }
    for (i, case) in arr(&file["label_rejections"], "cases").iter().enumerate() {
        let sender = keys.app_by_id(st(case, "sender_app"));
        let plaintext = MessagePlaintext::sign_json(
            &sender.keys,
            st(case, "json").as_bytes().to_vec(),
            OPAQUE_MEMBERS,
        )
        .unwrap();
        let at = format!("/label_rejections/cases/{i}");
        out.push((
            Place::At(format!("{at}/signature")),
            json!(b64(&plaintext.signature())),
        ));
        if case.get("recipient_cert").is_some() {
            let recipient = match st(case, "name") {
                "an acceptance labelled shop1 at an app holding only paykit_request:shop2" => {
                    with_scopes(
                        keys,
                        "hosted",
                        &[
                            "paykit_request:shop2",
                            "paykit_settlements",
                            "paykit_contacts",
                        ],
                    )
                }
                "an acceptance without a label at an app holding only paykit_request:shop1" => {
                    cert(keys.app("hosted"))
                }
                other => panic!("no recipient certificate for {other}"),
            };
            out.push((
                Place::At(format!("{at}/recipient_cert")),
                signed_json(recipient.signed()),
            ));
        }
    }
    out
}

const CANCELLATION_DESCRIPTION: &str = "Wire 4.1 check 8, 4.3, 4.8, spec 3.3: messages.json's \
payment_request_cancellation, as the shop signs it, without a label, presented with a \
certificate of the shop's keys (sender_cert) at a certificate of alice's wallet's keys \
(recipient_cert), each with its scopes changed, its permissions those Paykit's scopes need, \
and signed again; a certificate holding paykit_pay holds paykit_contacts too, which paykit_pay requires, and one \
holding paykit_request holds paykit_settlements, which paykit_request requires (spec 3.1). A cancellation's recipient holds the other role than its sender: the \
payer's goes from paykit_pay to paykit_request, the payee's from paykit_request to \
paykit_pay. accepted are the two \
pairings the recipient takes at check 8; refused are paykit_pay to paykit_pay and paykit_request to paykit_request, \
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

/// The scopes of a payer's certificate in `cancellation_roles`: `paykit_pay`, with the
/// `paykit_contacts` it requires (Paykit spec 3.1).
const PAY: &[&str] = &["paykit_pay", "paykit_contacts"];

/// The payee's scopes of `cancellation_roles`: `paykit_request` and the `paykit_settlements`
/// it requires (Paykit spec 3.1).
const REQUEST: &[&str] = &["paykit_request", "paykit_settlements", "paykit_contacts"];

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
            case("the payee's cancellation, from paykit_request to paykit_pay", REQUEST, PAY, true),
            case("the payer's cancellation, from paykit_pay to paykit_request", PAY, REQUEST, true),
        ],
        "description": CANCELLATION_DESCRIPTION,
        "refused": [
            case("a cancellation from paykit_pay to paykit_pay", PAY, PAY, false),
            case("a cancellation from paykit_request to paykit_request", REQUEST, REQUEST, false),
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
            assert!(
                !beyond_paykit(&sender).is_empty(),
                "{name}: a permission only chat brings"
            );
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
usd-polygon) makes them invalid against the request; each rate's asset is <asset> or \
<asset>-<rail> of lower-case letters and digits, and matches an endpoint the details or \
the request name (here btc-lightning-bolt11). Where a case has request_conversion, its \
request is payment_request_recurring with its conversion terms replaced by it, or removed \
where it is null: period details carry rates only for a request with per_period conversion \
terms. accepted are messages alice takes, \
at check 10 and, for period details, against their request; refused are those she drops. \
reason is paykit-lib's wording, of the first check that refuses it.";

/// One case of `conversion_terms`: `base` with its body replaced by `body`, as the shop
/// signs it.
struct ConversionCase {
    name: &'static str,
    base: &'static str,
    body: Value,
    request: Option<&'static str>,
    /// The request's conversion terms in place of its own, or none where `null`.
    request_conversion: Option<Value>,
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
        request_conversion: None,
    };
    // Period details with a rate, against payment_request_recurring with its conversion terms
    // replaced by `conversion`, or removed where it is null.
    let against = |name, conversion: Value| ConversionCase {
        request_conversion: Some(conversion),
        ..details(name, json!([{ "asset": "btc", "value": "0.00001234" }]))
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
            request_conversion: None,
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
        details(
            "period details with a rate selector in upper case",
            json!([{ "asset": "BTC", "value": "0.00001234" }]),
        ),
        details(
            "period details with a rate selector with an empty rail",
            json!([{ "asset": "btc-", "value": "0.00001234" }]),
        ),
        details(
            "period details with a rate selector of three segments",
            json!([{ "asset": "btc-lightning-bolt11", "value": "0.00001234" }]),
        ),
        details(
            "period details with a rate whose asset matches no endpoint",
            json!([{ "asset": "eth", "value": "0.0004" }]),
        ),
        details(
            "period details with a rate whose rail matches no endpoint",
            json!([{ "asset": "btc-bitcoin", "value": "0.00001234" }]),
        ),
        against(
            "period details with rates against a request with fixed conversion terms",
            json!({ "rates": [{ "asset": "btc", "value": "0.00001234" }], "type": "fixed" }),
        ),
        against(
            "period details with rates against a request without conversion terms",
            Value::Null,
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

/// The request `name` of messages.json, with its conversion terms replaced by `conversion`,
/// or removed where it is null.
pub(crate) fn conversion_request(name: &str, conversion: Option<&Value>) -> PaymentRequest {
    let mut message = Message::parse(&message_json(name), OPAQUE_MEMBERS).unwrap();
    match conversion {
        Some(Value::Null) => {
            message.body.remove("conversion");
        }
        Some(terms) => {
            message.body.insert("conversion".into(), terms.clone());
        }
        None => {}
    }
    let Body::PaymentRequest(request) =
        Body::parse(&message).unwrap_or_else(|e| panic!("{name}: {e}"))
    else {
        panic!("{name}: a request")
    };
    request
}

/// Alice's checks of a case the shop signed: check 10, then, for period details, the checks
/// against their request, its conversion terms replaced by `request_conversion` when given
/// (Paykit wire 4.9).
fn conversion_verdict(
    keys: &Keys,
    json: &[u8],
    request: Option<&str>,
    request_conversion: Option<&Value>,
) -> paykit_lib::Result<()> {
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
        details.check_against(&conversion_request(request, request_conversion))?;
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
        if let Some(conversion) = &case.request_conversion {
            out["request_conversion"] = conversion.clone();
        }
        match conversion_verdict(keys, &json, case.request, case.request_conversion.as_ref()) {
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
            let verdict = conversion_verdict(
                keys,
                &json,
                case["request"].as_str(),
                case.get("request_conversion"),
            );
            assert_eq!(verdict.is_ok(), taken, "{name}: {verdict:?}");
            if let Err(error) = verdict {
                assert_eq!(quoted(&error.to_string()), st(case, "reason"), "{name}");
            }
        }
    }
}

const CONVERSION_AMOUNTS_DESCRIPTION: &str = "Spec 7.2 step 1, wire 4.4, 4.9, \
specs/payment-conversion.md: the amount to pay to an endpoint, payment_endpoint_identifier \
of the form <asset>-<rail>-<format>, whose smallest unit has fraction_digits fraction \
digits, for a request of requested, under rates, the request's fixed rates or the period \
details' rates, null for none. The rate is the asset-rail rate (usdt-polygon), else the \
asset-wide rate (usdt), else exactly 1 when the endpoint's asset is the requested asset; \
else the endpoint is unusable for conversion and amount is null. requested times the rate \
is computed exactly and rounded up once to the smallest unit; amount is in the endpoint's \
asset, normalized.";

/// The cases of `conversion_amounts`: name, requested value and asset, rates (selector,
/// value), endpoint identifier and its smallest unit's fraction digits.
type AmountCase = (
    &'static str,
    (&'static str, &'static str),
    Option<&'static [(&'static str, &'static str)]>,
    &'static str,
    u32,
);

/// The cases of `conversion_amounts`, from specs/payment-conversion.md's examples.
const AMOUNT_CASES: [AmountCase; 11] = {
    const USDT_RATES: &[(&str, &str)] = &[("usdt", "1"), ("usdt-polygon", "1.008")];
    const POLYGON: &[(&str, &str)] = &[("usdt-polygon", "1.008")];
    const BTC: &[(&str, &str)] = &[("btc", "0.000012345")];
    const PARITY: &[(&str, &str)] = &[("usdt", "1")];
    [
        (
            "an asset-rail rate",
            ("10", "usd"),
            Some(USDT_RATES),
            "usdt-polygon-address",
            6,
        ),
        (
            "an asset-wide rate",
            ("10", "usd"),
            Some(USDT_RATES),
            "usdt-ethereum-address",
            6,
        ),
        (
            "an asset-rail rate for the requested asset",
            ("10", "usdt"),
            Some(POLYGON),
            "usdt-polygon-address",
            6,
        ),
        (
            "the requested asset at exactly 1",
            ("10", "usdt"),
            Some(POLYGON),
            "usdt-ethereum-address",
            6,
        ),
        (
            "the requested asset without rates",
            ("10", "usdt"),
            None,
            "usdt-ethereum-address",
            6,
        ),
        (
            "the requested asset rounded up",
            ("10.0000001", "usdt"),
            None,
            "usdt-ethereum-address",
            6,
        ),
        (
            "rounded up to satoshis",
            ("0.05", "usd"),
            Some(BTC),
            "btc-onchain-address",
            8,
        ),
        (
            "exact in millisatoshis",
            ("0.05", "usd"),
            Some(BTC),
            "btc-lightning-invoice",
            11,
        ),
        (
            "a token of six decimals",
            ("0.05", "usd"),
            Some(PARITY),
            "usdt-polygon-address",
            6,
        ),
        (
            "an asset no rate prices",
            ("10", "usd"),
            Some(USDT_RATES),
            "eur-sepa-iban",
            2,
        ),
        (
            "another rail of a rail-qualified rate",
            ("10", "usd"),
            Some(POLYGON),
            "usdt-ethereum-address",
            6,
        ),
    ]
};

/// The amount paykit-lib prices for a case's inputs, as the file writes it.
fn priced_amount(case: &Value) -> Value {
    let requested = PaymentAmount {
        value: Decimal::parse(st(&case["requested"], "value")).unwrap(),
        asset: Asset::parse(st(&case["requested"], "asset")).unwrap(),
    };
    let rates: Option<Vec<Rate>> = case["rates"].as_array().map(|rates| {
        rates
            .iter()
            .map(|r| {
                Rate::new(
                    Asset::parse(st(r, "asset")).unwrap(),
                    Decimal::parse(st(r, "value")).unwrap(),
                )
                .unwrap()
            })
            .collect()
    });
    let endpoint =
        PaymentEndpointIdentifier::parse(st(case, "payment_endpoint_identifier")).unwrap();
    let digits = u32::try_from(case["fraction_digits"].as_u64().unwrap()).unwrap();
    match amount_for_endpoint(&requested, rates.as_deref(), &endpoint, digits).unwrap() {
        EndpointAmount::ByRate { amount, .. } | EndpointAmount::RequestedAsset(amount) => {
            json!({ "asset": amount.asset.as_str(), "value": amount.value.as_str() })
        }
        _ => Value::Null,
    }
}

/// `conversion_amounts`, as paykit-lib generates it.
fn conversion_amounts() -> Value {
    let cases: Vec<Value> = AMOUNT_CASES
        .iter()
        .map(|(name, (value, asset), rates, endpoint, digits)| {
            let rates = rates.map(|rates| {
                rates
                    .iter()
                    .map(|(asset, value)| json!({ "asset": asset, "value": value }))
                    .collect::<Vec<_>>()
            });
            let mut case = json!({
                "fraction_digits": digits,
                "name": name,
                "payment_endpoint_identifier": endpoint,
                "rates": rates,
                "requested": { "asset": asset, "value": value },
            });
            case["amount"] = priced_amount(&case);
            case
        })
        .collect();
    json!({ "cases": cases, "description": CONVERSION_AMOUNTS_DESCRIPTION })
}

/// `conversion_amounts` (spec 7.2 step 1, Paykit wire 4.4, 4.9): each case's amount is the
/// one paykit-lib prices for its inputs.
fn check_conversion_amounts(file: &Value) {
    let m = &file["conversion_amounts"];
    let cases = arr(m, "cases");
    assert_eq!(cases.len(), AMOUNT_CASES.len());
    for case in cases {
        assert_eq!(priced_amount(case), case["amount"], "{}", st(case, "name"));
    }
}

const PROOF_MEMBERS_DESCRIPTION: &str = "Wire 4.10: alice's proofs with one member added or \
removed and signed again by her wallet. amount is only in a proof of a payment without a \
request, whose refs are empty; billing_period, label, payment_reference and allowance_id are \
only in a proof of a request. A proof that refers to period details must name their Billing \
Period. A payee drops each at check 10. reason is paykit-lib's wording.";

/// Name, base proof, body member and value to add, or `None` to remove the member.
type ProofCase = (&'static str, &'static str, &'static str, Option<Value>);

fn proof_cases() -> Vec<ProofCase> {
    vec![
        (
            "a request's proof carrying amount",
            "payment_proof",
            "amount",
            Some(json!({ "asset": "btc", "value": "0.00025" })),
        ),
        (
            "a proof without a request carrying billing_period",
            "payment_proof_unrequested",
            "billing_period",
            Some(
                json!({ "ends_at": "2026-03-31T00:00:00.000Z", "starts_at": "2026-02-28T00:00:00.000Z" }),
            ),
        ),
        (
            "a proof without a request carrying label",
            "payment_proof_unrequested",
            "label",
            Some(json!("shop1")),
        ),
        (
            "a proof without a request carrying payment_reference",
            "payment_proof_unrequested",
            "payment_reference",
            Some(json!("order-1042")),
        ),
        (
            "a proof without a request carrying allowance_id",
            "payment_proof_unrequested",
            "allowance_id",
            Some(json!(crate::common::ALLOWANCE_ID)),
        ),
        (
            "a proof referring to period details without billing_period",
            "payment_proof_period",
            "billing_period",
            None,
        ),
    ]
}

/// `proof_members`, as paykit-lib generates it.
fn proof_members(keys: &Keys) -> Value {
    let wallet = keys.app("wallet");
    let refused: Vec<Value> = proof_cases()
        .into_iter()
        .map(|(name, base, member, value)| {
            let original = message_json(base);
            let mut m = Message::parse(&original, OPAQUE_MEMBERS).unwrap();
            check_body(&m, &original).expect(base);
            match value {
                Some(value) => assert!(m.body.insert(member.into(), value).is_none(), "{name}"),
                None => assert!(m.body.remove(member).is_some(), "{name}"),
            }
            let json = m.to_json().unwrap();
            assert_eq!(m.from.app_id, wallet.keys.app_id_bytes(), "{name}");
            let plaintext =
                MessagePlaintext::sign_json(&wallet.keys, json.clone(), OPAQUE_MEMBERS).unwrap();
            let error = check_body(&m, &json).expect_err(name);
            json!({
                "base": base,
                "json": String::from_utf8(json).unwrap(),
                "name": name,
                "reason": quoted(&error.to_string()),
                "sender_app": wallet.app_id,
                "signature": b64(&plaintext.signature()),
            })
        })
        .collect();
    json!({ "description": PROOF_MEMBERS_DESCRIPTION, "refused": refused })
}

/// `proof_members` (Paykit wire 4.10): the wallet's signatures, and the payee dropping each
/// at check 10.
fn check_proof_members(keys: &Keys, file: &Value) {
    let wallet = keys.app("wallet");
    let cases = arr(&file["proof_members"], "refused");
    assert_eq!(cases.len(), proof_cases().len());
    for case in cases {
        let name = st(case, "name");
        let json = st(case, "json").as_bytes().to_vec();
        let plaintext =
            MessagePlaintext::sign_json(&wallet.keys, json.clone(), OPAQUE_MEMBERS).unwrap();
        assert_eq!(b64(&plaintext.signature()), st(case, "signature"), "{name}");
        let message = plaintext
            .check(
                &pubky(BOB),
                &wallet.identity,
                &VerifiedSender::from_cert(&cert(wallet)),
                OPAQUE_MEMBERS,
            )
            .unwrap();
        let error = check_body(&message, &json).expect_err(name);
        assert_eq!(quoted(&error.to_string()), st(case, "reason"), "{name}");
    }
}

const RECEIPT_MEMBERS_DESCRIPTION: &str = "Wire 4.11: bob's receipts, payment_receipt as the \
shop signs it, with members added and signed again by the shop. A receipt may carry the \
payee's metadata, an object, as v0.1 Receipt.metadata; and a receipt of a payment without a \
request, whose refs name its proof alone (here payment_proof_unrequested, with that proof's \
attempt_id), may carry the payee's payment_reference for it. accepted are receipts a payer \
takes at check 10: the request's receipt with metadata, and the receipt of the payment \
without a request with metadata and a payment_reference. refused are receipts it drops \
there: the request's receipt carrying a payment_reference, whose request's reference \
applies, one whose metadata isn't an object, and a receipt naming a Billing Period without a \
request. reason is paykit-lib's wording.";

/// A case of `receipt_members`: name, whether it confirms the payment without a request, and
/// the members added.
type ReceiptCase = (&'static str, bool, Vec<(&'static str, Value)>);

/// The cases of `receipt_members`, accepted then refused.
fn receipt_cases() -> (Vec<ReceiptCase>, Vec<ReceiptCase>) {
    let metadata = json!({ "note": "thank you", "order": "1042" });
    let mut without_request = vec![
        ("metadata", metadata.clone()),
        ("payment_reference", json!("invoice-77")),
    ];
    let accepted = vec![
        (
            "a request's receipt with metadata",
            false,
            vec![("metadata", metadata)],
        ),
        (
            "a receipt of a payment without a request with metadata and payment_reference",
            true,
            without_request.clone(),
        ),
    ];
    let period_proof =
        Message::parse(&message_json("payment_proof_period"), OPAQUE_MEMBERS).unwrap();
    without_request.push((
        "billing_period",
        period_proof.body["billing_period"].clone(),
    ));
    let refused = vec![
        (
            "a request's receipt carrying payment_reference",
            false,
            vec![("payment_reference", json!("order-1042"))],
        ),
        (
            "a receipt whose metadata isn't an object",
            false,
            vec![("metadata", json!("thank you"))],
        ),
        (
            "a receipt of a payment without a request naming a Billing Period",
            true,
            without_request,
        ),
    ];
    (accepted, refused)
}

/// The receipt of a case, as the shop signs it.
fn receipt_json((_, unrequested, members): &ReceiptCase) -> (Message, Vec<u8>) {
    let mut m = Message::parse(&message_json("payment_receipt"), OPAQUE_MEMBERS).unwrap();
    if *unrequested {
        let proof =
            Message::parse(&message_json("payment_proof_unrequested"), OPAQUE_MEMBERS).unwrap();
        m.refs = vec![paykit_lib::derive::request_hash(&message_json(
            "payment_proof_unrequested",
        ))];
        m.body
            .insert("attempt_id".into(), proof.body["attempt_id"].clone());
    }
    for (member, value) in members {
        m.body.insert((*member).into(), value.clone());
    }
    let json = m.to_json().unwrap();
    (m, json)
}

/// `receipt_members`, as paykit-lib generates it.
fn receipt_members(keys: &Keys) -> Value {
    let shop = keys.app("shop");
    let (accepted, refused) = receipt_cases();
    let case = |c: &ReceiptCase, taken: bool| -> Value {
        let (m, json) = receipt_json(c);
        assert_eq!(m.from.app_id, shop.keys.app_id_bytes(), "{}", c.0);
        let plaintext =
            MessagePlaintext::sign_json(&shop.keys, json.clone(), OPAQUE_MEMBERS).unwrap();
        let mut out = json!({
            "json": String::from_utf8(json.clone()).unwrap(),
            "name": c.0,
            "sender_app": shop.app_id,
            "signature": b64(&plaintext.signature()),
        });
        match check_body(&m, &json) {
            Ok(_) => assert!(taken, "{}: taken", c.0),
            Err(error) => {
                assert!(!taken, "{}: {error}", c.0);
                out["reason"] = json!(quoted(&error.to_string()));
            }
        }
        out
    };
    json!({
        "accepted": accepted.iter().map(|c| case(c, true)).collect::<Vec<_>>(),
        "description": RECEIPT_MEMBERS_DESCRIPTION,
        "refused": refused.iter().map(|c| case(c, false)).collect::<Vec<_>>(),
    })
}

/// `receipt_members` (Paykit wire 4.11): the shop's signatures, and alice taking or dropping
/// each at check 10; a receipt taken reads its members back.
fn check_receipt_members(keys: &Keys, file: &Value) {
    let shop = keys.app("shop");
    let (accepted, refused) = receipt_cases();
    let m = &file["receipt_members"];
    assert_eq!(arr(m, "accepted").len(), accepted.len());
    assert_eq!(arr(m, "refused").len(), refused.len());
    for (list, taken) in [("accepted", true), ("refused", false)] {
        for case in arr(m, list) {
            let name = st(case, "name");
            let json = st(case, "json").as_bytes().to_vec();
            let plaintext =
                MessagePlaintext::sign_json(&shop.keys, json.clone(), OPAQUE_MEMBERS).unwrap();
            assert_eq!(b64(&plaintext.signature()), st(case, "signature"), "{name}");
            let message = plaintext
                .check(
                    &pubky(ALICE),
                    &shop.identity,
                    &VerifiedSender::from_cert(&cert(shop)),
                    OPAQUE_MEMBERS,
                )
                .unwrap();
            match check_body(&message, &json) {
                Ok(Body::PaymentReceipt(r)) => {
                    assert!(taken, "{name}: taken");
                    assert!(r.metadata.is_some(), "{name}");
                    assert_eq!(
                        r.payment_reference.is_some(),
                        message.refs.len() == 1,
                        "{name}"
                    );
                    let body = r.to_body(message.refs.len()).unwrap();
                    assert_eq!(body, message.body, "{name}: writes back");
                }
                Ok(other) => panic!("{name}: {:?}", other.kind()),
                Err(error) => {
                    assert!(!taken, "{name}: {error}");
                    assert_eq!(quoted(&error.to_string()), st(case, "reason"), "{name}");
                }
            }
        }
    }
}

const LIST_VERSIONS_DESCRIPTION: &str = "Wire 4.13: the shop's private_payment_list with its \
version replaced by a malformed one and signed again by the shop; every other member is the \
list's. A version is an object with generation and counter, each 1 through 9007199254740991. \
A payer drops each at check 10. reason is paykit-lib's wording.";

/// The cases of `list_versions`: name, the `version` written, and the words of the rule it
/// breaks.
fn list_version_cases() -> Vec<(&'static str, Value, &'static str)> {
    vec![
        (
            "a list whose version has no counter",
            json!({ "generation": 1 }),
            "counter: missing",
        ),
        (
            "a list whose version has a zero counter",
            json!({ "counter": 0, "generation": 1 }),
            "must be positive",
        ),
        (
            "a list whose version is an integer",
            json!(12),
            "list-version: not an object",
        ),
    ]
}

/// The shop's `private_payment_list` with `version` replaced.
fn list_with_version(version: Value) -> Message {
    let mut m = Message::parse(&message_json("private_payment_list"), OPAQUE_MEMBERS).unwrap();
    assert!(m.body.insert("version".into(), version).is_some());
    m
}

/// `list_versions`, as paykit-lib generates it.
fn list_versions(keys: &Keys) -> Value {
    let shop = keys.app("shop");
    let refused: Vec<Value> = list_version_cases()
        .into_iter()
        .map(|(name, version, _)| {
            let m = list_with_version(version);
            assert_eq!(m.from.app_id, shop.keys.app_id_bytes(), "{name}");
            let json = m.to_json().unwrap();
            let plaintext =
                MessagePlaintext::sign_json(&shop.keys, json.clone(), OPAQUE_MEMBERS).unwrap();
            let error = check_body(&m, &json).expect_err(name);
            json!({
                "json": String::from_utf8(json).unwrap(),
                "name": name,
                "reason": quoted(&error.to_string()),
                "sender_app": shop.app_id,
                "signature": b64(&plaintext.signature()),
            })
        })
        .collect();
    json!({ "description": LIST_VERSIONS_DESCRIPTION, "refused": refused })
}

/// `list_versions` (Paykit wire 4.13): the shop's signatures, alice's wallet dropping each at
/// check 10 for its version's rule, and the same list with the stored version taken.
fn check_list_versions(keys: &Keys, file: &Value) {
    let shop = keys.app("shop");
    let cases = arr(&file["list_versions"], "refused");
    let expected = list_version_cases();
    assert_eq!(cases.len(), expected.len());
    let held = Message::parse(&message_json("private_payment_list"), OPAQUE_MEMBERS).unwrap();
    for (case, (name, version, rule)) in cases.iter().zip(expected) {
        assert_eq!(st(case, "name"), name);
        let json = st(case, "json").as_bytes().to_vec();
        let plaintext =
            MessagePlaintext::sign_json(&shop.keys, json.clone(), OPAQUE_MEMBERS).unwrap();
        assert_eq!(b64(&plaintext.signature()), st(case, "signature"), "{name}");
        let message = plaintext
            .check(
                &pubky(ALICE),
                &shop.identity,
                &VerifiedSender::from_cert(&cert(shop)),
                OPAQUE_MEMBERS,
            )
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(message.body["version"], version, "{name}");
        let error = check_body(&message, &json).expect_err(name).to_string();
        assert!(error.contains(rule), "{name}: {error}");
        assert_eq!(quoted(&error), st(case, "reason"), "{name}");
        // Only the version differs: with the stored list's version, the case is that list.
        let restored = Message {
            body: {
                let mut body = message.body.clone();
                body.insert("version".into(), held.body["version"].clone());
                body
            },
            ..message
        };
        let restored_json = restored.to_json().unwrap();
        assert_eq!(
            restored_json,
            message_json("private_payment_list"),
            "{name}"
        );
        assert!(check_body(&restored, &restored_json).is_ok(), "{name}");
    }
}
