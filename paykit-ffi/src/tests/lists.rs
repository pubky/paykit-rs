//! Private Payment Lists through the wallet bindings (Paykit spec 8.4.1).

use super::{
    method::Account,
    refusal::categorized,
    wallet::{enrolled_payer, host, run},
    TestResult,
};
use crate::{
    fixed, ListEndpoint, ListPublicationConflict, ListPublicationRecord, NativeWallet, RefusalKind,
    StorageError, WalletError,
};
use paykit_lib::{bodies::PrivatePaymentList, collections::Collection, derive::natural_key};
use pubky_switch_client::app::{Collection as SwitchCollection, LocalWrite, RecordTarget};
use pubky_switch_core::{
    enc::{b64, pubky_id, zbase32_encode},
    ident::Policy,
    message::{Message, Sender},
    records::{AppCert, AppCertBody},
};
use pubky_switch_harness::{Backend, World};
use serde_json::{json, Value};
use std::sync::Arc;

fn endpoint(identifier: &str, payload: &str) -> ListEndpoint {
    ListEndpoint {
        identifier: identifier.into(),
        payload: payload.into(),
    }
}

#[test]
fn test_list_endpoint_debug_shows_only_payload_length() {
    let value = endpoint("btc-lightning-bolt11", "private-\u{e9}");
    assert_eq!(format!("{value:?}"), "ListEndpoint { payload_bytes: 10 }");
}

#[test]
fn test_lists_set_endpoints_preserves_each_payload_and_accepts_an_empty_list() -> TestResult {
    let endpoints = crate::lists::list_endpoints(vec![
        endpoint("btc-onchain", "address"),
        endpoint("btc-lightning-bolt11", "invoice"),
    ])?;
    let pairs: Vec<_> = endpoints
        .iter()
        .map(|(id, payload)| (id.as_str(), payload.as_str()))
        .collect();
    assert_eq!(
        pairs,
        [
            ("btc-lightning-bolt11", "invoice"),
            ("btc-onchain", "address")
        ]
    );
    assert!(crate::lists::list_endpoints(Vec::new())?.is_empty());
    Ok(())
}

async fn check_sharing(wallet: &NativeWallet, recipient: &[u8; 32]) -> TestResult {
    let valid = || endpoint("btc-lightning-bolt11", "lnbc1example");
    for (endpoints, name) in [
        (vec![endpoint("bad/id", "lnbc1example")], "identifier"),
        (vec![endpoint("btc-onchain", "")], "payload"),
        (
            vec![valid(), endpoint("btc-lightning-bolt11", "another")],
            "identifier",
        ),
    ] {
        assert!(matches!(
            wallet.set_list_endpoints(endpoints).await,
            Err(WalletError::InvalidInput { field }) if field == name
        ));
    }

    let error = wallet
        .set_list_endpoints(vec![valid()])
        .await
        .err()
        .ok_or("payer set list endpoints")?;
    let (case, failure) = categorized(error).ok_or("expected a selling scope refusal")?;
    assert_eq!(case, "needs_user");
    assert_eq!(failure.code(), "scope_missing");
    assert!(matches!(
        failure.refusal().map(|r| r.kind()),
        Some(RefusalKind::ScopeMissing { scope }) if scope == "paykit_request"
    ));

    let error = wallet
        .publish_list_to(recipient.to_vec())
        .await
        .err()
        .ok_or("payer published a list")?;
    let (case, failure) = categorized(error).ok_or("expected a publication refusal")?;
    assert_eq!(case, "needs_user");
    assert_eq!(failure.code(), "not_now");

    for result in [
        wallet.publish_list_to(vec![1; 31]).await,
        wallet.withdraw_list_from(vec![2; 33]).await,
    ] {
        assert!(matches!(result, Err(WalletError::InvalidInput { field }) if field == "recipient"));
    }
    wallet.withdraw_list_from(recipient.to_vec()).await?;
    // Only the refusal of a list delivered with an unknown outcome is a confirmation to send
    // another; any other is the caller's mistake, refused before anything is read.
    let other = Arc::new(crate::PaymentRefusal(paykit_sdk::Refusal::NotFound));
    assert!(matches!(
        wallet.send_list_again(other).await,
        Err(WalletError::InvalidInput { field }) if field == "refusal"
    ));
    Ok(())
}

