//! Public Payment Endpoints (Paykit spec 4) and outbox delivery (Switch spec 11).

use super::{
    method::Account,
    refusal::categorized,
    signer,
    wallet::{enrolled_payer, host, run},
    TestResult,
};
use crate::{
    MessageDelivery, NativeWallet, PublicEndpoint, PublicationState, RuntimeTimer, WalletError,
};
use paykit_lib::{
    Asset, Decimal, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload,
    PaymentReference,
};
use paykit_sdk::{AppScopes, Paykit, RequestDestination, RequestDraft};
use pubky_switch_client::{app::Publication, store::MemoryStore, transport::TransportError};
use pubky_switch_harness::{parties::Asking, Backend, World};
use std::{collections::BTreeMap, error::Error, sync::Arc};

const IDENTIFIER: &str = "btc-lightning-bolt11";
const UNKNOWN_ID: &str = "00000000-0000-4000-8000-000000000000";

fn endpoint(identifier: &str, payload: &str) -> PublicEndpoint {
    PublicEndpoint {
        identifier: identifier.into(),
        payload: payload.into(),
    }
}

// The test wallet is enrolled to pay only, so its writes of public endpoints are refused for
// the selling scope. A successful publication needs a wallet enrolled with that scope, which
// the choice of scopes at enrolment brings.
async fn public_endpoints(world: &World) -> TestResult {
    let root = tempfile::tempdir()?;
    let account = Arc::new(Account::open(root.path(), world.clock().clone()));
    let (_, wallet, _, _) = Box::pin(enrolled_payer(world, root.path(), account)).await?;
    assert!(wallet.own_public_endpoints().await?.is_empty());
    let offer = endpoint(IDENTIFIER, "lnbc1public");
    for result in [
        wallet
            .set_public_endpoint(offer.identifier.clone(), offer.payload.clone())
            .await,
        wallet.set_public_endpoints(vec![offer], None).await,
    ] {
        let (case, failure) = result
            .err()
            .and_then(categorized)
            .ok_or("expected a categorized publication refusal")?;
        assert_eq!(case, "needs_user");
        assert_eq!(failure.code(), "scope_missing");
    }
    assert_eq!(
        wallet.remove_public_endpoint(IDENTIFIER.into()).await?,
        PublicationState::NotPublishing
    );
    assert_eq!(
        wallet
            .set_public_endpoints(vec![], Some(world.now_ms()))
            .await?,
        PublicationState::NotPublishing
    );
    assert_eq!(
        wallet.update_own_record().await?,
        PublicationState::NotPublishing
    );
    assert!(wallet.own_public_endpoints().await?.is_empty());
    wallet.shutdown().await?;
    assert!(matches!(
        wallet.own_public_endpoints().await,
        Err(WalletError::Closed)
    ));
    assert!(matches!(
        wallet.update_own_record().await,
        Err(WalletError::Closed)
    ));
    Ok(())
}

#[test]
fn test_public_endpoints_without_a_selling_grant() {
    run(Backend::Scripted, public_endpoints);
    run(Backend::Service, public_endpoints);
}

async fn invalid_endpoints(world: &World) -> TestResult {
    let root = tempfile::tempdir()?;
    let account = Arc::new(Account::open(root.path(), world.clock().clone()));
    let (_, wallet, _, _) = Box::pin(enrolled_payer(world, root.path(), account)).await?;
    for (result, expected) in [
        (
            wallet
                .set_public_endpoint("bad/id".into(), "valid".into())
                .await,
            "identifier",
        ),
        (
            wallet.remove_public_endpoint("bad/id".into()).await,
            "identifier",
        ),
        (
            wallet
                .set_public_endpoint(IDENTIFIER.into(), String::new())
                .await,
            "payload",
        ),
        (
            wallet
                .set_public_endpoints(vec![endpoint("bad/id", "valid")], None)
                .await,
            "identifier",
        ),
        (
            wallet
                .set_public_endpoints(vec![endpoint(IDENTIFIER, "")], None)
                .await,
            "payload",
        ),
        (
            wallet
                .set_public_endpoints(
                    vec![
                        endpoint(IDENTIFIER, "first"),
                        endpoint(IDENTIFIER, "second"),
                    ],
                    None,
                )
                .await,
            "identifier",
        ),
    ] {
        assert!(matches!(result, Err(WalletError::InvalidInput { field }) if field == expected));
    }
    assert!(wallet.own_public_endpoints().await?.is_empty());
    wallet.shutdown().await?;
    Ok(())
}