// Put a retained SDK value at the real record key derived from the wallet's naming key.
async fn retain(
    wallet: &NativeWallet,
    family: &str,
    target: &RecordTarget,
    data: &Value,
) -> TestResult {
    wallet
        .lend(async |app| -> TestResult {
            let record = app.app().record_key(target)?;
            let write = LocalWrite::Put {
                key: format!("{family}/{}", b64(&record)).into_bytes(),
                value: serde_json::to_vec(&json!({ "v": 1, "d": data }))?,
            };
            app.app_mut().protocol_commit("paykit", &[write]).await?;
            Ok(())
        })
        .await?
}

async fn check_held_list(wallet: &NativeWallet, world: &World) -> TestResult {
    assert!(wallet.held_lists().await?.is_empty());
    let payee = world.identity("shop")?;
    let keys = world.app_keys(&payee, "seller")?;
    let app_id = keys.app_id_bytes();
    let now = world.now_ms();
    let expiry = now.checked_add(86_400_000).ok_or("list expiry overflow")?;
    let scopes = paykit_sdk::AppScopes::new().request().build()?;
    let cert = AppCert::issue(
        payee.key(),
        AppCertBody::new(
            payee.public(),
            keys.signing_public(),
            keys.bundle_public(),
            scopes.scopes,
            &scopes.permissions,
            now,
            expiry,
        ),
    )?;
    let list = PrivatePaymentList {
        version: paykit_lib::ListVersion::new(3, 7)?,
        payment_endpoints: Default::default(),
        expires_at: expiry,
    };
    let message = Message::new(
        "paykit.private_payment_list",
        Sender::of_app(payee.public(), &keys),
        fixed(wallet.identity().await?, "identity")?,
        now,
        vec![],
        list.to_body()?,
    )?;
    let collection = Collection::PaymentLists;
    let target = RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &natural_key::payment_list(&payee.public(), &app_id),
    );
    retain(
        wallet,
        "lh",
        &target,
        &json!({
            "sender": b64(&payee.public()), "cert": cert.signed(),
            "plaintext": b64(&message.sign(&keys)?.encode()), "accepted_at": now,
        }),
    )
    .await?;
    let [held]: [_; 1] = wallet
        .held_lists()
        .await?
        .try_into()
        .map_err(|_| "expected one held list")?;
    assert_eq!(held.payee, payee.public());
    assert_eq!(held.app_id, app_id);
    assert_eq!((held.version.generation, held.version.counter), (3, 7));
    assert_eq!(held.msg_id, message.msg_id);

    retain(wallet, "lh", &target, &Value::Null).await?;
    assert!(matches!(
        wallet.held_lists().await,
        Err(WalletError::Storage {
            failure: StorageError::Corrupt
        })
    ));
    Ok(())
}

const HELD_ID: &str = "3198762f-3816-4cd4-86e2-4a8dc40fc737";
const REFUSED_ID: &str = "a5dbfc07-4d85-4b39-bd47-29ff28493ce1";