#[test]
fn test_public_endpoint_invalid_input_names_only_the_field() {
    run(Backend::Scripted, invalid_endpoints);
    run(Backend::Service, invalid_endpoints);
}

async fn cancel_received_request(
    world: &World,
    payer: &NativeWallet,
) -> Result<String, Box<dyn Error>> {
    let shop = world.identity("shop")?;
    let mut issuer = Box::pin(signer::serving(world, "shop")).await?;
    let scopes = AppScopes::new().request().manage_contacts().build()?;
    let names: Vec<_> = scopes.scopes.iter().map(String::as_str).collect();
    let asking = Asking::for_scopes(world.app_keys(&shop, "wallet")?, &names)?;
    let review = issuer.review(asking.request.signed(), &asking.auth)?;
    let approval = Box::pin(issuer.approve(review)).await?;
    let mut seller = Box::pin(Paykit::join(
        world.app_env(Arc::new(MemoryStore::new())),
        Default::default(),
        asking.join_request(shop.public(), &approval)?,
        payer.rail(),
        Arc::new(RuntimeTimer),
    ))
    .await?;
    issuer.claim_delivered(&approval.request).await?;
    let buyer = world.identity("buyer")?.public();
    // The payer invites the shop, as a host does for a seller it chose (Switch spec 7).
    let invitation = payer
        .lend(async |app| -> Result<_, Box<dyn Error>> {
            while Box::pin(app.receive(None)).await?.report.more {}
            Ok(app
                .app_mut()
                .register_invite_code(world.now_ms() + 3_600_000, 1)
                .await?)
        })
        .await??;
    while Box::pin(seller.receive(None)).await?.report.more {}
    seller.app_mut().allow_connect(&buyer).await?;
    let draft = RequestDraft::new(
        PaymentAmount {
            asset: Asset::new("btc")?,
            value: Decimal::new("7")?,
        },
        PaymentReference::new("checkout")?,
        RequestDestination::Endpoints(BTreeMap::from([(
            PaymentEndpointIdentifier::new(IDENTIFIER)?,
            PaymentEndpointPayload::new("lnbc1checkout")?,
        )])),
    )
    .with_invite_code(invitation.code);
    let sent = Box::pin(seller.send_request(buyer, draft)).await?;
    let request = sent.request.ok_or("missing signed request")?;
    while Box::pin(payer.receive()).await?.more {}
    seller.app_mut().remove_conversation(&buyer).await?;
    let msg_id = payer.cancel_request(request.into(), None).await?;
    assert!(matches!(
        payer.message_delivery(msg_id.clone()).await?,
        Some(MessageDelivery::WaitingForAdmission { since_ms, tries })
            if since_ms == world.now_ms() && tries > 0
    ));
    Box::pin(seller.allow_connect(&buyer)).await?;
    Ok(msg_id)
}

async fn outbox(world: &World) -> TestResult {
    let root = tempfile::tempdir()?;
    let account = Arc::new(Account::open(root.path(), world.clock().clone()));
    let (_, payer, app_id, _) =
        Box::pin(enrolled_payer(world, root.path(), account.clone())).await?;
    assert_eq!(payer.message_delivery(UNKNOWN_ID.into()).await?, None);
    let msg_id = Box::pin(cancel_received_request(world, &payer)).await?;
    payer.flush().await?;
    let delivery = payer.message_delivery(msg_id.clone()).await?;
    assert!(matches!(delivery, Some(MessageDelivery::Accepted { .. })));
    payer.shutdown().await?;
    assert!(matches!(payer.flush().await, Err(WalletError::Closed)));
    assert!(matches!(
        payer.message_delivery(msg_id.clone()).await,
        Err(WalletError::Closed)
    ));
    let reopened = host(world, root.path(), account).load(app_id).await?;
    assert_eq!(reopened.message_delivery(msg_id).await?, delivery);
    reopened.shutdown().await?;
    Ok(())
}

#[test]
fn test_outbox_retains_the_cancellation_delivery() {
    run(Backend::Scripted, outbox);
    run(Backend::Service, outbox);
}

#[test]
fn test_public_endpoint_debug_shows_only_payload_length() {
    let offer = endpoint(IDENTIFIER, "destin\u{e9}");
    assert_eq!(
        format!("{offer:?}"),
        r#"PublicEndpoint { identifier: "btc-lightning-bolt11", payload_bytes: 8 }"#
    );
}