async fn check_conflicts(wallet: &NativeWallet, recipient: &[u8; 32]) -> TestResult {
    assert!(wallet.publication_conflicts().await?.is_empty());
    let publisher = fixed(wallet.app_id().await?, "publisher")?;
    let target = RecordTarget::new(
        SwitchCollection::new(
            "private_list_publications",
            Policy::new("paykit_request", &["paykit_request"]).permanent(Some(3)),
        ),
        &natural_key::list_publication(&publisher, recipient, 3),
    );
    let issue = |counter: u64, msg_id: &str, hash: u8| {
        json!({
            "v": 1, "publisher": zbase32_encode(&publisher), "recipient": pubky_id(recipient),
            "generation": 3, "counter": counter, "msg_id": msg_id, "message_hash": b64(&[hash; 32]),
        })
    };
    let held = issue(9, HELD_ID, 4);
    let refused = issue(8, REFUSED_ID, 5);
    let history = |floor: &Value| {
        json!({
            "floor": floor, "missing_since": null, "acknowledged": false,
            "conflict": { "application_hash": b64(&[6; 32]), "other_applications": true,
                          "held": held, "refused": refused },
        })
    };
    retain(wallet, "lhi", &target, &history(&held)).await?;
    let [mut report]: [_; 1] = wallet
        .publication_conflicts()
        .await?
        .try_into()
        .map_err(|_| "expected one conflict")?;
    assert_eq!(report.application_hash, [6; 32]);
    assert!(report.other_applications);
    for (record, counter, id, hash) in [
        (&report.held, 9, HELD_ID, 4),
        (&report.refused, 8, REFUSED_ID, 5),
    ] {
        assert_eq!(record.publisher, publisher);
        assert_eq!(&record.recipient, recipient);
        assert_eq!((record.generation, record.counter), (3, counter));
        assert_eq!(record.msg_id, id);
        assert_eq!(record.message_hash, [hash; 32]);
    }
    report.other_applications = false;
    wallet.acknowledge_publication_conflict(report).await?;
    let [exact]: [_; 1] = wallet
        .publication_conflicts()
        .await?
        .try_into()
        .map_err(|_| "a stale acknowledgment cleared the conflict")?;
    wallet
        .acknowledge_publication_conflict(exact.clone())
        .await?;
    assert!(wallet.publication_conflicts().await?.is_empty());

    // The retained floor cannot be lower than the held counter (Paykit wire 5.8).
    retain(wallet, "lhi", &target, &history(&refused)).await?;
    for error in [
        wallet.publication_conflicts().await.err(),
        wallet.acknowledge_publication_conflict(exact).await.err(),
    ] {
        let (case, failure) = error
            .and_then(categorized)
            .ok_or("expected a publication counter refusal")?;
        assert_eq!(case, "needs_user");
        assert_eq!(failure.code(), "not_now");
    }
    Ok(())
}

fn conflict() -> ListPublicationConflict {
    let record = |counter, msg_id: &str| ListPublicationRecord {
        publisher: vec![1; 16],
        recipient: vec![2; 32],
        generation: 3,
        counter,
        msg_id: msg_id.into(),
        message_hash: vec![4; 32],
    };
    ListPublicationConflict {
        application_hash: vec![5; 32],
        other_applications: false,
        held: record(2, HELD_ID),
        refused: record(1, REFUSED_ID),
    }
}

#[test]
fn test_publication_conflict_refuses_malformed_fields() {
    assert!(paykit_sdk::PublicationConflict::try_from(conflict()).is_ok());
    let invalid = |edit: fn(&mut ListPublicationConflict), name: &str| {
        let mut report = conflict();
        edit(&mut report);
        assert!(matches!(
            paykit_sdk::PublicationConflict::try_from(report),
            Err(WalletError::InvalidInput { field }) if field == name
        ));
    };
    invalid(|r| r.application_hash.clear(), "application_hash");
    invalid(|r| r.held.publisher.clear(), "held.publisher");
    invalid(|r| r.refused.recipient.clear(), "refused.recipient");
    invalid(|r| r.refused.message_hash.clear(), "refused.message_hash");
    invalid(|r| r.held.generation = u64::MAX, "held.generation");
    invalid(|r| r.refused.counter = 0, "refused.counter");
    invalid(
        |r| r.held.msg_id = "private host input".into(),
        "held.msg_id",
    );
}