#[test]
fn test_publication_state_preserves_the_result_without_transport_text() {
    use Publication as P;
    use PublicationState as S;
    for (publication, state) in [
        (P::NotPublishing, S::NotPublishing),
        (P::NothingAccepted, S::NothingAccepted),
        (P::Published, S::Published),
        (P::AwaitingRecord, S::AwaitingRecord),
        (P::Superseded, S::Superseded),
        (
            P::Owed {
                error: TransportError::Connect("private host".into()),
            },
            S::Owed {
                error: "connect".into(),
            },
        ),
        (
            P::Unchecked {
                error: TransportError::Timeout,
            },
            S::Unchecked {
                error: "timeout".into(),
            },
        ),
        (P::AwaitingRead, S::AwaitingRead),
    ] {
        assert_eq!(S::from(publication), state);
    }
}

#[test]
fn test_message_delivery_preserves_fields_without_failure_text() {
    use paykit_sdk::MessageDelivery as D;
    use MessageDelivery as M;
    for (delivery, expected) in [
        (
            D::Kept { batch_id: [4; 16] },
            M::Kept {
                batch_id: vec![4; 16],
            },
        ),
        (
            D::Queued {
                tries: 0,
                last_error: None,
            },
            M::Queued {
                tries: 0,
                last_error: None,
            },
        ),
        (
            D::Queued {
                tries: 2,
                last_error: Some("429 rate_limited: private details".into()),
            },
            M::Queued {
                tries: 2,
                last_error: Some("rate_limited".into()),
            },
        ),
        (
            D::Queued {
                tries: 3,
                last_error: Some("private details: 429 rate_limited".into()),
            },
            M::Queued {
                tries: 3,
                last_error: Some("unknown".into()),
            },
        ),
        (
            D::WaitingForAdmission {
                since_ms: 12,
                tries: 3,
            },
            M::WaitingForAdmission {
                since_ms: 12,
                tries: 3,
            },
        ),
        (
            D::CertificateNotCurrent {
                since_ms: 16,
                tries: 2,
            },
            M::CertificateNotCurrent {
                since_ms: 16,
                tries: 2,
            },
        ),
        (
            D::Accepted { accepted_at_ms: 17 },
            M::Accepted { accepted_at_ms: 17 },
        ),
        (
            D::Failed {
                reason: "403 permission_missing: private details".into(),
                failed_at_ms: 19,
            },
            M::Failed {
                reason: "permission_missing".into(),
                failed_at_ms: 19,
            },
        ),
        (
            D::Other(pubky_switch_client::app::Delivery::Accepted { accepted_at_ms: 20 }),
            M::Other,
        ),
    ] {
        assert_eq!(M::from(delivery), expected);
    }
}

#[test]
fn test_delivery_and_transport_codes_name_each_known_answer() {
    use paykit_sdk::MessageDelivery as D;
    // Each failure text the outbox keeps maps to a code; nothing of the text crosses.
    for (text, code) in [
        ("the batch carrying it was dropped", "batch_dropped"),
        ("not sealed", "not_sealed"),
        ("timed out", "timeout"),
        (
            "the batch carrying it was refused: 413 too_large: private details",
            "too_large",
        ),
        ("403 not_admitted", "not_admitted"),
        ("500 not_admitted", "unknown"),
    ] {
        let delivery = MessageDelivery::from(D::Queued {
            tries: 1,
            last_error: Some(text.into()),
        });
        assert_eq!(
            delivery,
            MessageDelivery::Queued {
                tries: 1,
                last_error: Some(code.into()),
            },
            "{text}"
        );
    }
    // Each transport failure of an owed publication is named by its kind alone.
    for (error, code) in [
        (
            TransportError::FirstSendUnavailable,
            "first_send_unavailable",
        ),
        (TransportError::FirstSendExpired, "first_send_expired"),
        (TransportError::Refused("private".into()), "refused"),
        (TransportError::Connect("private host".into()), "connect"),
        (TransportError::Timeout, "timeout"),
        (TransportError::TooLarge(9), "too_large"),
        (
            TransportError::InvalidRequest("private".into()),
            "invalid_request",
        ),
        (
            TransportError::UnreadableSignedHeader { status: 502 },
            "unreadable_signed_header",
        ),
        (TransportError::Other("private".into()), "other"),
    ] {
        assert_eq!(
            PublicationState::from(Publication::Owed { error }),
            PublicationState::Owed { error: code.into() }
        );
    }
}