async fn lists(world: &World) -> TestResult {
    let root = tempfile::tempdir()?;
    let account = Arc::new(Account::open(root.path(), world.clock().clone()));
    let (_, wallet, app_id, _) =
        Box::pin(enrolled_payer(world, root.path(), account.clone())).await?;
    let recipient = world.identity("shop")?.public();
    check_sharing(&wallet, &recipient).await?;
    let mut malformed = conflict();
    malformed.refused.msg_id.clear();
    assert!(matches!(
        wallet.acknowledge_publication_conflict(malformed).await,
        Err(WalletError::InvalidInput { .. })
    ));
    wallet.shutdown().await?;
    let reopened = host(world, root.path(), account).load(app_id).await?;
    check_conflicts(&reopened, &recipient).await?;
    check_held_list(&reopened, world).await?;
    reopened.shutdown().await?;
    Ok(())
}

#[test]
fn test_lists_payer_sharing_and_retained_records_on_scripted() {
    run(Backend::Scripted, lists);
}

#[test]
fn test_lists_payer_sharing_and_retained_records_on_service() {
    run(Backend::Service, lists);
}

async fn restored_list_delivery(world: &World) -> TestResult {
    use crate::{RuntimeTimer, WalletScopes};
    use paykit_sdk::{AppScopes, Paykit};
    use pubky_switch_client::store::MemoryStore;
    use pubky_switch_harness::parties::Asking;

    let root = tempfile::tempdir()?;
    let account = Arc::new(Account::open(root.path(), world.clock().clone()));
    let scopes = WalletScopes {
        request: true,
        ..Default::default()
    };
    let (host, seller, id, _) = Box::pin(super::wallet::enrolled_wallet(
        world,
        root.path(),
        account,
        scopes,
        None,
    ))
    .await?;
    let peer = world.identity("payer")?;
    let mut signer = Box::pin(super::signer::serving(world, "payer")).await?;
    let scopes = AppScopes::new().pay().build()?;
    let names: Vec<_> = scopes.scopes.iter().map(String::as_str).collect();
    let asking = Asking::for_scopes(world.app_keys(&peer, "wallet")?, &names)?;
    let review = signer.review(asking.request.signed(), &asking.auth)?;
    let approved = Box::pin(signer.approve(review)).await?;
    let mut payer = Box::pin(Paykit::join(
        world.app_env(Arc::new(MemoryStore::new())),
        Default::default(),
        asking.join_request(peer.public(), &approved)?,
        seller.rail(),
        Arc::new(RuntimeTimer),
    ))
    .await?;
    signer.claim_delivered(&approved.request).await?;
    let publisher = fixed(seller.identity().await?, "identity")?;
    Box::pin(payer.allow_connect(&publisher)).await?;
    while seller.receive().await?.more {}
    let backup = seller.backup().await?;
    seller.shutdown().await?;
    let seller = Box::pin(crate::lists::restored_list_seller(
        world,
        &host,
        id,
        backup,
        peer.public(),
    ))
    .await?;
    let Err(WalletError::NeedsUser { failure }) =
        seller.publish_list_to(peer.public().into()).await
    else {
        return Err("expected a list delivery refusal".into());
    };
    let refusal = failure.refusal().ok_or("missing list refusal")?;
    let RefusalKind::ListDeliveryUnknown {
        msg_id,
        recipient,
        withdrawal: false,
        ..
    } = refusal.kind()
    else {
        return Err("expected the unknown nonempty list".into());
    };
    assert_eq!(recipient, peer.public());
    seller.send_list_again(refusal).await?;
    let mut ids = Vec::new();
    for _ in 0..8 {
        let report = Box::pin(payer.receive(None)).await?.report;
        for event in report.events {
            if let pubky_switch_client::app::AppEvent::MessageReceived {
                kind,
                msg_id,
                sender,
                ..
            } = event
            {
                if kind == "paykit.private_payment_list" && sender == publisher {
                    ids.push(msg_id);
                }
            }
        }
        if ids.len() >= 2 {
            break;
        }
    }
    assert_eq!(ids.len(), 2);
    assert_eq!(ids.first(), Some(&msg_id));
    assert_ne!(ids.first(), ids.last());
    seller.shutdown().await?;
    Ok(())
}

#[test]
fn test_lists_restored_seller_sends_another_list_after_confirmation() {
    run(Backend::Scripted, restored_list_delivery);
    run(Backend::Service, restored_list_delivery);
}
